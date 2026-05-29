//! In-memory storage engine.
//!
//! This is the first storage backend: a simple set of named tables, each an
//! ordered `Vec` of rows. It is intentionally minimal but defines the
//! interface (`Database`, `Table`, `Column`) that a future disk-backed,
//! WAL-logged engine will implement.

use std::collections::{HashMap, HashSet};

use crate::index::{Bound, Index, IndexMethod, RowId};
use crate::sql::ast::{CommentObject, Expr, RoleOptions, Select, TablePersistence};
use crate::types::{DataType, Value};

/// A user-defined type created via `CREATE TYPE`.
#[derive(Debug, Clone, PartialEq)]
pub enum UserType {
    /// `CREATE TYPE ... AS ENUM (...)`: an ordered list of valid text labels.
    Enum { labels: Vec<String> },
    /// `CREATE TYPE ... AS (...)`: a composite type (text-backed, definition
    /// only — value semantics are not enforced).
    Composite { attributes: Vec<(String, DataType)> },
    /// `CREATE TYPE ... AS RANGE (...)`: a range type (text-backed, definition
    /// only).
    Range { subtype: DataType },
}

/// A user-defined domain created via `CREATE DOMAIN`.
#[derive(Debug, Clone, PartialEq)]
pub struct Domain {
    pub name: String,
    pub base: DataType,
    pub not_null: bool,
    /// `CHECK (VALUE ...)` predicate. `VALUE` is bound to the inserted value.
    pub check: Option<Expr>,
}

pub const DEFAULT_PAGE_SIZE: usize = 8192;

/// A table column: a name and its declared type, plus simple constraints.
#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub data_type: DataType,
    /// The declared user-defined type/domain name, lowercased, when the column
    /// was declared with an enum/domain/composite/range type. `None` for
    /// built-in types. Drives enum-label and domain constraint enforcement.
    pub type_name: Option<String>,
    pub not_null: bool,
    /// Whether this column is a PRIMARY KEY (enforced via a unique index).
    #[allow(dead_code)]
    pub primary_key: bool,
    /// `DEFAULT` expression applied when the column is omitted from an INSERT.
    pub default: Option<Expr>,
    /// Auto-incrementing (`serial`): values come from a sequence on insert.
    pub serial: bool,
    /// Auto-incrementing identity column.
    pub identity: bool,
    /// `GENERATED ALWAYS` identity mode.
    pub identity_always: bool,
    /// Stored generated expression.
    pub generated: Option<Expr>,
}

/// A stored table: schema, its rows, and any secondary indexes.
///
/// Rows live in `rows`. Each row also has a *stable* [`RowId`] in the parallel
/// `row_ids` vector (`row_ids[i]` is the id of `rows[i]`), and `row_pos` maps a
/// `RowId` back to its current position. Stable ids let B-tree indexes
/// reference rows that survive deletions of other rows, and the position map
/// turns an index hit (a set of `RowId`s) into row lookups in `O(1)` each.
#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    /// Stable id for each row, parallel to `rows`.
    row_ids: Vec<RowId>,
    /// Reverse map: `RowId` -> current index into `rows`/`row_ids`.
    row_pos: HashMap<RowId, usize>,
    /// Next id to hand out; monotonically increasing, never reused.
    next_row_id: RowId,
    /// Secondary indexes maintained incrementally on every mutation.
    indexes: Vec<Index>,
    unique_constraints: Vec<UniqueConstraint>,
    check_constraints: Vec<CheckConstraint>,
    foreign_key_constraints: Vec<ForeignKeyConstraint>,
    persistence: TablePersistence,
    /// Lightweight heap-page accounting. This is still in-memory storage, but
    /// gives future disk pages, FSM, VM, and vacuum logic a concrete boundary.
    storage_page_size: usize,
    storage_pages: Vec<StoragePage>,
    row_storage_pages: HashMap<RowId, usize>,
    row_storage_bytes: HashMap<RowId, usize>,
    vacuum_count: usize,
    compaction_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageMeta {
    pub page_id: usize,
    pub live_rows: usize,
    pub dead_rows: usize,
    pub live_bytes: usize,
    pub dead_bytes: usize,
    pub free_bytes: usize,
    pub all_visible: bool,
    pub all_frozen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeSpaceEntry {
    pub page_id: usize,
    pub free_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibilityMapEntry {
    pub page_id: usize,
    pub all_visible: bool,
    pub all_frozen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageStats {
    pub page_size: usize,
    pub page_count: usize,
    pub live_rows: usize,
    pub dead_rows: usize,
    pub live_bytes: usize,
    pub dead_bytes: usize,
    pub free_space_bytes: usize,
    pub all_visible_pages: usize,
    pub all_frozen_pages: usize,
    pub vacuum_count: usize,
    pub compaction_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VacuumStats {
    pub pages_before: usize,
    pub pages_after: usize,
    pub dead_rows_removed: usize,
    pub dead_bytes_removed: usize,
    pub pages_removed: usize,
}

impl VacuumStats {
    fn empty() -> Self {
        VacuumStats {
            pages_before: 0,
            pages_after: 0,
            dead_rows_removed: 0,
            dead_bytes_removed: 0,
            pages_removed: 0,
        }
    }

    fn absorb(&mut self, other: VacuumStats) {
        self.pages_before += other.pages_before;
        self.pages_after += other.pages_after;
        self.dead_rows_removed += other.dead_rows_removed;
        self.dead_bytes_removed += other.dead_bytes_removed;
        self.pages_removed += other.pages_removed;
    }
}

#[derive(Debug, Clone)]
struct StoragePage {
    id: usize,
    live_rows: usize,
    dead_rows: usize,
    live_bytes: usize,
    dead_bytes: usize,
}

impl StoragePage {
    fn new(id: usize) -> Self {
        StoragePage {
            id,
            live_rows: 0,
            dead_rows: 0,
            live_bytes: 0,
            dead_bytes: 0,
        }
    }

    fn used_bytes(&self) -> usize {
        self.live_bytes + self.dead_bytes
    }

    fn free_bytes(&self, page_size: usize) -> usize {
        page_size.saturating_sub(self.used_bytes())
    }

    fn can_fit(&self, bytes: usize, page_size: usize) -> bool {
        bytes <= self.free_bytes(page_size)
    }

    fn add_live(&mut self, bytes: usize) {
        self.live_rows += 1;
        self.live_bytes += bytes;
    }

    fn mark_dead(&mut self, bytes: usize) {
        self.live_rows = self.live_rows.saturating_sub(1);
        self.live_bytes = self.live_bytes.saturating_sub(bytes);
        self.dead_rows += 1;
        self.dead_bytes += bytes;
    }
}

#[derive(Debug, Clone)]
pub struct CheckConstraint {
    pub name: String,
    pub expr: Expr,
    pub validated: bool,
}

#[derive(Debug, Clone)]
pub struct ForeignKeyConstraint {
    pub name: String,
    pub column: usize,
    pub ref_table: String,
    pub ref_column: String,
    pub validated: bool,
}

#[derive(Debug, Clone)]
pub struct UniqueConstraint {
    pub name: String,
    pub columns: Vec<usize>,
    pub primary_key: bool,
}

impl Table {
    /// Create an empty table with the given schema and no indexes.
    pub fn new(name: String, columns: Vec<Column>) -> Self {
        Self::new_with_persistence(name, columns, TablePersistence::Permanent)
    }

    pub fn new_with_persistence(
        name: String,
        columns: Vec<Column>,
        persistence: TablePersistence,
    ) -> Self {
        Table {
            name,
            columns,
            rows: Vec::new(),
            row_ids: Vec::new(),
            row_pos: HashMap::new(),
            next_row_id: 0,
            indexes: Vec::new(),
            unique_constraints: Vec::new(),
            check_constraints: Vec::new(),
            foreign_key_constraints: Vec::new(),
            persistence,
            storage_page_size: DEFAULT_PAGE_SIZE,
            storage_pages: Vec::new(),
            row_storage_pages: HashMap::new(),
            row_storage_bytes: HashMap::new(),
            vacuum_count: 0,
            compaction_count: 0,
        }
    }

    /// Index of a column by name (case-sensitive, matching how it was created).
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    pub fn column_names(&self) -> Vec<String> {
        self.columns.iter().map(|c| c.name.clone()).collect()
    }

    // --- row + index maintenance ---------------------------------------------

    /// Compute the key values for `idx` from `row` (reading its columns, or
    /// evaluating its expression). Returns `None` if the expression fails to
    /// evaluate (the row is then simply left out of that index).
    fn index_key(&self, idx: &Index, row: &[Value]) -> Option<Vec<Value>> {
        if let Some(expr) = &idx.expr {
            let names = self.column_names();
            match crate::executor::eval_expr(expr, &names, row) {
                Ok(v) => Some(vec![v]),
                Err(_) => None,
            }
        } else {
            Some(idx.columns.iter().map(|&c| row[c].clone()).collect())
        }
    }

    /// Whether `row` satisfies `idx`'s partial-index predicate (always true for
    /// a non-partial index).
    fn row_qualifies(&self, idx: &Index, row: &[Value]) -> bool {
        match &idx.predicate {
            None => true,
            Some(pred) => {
                let names = self.column_names();
                crate::executor::eval_expr(pred, &names, row)
                    .map(|v| v.is_true())
                    .unwrap_or(false)
            }
        }
    }

    /// Insert `row` (id `id`) into every applicable index.
    fn index_row(&mut self, row: &[Value], id: RowId) {
        // Clone the index metadata we need so we can borrow `self` immutably
        // for key/predicate evaluation, then mutate the index.
        let mut to_insert: Vec<(usize, Vec<Value>)> = Vec::new();
        for (i, idx) in self.indexes.iter().enumerate() {
            if !self.row_qualifies(idx, row) {
                continue;
            }
            if let Some(key) = self.index_key(idx, row) {
                to_insert.push((i, key));
            }
        }
        for (i, key) in to_insert {
            self.indexes[i].insert(key, id);
        }
    }

    /// Remove `row` (id `id`) from every applicable index.
    fn unindex_row(&mut self, row: &[Value], id: RowId) {
        let mut to_remove: Vec<(usize, Vec<Value>)> = Vec::new();
        for (i, idx) in self.indexes.iter().enumerate() {
            if !self.row_qualifies(idx, row) {
                continue;
            }
            if let Some(key) = self.index_key(idx, row) {
                to_remove.push((i, key));
            }
        }
        for (i, key) in to_remove {
            self.indexes[i].remove(key, id);
        }
    }

    /// Append a row, assigning it a fresh stable id and updating all indexes.
    pub fn push_row(&mut self, row: Vec<Value>) {
        let id = self.next_row_id;
        self.next_row_id += 1;
        let pos = self.rows.len();
        let storage_bytes = row_storage_bytes(&row);
        self.index_row(&row, id);
        self.assign_live_storage(id, storage_bytes);
        self.rows.push(row);
        self.row_ids.push(id);
        self.row_pos.insert(id, pos);
    }

    /// Replace the row at position `pos` with `new_row`, keeping its id and
    /// repairing every index whose column changed.
    pub fn update_row(&mut self, pos: usize, new_row: Vec<Value>) {
        let id = self.row_ids[pos];
        // Re-derive index membership wholesale: a row can enter or leave a
        // partial index, or change an expression key, so removing the old key
        // and inserting the new one is both simplest and always correct.
        let old_row = self.rows[pos].clone();
        self.unindex_row(&old_row, id);
        self.index_row(&new_row, id);
        let storage_bytes = row_storage_bytes(&new_row);
        self.mark_dead_storage(id);
        self.assign_live_storage(id, storage_bytes);
        self.rows[pos] = new_row;
    }

    /// Delete the rows at the given positions (in any order), patching indexes
    /// and the position map. Implemented by rebuilding the kept rows, which is
    /// `O(n)` — acceptable since a DELETE already scans/filters the table.
    pub fn delete_rows(&mut self, positions: &[usize]) {
        if positions.is_empty() {
            return;
        }
        let mut drop_mask = vec![false; self.rows.len()];
        for &p in positions {
            drop_mask[p] = true;
        }
        // Remove deleted entries from each index first (we still have the old
        // values and ids in place).
        for (p, dropped) in drop_mask.iter().enumerate() {
            if *dropped {
                let row = self.rows[p].clone();
                let id = self.row_ids[p];
                self.unindex_row(&row, id);
            }
        }
        for (p, dropped) in drop_mask.iter().enumerate() {
            if *dropped {
                self.mark_dead_storage(self.row_ids[p]);
            }
        }
        // Compact the surviving rows/ids and rebuild the position map.
        let mut new_rows = Vec::with_capacity(self.rows.len() - positions.len());
        let mut new_ids = Vec::with_capacity(new_rows.capacity());
        let mut new_pos = HashMap::with_capacity(new_rows.capacity());
        for (p, row) in std::mem::take(&mut self.rows).into_iter().enumerate() {
            if drop_mask[p] {
                continue;
            }
            new_pos.insert(self.row_ids[p], new_rows.len());
            new_rows.push(row);
            new_ids.push(self.row_ids[p]);
        }
        self.rows = new_rows;
        self.row_ids = new_ids;
        self.row_pos = new_pos;
    }

    pub fn truncate(&mut self) {
        self.rows.clear();
        self.row_ids.clear();
        self.row_pos.clear();
        self.storage_pages.clear();
        self.row_storage_pages.clear();
        self.row_storage_bytes.clear();
        for idx in &mut self.indexes {
            idx.clear();
        }
    }

    /// Current position of a row id, if it still exists.
    pub fn position_of(&self, id: RowId) -> Option<usize> {
        self.row_pos.get(&id).copied()
    }

    /// Append a column, giving every existing row the supplied value.
    pub fn add_column(&mut self, column: Column, fill: &dyn Fn(usize) -> Value) {
        self.columns.push(column);
        for (pos, row) in self.rows.iter_mut().enumerate() {
            row.push(fill(pos));
        }
        self.rewrite_storage_pages();
    }

    /// Remove the column at `idx` from the schema and every row, dropping any
    /// index on it and shifting later indexes' column positions down. The
    /// surviving index trees stay valid since the column *values* don't change.
    pub fn drop_column(&mut self, idx: usize) {
        self.columns.remove(idx);
        for row in &mut self.rows {
            row.remove(idx);
        }
        // Drop any index that references the removed column (in its key or its
        // INCLUDE list), then shift later column positions down by one.
        self.indexes
            .retain(|i| !i.columns.contains(&idx) && !i.include.contains(&idx));
        for i in &mut self.indexes {
            for c in &mut i.columns {
                if *c > idx {
                    *c -= 1;
                }
            }
            for c in &mut i.include {
                if *c > idx {
                    *c -= 1;
                }
            }
        }
        self.unique_constraints
            .retain(|constraint| !constraint.columns.contains(&idx));
        for constraint in &mut self.unique_constraints {
            for column in &mut constraint.columns {
                if *column > idx {
                    *column -= 1;
                }
            }
        }
        self.rewrite_storage_pages();
    }

    // --- index management ----------------------------------------------------

    /// Find a single-column, non-partial, non-expression index over `column`,
    /// preferring a unique one when both exist. Used for the simple equality /
    /// range / join probe paths (which all assume a one-column key).
    pub fn index_on(&self, column: usize) -> Option<&Index> {
        let mut chosen: Option<&Index> = None;
        for idx in &self.indexes {
            if idx.columns == [column]
                && idx.expr.is_none()
                && idx.predicate.is_none()
                && idx.method == IndexMethod::Btree
            {
                match chosen {
                    Some(c) if c.unique => {}
                    _ => chosen = Some(idx),
                }
            }
        }
        chosen
    }

    /// Find an index usable for equality lookups on `column` as its single key
    /// column (B-tree or hash), non-partial and non-expression. Used by the
    /// equality / join paths which only need point lookups.
    pub fn eq_index_on(&self, column: usize) -> Option<&Index> {
        let mut chosen: Option<&Index> = None;
        for idx in &self.indexes {
            if idx.columns == [column] && idx.expr.is_none() && idx.predicate.is_none() {
                match chosen {
                    Some(c) if c.unique => {}
                    _ => chosen = Some(idx),
                }
            }
        }
        chosen
    }

    /// Whether an index with this name already exists.
    pub fn has_index_named(&self, name: &str) -> bool {
        self.indexes.iter().any(|i| i.name == name)
    }

    pub fn has_constraint_named(&self, name: &str) -> bool {
        self.has_index_named(name)
            || self.unique_constraints.iter().any(|c| c.name == name)
            || self.check_constraints.iter().any(|c| c.name == name)
            || self.foreign_key_constraints.iter().any(|c| c.name == name)
    }

    /// Indexes defined on this table, in creation order.
    pub fn indexes(&self) -> &[Index] {
        &self.indexes
    }

    pub fn unique_constraints(&self) -> &[UniqueConstraint] {
        &self.unique_constraints
    }

    pub fn check_constraints(&self) -> &[CheckConstraint] {
        &self.check_constraints
    }

    pub fn foreign_key_constraints(&self) -> &[ForeignKeyConstraint] {
        &self.foreign_key_constraints
    }

    pub fn persistence(&self) -> TablePersistence {
        self.persistence
    }

    /// If inserting/updating `row` would collide with an existing row on a
    /// unique index, return that index's name. `exclude` skips a position (the
    /// row being updated, so it doesn't conflict with itself). NULLs never
    /// conflict (SQL permits multiple NULLs in a unique index).
    pub fn unique_violation(&self, row: &[Value], exclude: Option<usize>) -> Option<String> {
        for (i, idx) in self.indexes.iter().enumerate() {
            if !idx.unique {
                continue;
            }
            // Unique indexes are single-column in this engine.
            let Some(col) = idx.leading_column() else {
                continue;
            };
            let value = &row[col];
            if value.is_null() {
                continue;
            }
            let positions = self.index_eq_multi(i, std::slice::from_ref(value));
            if positions.iter().any(|&p| Some(p) != exclude) {
                return Some(idx.name.clone());
            }
        }
        for constraint in &self.unique_constraints {
            if unique_key_is_null(row, &constraint.columns) {
                continue;
            }
            for (pos, existing) in self.rows.iter().enumerate() {
                if Some(pos) == exclude {
                    continue;
                }
                if same_unique_key(existing, row, &constraint.columns) {
                    return Some(constraint.name.clone());
                }
            }
        }
        None
    }

    /// Build and populate a new single-column B-tree index over `column`.
    pub fn create_index(&mut self, name: String, column: usize, unique: bool) {
        let idx = Index::new(name, column, unique);
        self.add_and_populate_index(idx);
    }

    /// Build and populate a fully-described index (multi-column, expression,
    /// partial, covering, and/or hash) from the current rows.
    #[allow(clippy::too_many_arguments)]
    pub fn create_index_full(
        &mut self,
        name: String,
        columns: Vec<usize>,
        expr: Option<Expr>,
        predicate: Option<Expr>,
        include: Vec<usize>,
        unique: bool,
        method: IndexMethod,
    ) {
        let mut idx = Index::new_multi(name, columns, unique, method);
        idx.expr = expr;
        idx.predicate = predicate;
        idx.include = include;
        self.add_and_populate_index(idx);
    }

    /// Push `idx` and populate it from the existing rows (honouring any partial
    /// predicate and expression key).
    fn add_and_populate_index(&mut self, idx: Index) {
        self.indexes.push(idx);
        let pos = self.indexes.len() - 1;
        let mut entries: Vec<(Vec<Value>, RowId)> = Vec::new();
        for (row, &id) in self.rows.iter().zip(&self.row_ids) {
            let idx_ref = &self.indexes[pos];
            if !self.row_qualifies(idx_ref, row) {
                continue;
            }
            if let Some(key) = self.index_key(idx_ref, row) {
                entries.push((key, id));
            }
        }
        for (key, id) in entries {
            self.indexes[pos].insert(key, id);
        }
    }

    pub fn column_has_duplicate_values(&self, column: usize) -> bool {
        let mut seen = HashSet::new();
        for row in &self.rows {
            if row[column].is_null() {
                continue;
            }
            if let Some(key) = row[column].to_text() {
                if !seen.insert(key) {
                    return true;
                }
            }
        }
        false
    }

    pub fn columns_have_duplicate_values(&self, columns: &[usize]) -> bool {
        let mut seen = HashSet::new();
        for row in &self.rows {
            if unique_key_is_null(row, columns) {
                continue;
            }
            let key = unique_key(row, columns);
            if !seen.insert(key) {
                return true;
            }
        }
        false
    }

    pub fn set_primary_key(&mut self, column: usize, primary_key: bool) {
        self.columns[column].primary_key = primary_key;
        if primary_key {
            self.columns[column].not_null = true;
        }
    }

    pub fn add_unique_constraint(&mut self, constraint: UniqueConstraint) {
        if constraint.primary_key {
            for &column in &constraint.columns {
                self.columns[column].primary_key = true;
                self.columns[column].not_null = true;
            }
        }
        self.unique_constraints.push(constraint);
    }

    pub fn drop_unique_constraint(&mut self, name: &str) -> bool {
        let before = self.unique_constraints.len();
        self.unique_constraints.retain(|c| c.name != name);
        self.unique_constraints.len() != before
    }

    pub fn add_check_constraint(&mut self, constraint: CheckConstraint) {
        self.check_constraints.push(constraint);
    }

    pub fn drop_check_constraint(&mut self, name: &str) -> bool {
        let before = self.check_constraints.len();
        self.check_constraints.retain(|c| c.name != name);
        self.check_constraints.len() != before
    }

    pub fn add_foreign_key_constraint(&mut self, constraint: ForeignKeyConstraint) {
        self.foreign_key_constraints.push(constraint);
    }

    pub fn drop_foreign_key_constraint(&mut self, name: &str) -> bool {
        let before = self.foreign_key_constraints.len();
        self.foreign_key_constraints.retain(|c| c.name != name);
        self.foreign_key_constraints.len() != before
    }

    /// Columns covered by a unique index (for batch duplicate checks).
    pub fn unique_key_columns(&self) -> Vec<Vec<usize>> {
        let mut columns: Vec<Vec<usize>> = self
            .indexes
            .iter()
            .filter(|i| i.unique)
            .map(|i| i.columns.clone())
            .collect();
        columns.extend(
            self.unique_constraints
                .iter()
                .map(|constraint| constraint.columns.clone()),
        );
        columns
    }

    /// Drop an index by name, returning whether it existed.
    pub fn drop_index(&mut self, name: &str) -> bool {
        self.remove_index(name).is_some()
    }

    pub fn remove_index(&mut self, name: &str) -> Option<Index> {
        let index = self.indexes.iter().position(|i| i.name == name)?;
        Some(self.indexes.remove(index))
    }

    // --- index-accelerated scans ---------------------------------------------

    /// Row positions whose `column` equals `value`, via an index if one exists.
    /// Positions are returned for the caller to read from `rows`.
    pub fn index_eq(&self, column: usize, value: &Value) -> Option<Vec<usize>> {
        let idx = self.eq_index_on(column)?;
        Some(self.ids_to_positions(&idx.lookup_eq(std::slice::from_ref(value))))
    }

    /// Row positions whose multi-column index `idx_pos` matches the full key
    /// `values` (one value per index column).
    pub fn index_eq_multi(&self, idx_pos: usize, values: &[Value]) -> Vec<usize> {
        self.ids_to_positions(&self.indexes[idx_pos].lookup_eq(values))
    }

    /// Row positions matching the leading prefix `values` of index `idx_pos`.
    pub fn index_prefix_multi(&self, idx_pos: usize, values: &[Value]) -> Vec<usize> {
        self.ids_to_positions(&self.indexes[idx_pos].lookup_prefix(values))
    }

    /// All row positions currently held by index `idx_pos` (used to scan a
    /// partial index whose predicate covers the query).
    pub fn index_all_positions(&self, idx_pos: usize) -> Vec<usize> {
        self.ids_to_positions(&self.indexes[idx_pos].all_row_ids())
    }

    /// Row positions matching `key` via expression index `idx_pos`.
    pub fn index_eq_expr(&self, idx_pos: usize, key: &Value) -> Vec<usize> {
        self.ids_to_positions(&self.indexes[idx_pos].lookup_eq(std::slice::from_ref(key)))
    }

    /// Row positions whose `column` falls in the given range, via an index.
    pub fn index_range(
        &self,
        column: usize,
        lo: Option<Bound>,
        hi: Option<Bound>,
    ) -> Option<Vec<usize>> {
        let idx = self.index_on(column)?;
        Some(self.ids_to_positions(&idx.lookup_range(lo, hi)))
    }

    pub fn page_layout(&self) -> Vec<PageMeta> {
        self.storage_pages
            .iter()
            .map(|page| PageMeta {
                page_id: page.id,
                live_rows: page.live_rows,
                dead_rows: page.dead_rows,
                live_bytes: page.live_bytes,
                dead_bytes: page.dead_bytes,
                free_bytes: page.free_bytes(self.storage_page_size),
                all_visible: page.dead_rows == 0,
                all_frozen: page.dead_rows == 0,
            })
            .collect()
    }

    pub fn free_space_map(&self) -> Vec<FreeSpaceEntry> {
        self.storage_pages
            .iter()
            .map(|page| FreeSpaceEntry {
                page_id: page.id,
                free_bytes: page.free_bytes(self.storage_page_size),
            })
            .collect()
    }

    pub fn visibility_map(&self) -> Vec<VisibilityMapEntry> {
        self.storage_pages
            .iter()
            .map(|page| VisibilityMapEntry {
                page_id: page.id,
                all_visible: page.dead_rows == 0,
                all_frozen: page.dead_rows == 0,
            })
            .collect()
    }

    pub fn storage_stats(&self) -> StorageStats {
        let page_layout = self.page_layout();
        StorageStats {
            page_size: self.storage_page_size,
            page_count: page_layout.len(),
            live_rows: page_layout.iter().map(|page| page.live_rows).sum(),
            dead_rows: page_layout.iter().map(|page| page.dead_rows).sum(),
            live_bytes: page_layout.iter().map(|page| page.live_bytes).sum(),
            dead_bytes: page_layout.iter().map(|page| page.dead_bytes).sum(),
            free_space_bytes: page_layout.iter().map(|page| page.free_bytes).sum(),
            all_visible_pages: page_layout.iter().filter(|page| page.all_visible).count(),
            all_frozen_pages: page_layout.iter().filter(|page| page.all_frozen).count(),
            vacuum_count: self.vacuum_count,
            compaction_count: self.compaction_count,
        }
    }

    pub fn vacuum_storage(&mut self) -> VacuumStats {
        let before = self.storage_stats();
        self.rebuild_live_storage();
        self.vacuum_count += 1;
        self.compaction_count += 1;
        let after = self.storage_stats();
        VacuumStats {
            pages_before: before.page_count,
            pages_after: after.page_count,
            dead_rows_removed: before.dead_rows,
            dead_bytes_removed: before.dead_bytes,
            pages_removed: before.page_count.saturating_sub(after.page_count),
        }
    }

    /// Translate a set of row ids into current row positions, dropping any that
    /// no longer exist (defensive; ids in an index should always be present).
    fn ids_to_positions(&self, ids: &[RowId]) -> Vec<usize> {
        ids.iter().filter_map(|&id| self.position_of(id)).collect()
    }

    fn assign_live_storage(&mut self, id: RowId, bytes: usize) {
        let page_id = self.page_for(bytes);
        self.storage_pages[page_id].add_live(bytes);
        self.row_storage_pages.insert(id, page_id);
        self.row_storage_bytes.insert(id, bytes);
    }

    fn mark_dead_storage(&mut self, id: RowId) {
        let Some(page_id) = self.row_storage_pages.remove(&id) else {
            return;
        };
        let bytes = self
            .row_storage_bytes
            .remove(&id)
            .unwrap_or_else(|| row_storage_bytes(&self.rows[self.row_pos[&id]]));
        if let Some(page) = self.storage_pages.get_mut(page_id) {
            page.mark_dead(bytes);
        }
    }

    fn page_for(&mut self, bytes: usize) -> usize {
        if let Some(page) = self
            .storage_pages
            .iter()
            .find(|page| page.can_fit(bytes, self.storage_page_size))
        {
            return page.id;
        }
        let id = self.storage_pages.len();
        self.storage_pages.push(StoragePage::new(id));
        id
    }

    fn rewrite_storage_pages(&mut self) {
        self.compaction_count += 1;
        self.rebuild_live_storage();
    }

    fn rebuild_live_storage(&mut self) {
        self.storage_pages.clear();
        self.row_storage_pages.clear();
        self.row_storage_bytes.clear();
        for (pos, row) in self.rows.clone().into_iter().enumerate() {
            let id = self.row_ids[pos];
            self.assign_live_storage(id, row_storage_bytes(&row));
        }
    }
}

fn row_storage_bytes(row: &[Value]) -> usize {
    const ROW_HEADER_BYTES: usize = 24;
    let null_bitmap_bytes = row.len().div_ceil(8);
    align8(
        ROW_HEADER_BYTES + null_bitmap_bytes + row.iter().map(value_storage_bytes).sum::<usize>(),
    )
}

fn value_storage_bytes(value: &Value) -> usize {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int(_) | Value::Float(_) => 8,
        Value::Text(s) => 4 + s.len(),
    }
}

fn align8(bytes: usize) -> usize {
    (bytes + 7) & !7
}

fn unique_key_is_null(row: &[Value], columns: &[usize]) -> bool {
    columns.iter().any(|&column| row[column].is_null())
}

fn unique_key(row: &[Value], columns: &[usize]) -> Vec<String> {
    columns
        .iter()
        .map(|&column| row[column].to_text().unwrap_or_default())
        .collect()
}

fn same_unique_key(left: &[Value], right: &[Value], columns: &[usize]) -> bool {
    columns
        .iter()
        .all(|&column| left[column].to_text() == right[column].to_text())
}

/// The whole database: a flat namespace of tables.
///
/// `Clone` is used to snapshot the database when a transaction begins, so a
/// `ROLLBACK` can restore the prior state.
#[derive(Debug, Clone)]
pub struct Database {
    tables: HashMap<String, Table>,
    views: HashMap<String, View>,
    materialized_views: HashMap<String, MaterializedView>,
    cursors: HashMap<String, Cursor>,
    schemas: HashSet<String>,
    search_path: Vec<String>,
    databases: HashMap<String, DatabaseInfo>,
    tablespaces: HashMap<String, Tablespace>,
    collations: HashMap<String, Collation>,
    /// User-defined types (`CREATE TYPE`), keyed by lowercased name.
    user_types: HashMap<String, UserType>,
    /// Domains (`CREATE DOMAIN`), keyed by lowercased name.
    domains: HashMap<String, Domain>,
    extensions: HashMap<String, Extension>,
    roles: HashMap<String, Role>,
    comments: HashMap<CommentObject, String>,
    security_labels: HashMap<(String, CommentObject), String>,
    system_settings: HashMap<String, String>,
    /// Sequence state for explicit sequences and `serial` columns, keyed by name.
    sequences: HashMap<String, Sequence>,
    advisory_locks: HashSet<(i64, i64)>,
    /// Table privileges granted to roles: `(table, grantee, privilege)`.
    /// `grantee` is a role name or `PUBLIC`. Acceptance/introspection only —
    /// there is no runtime enforcement.
    table_privileges: HashSet<(String, String, String)>,
    /// Role memberships: `(member_role, group_role)` meaning `member` is a
    /// member of `group`.
    role_memberships: HashSet<(String, String)>,
    /// User-defined functions (`CREATE FUNCTION`), keyed by lowercased name.
    /// A name may map to several overloads distinguished by argument signature.
    functions: HashMap<String, Vec<SqlFunction>>,
    /// Triggers, keyed by lowercased trigger name. Each trigger names the table
    /// it is attached to and the function it executes.
    triggers: HashMap<String, Trigger>,
    /// Rules (`CREATE RULE`), keyed by lowercased name. Accepted and stored but
    /// never applied (no query rewriting).
    rules: HashMap<String, Rule>,
    /// User-defined aggregates (`CREATE AGGREGATE`), keyed by lowercased name.
    /// Accepted and stored but never used during aggregation.
    aggregates: HashMap<String, Aggregate>,
}

impl Default for Database {
    fn default() -> Self {
        let schemas = ["public", "pg_catalog", "information_schema"]
            .into_iter()
            .map(String::from)
            .collect();
        let mut extensions = HashMap::new();
        extensions.insert(
            "plpgsql".into(),
            Extension {
                name: "plpgsql".into(),
                version: "1.0".into(),
            },
        );
        let mut roles = HashMap::new();
        roles.insert("postgres".into(), Role::postgres());
        Database {
            tables: HashMap::new(),
            views: HashMap::new(),
            materialized_views: HashMap::new(),
            cursors: HashMap::new(),
            schemas,
            search_path: vec!["$user".into(), "public".into()],
            databases: HashMap::from([("postgres".into(), DatabaseInfo::postgres())]),
            tablespaces: HashMap::from([
                ("pg_default".into(), Tablespace::pg_default()),
                ("pg_global".into(), Tablespace::pg_global()),
            ]),
            collations: HashMap::from([
                ("default".into(), Collation::default_collation()),
                ("C".into(), Collation::named(950, "C".into(), "C".into())),
                (
                    "POSIX".into(),
                    Collation::named(951, "POSIX".into(), "POSIX".into()),
                ),
            ]),
            user_types: HashMap::new(),
            domains: HashMap::new(),
            extensions,
            roles,
            comments: HashMap::new(),
            security_labels: HashMap::new(),
            system_settings: HashMap::new(),
            sequences: HashMap::new(),
            advisory_locks: HashSet::new(),
            table_privileges: HashSet::new(),
            role_memberships: HashSet::new(),
            functions: HashMap::new(),
            triggers: HashMap::new(),
            rules: HashMap::new(),
            aggregates: HashMap::new(),
        }
    }
}

/// A stored user-defined function. Only `LANGUAGE sql` bodies are interpreted;
/// other languages are accepted and catalogued but not callable.
#[derive(Debug, Clone)]
pub struct SqlFunction {
    pub name: String,
    /// Argument names (in order); `None` for an unnamed argument.
    pub arg_names: Vec<Option<String>>,
    /// Resolved argument data types, parallel to `arg_names`.
    pub arg_types: Vec<DataType>,
    /// The lowercased written argument type names, used as the overload key.
    pub arg_type_names: Vec<String>,
    pub return_type: Option<DataType>,
    pub return_type_name: Option<String>,
    pub body: String,
    pub language: String,
}

/// A stored trigger.
#[derive(Debug, Clone)]
pub struct Trigger {
    pub name: String,
    /// `true` = BEFORE, `false` = AFTER.
    pub before: bool,
    /// The events this trigger fires on, as lowercased strings
    /// (`"insert"`, `"update"`, `"delete"`).
    pub events: Vec<String>,
    pub table: String,
    pub for_each_row: bool,
    pub function: String,
}

/// A stored rule (accept-and-store; never applied).
#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub event: String,
    pub table: String,
    pub definition: String,
}

/// A stored user-defined aggregate (accept-and-store; never applied).
#[derive(Debug, Clone)]
pub struct Aggregate {
    pub name: String,
    pub arg_types: Vec<String>,
    pub options: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvisoryLock {
    pub classid: i64,
    pub objid: i64,
}

#[derive(Debug, Clone)]
pub struct Extension {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct Tablespace {
    pub oid: i64,
    pub name: String,
    pub owner: i64,
    pub location: String,
}

#[derive(Debug, Clone)]
pub struct Collation {
    pub oid: i64,
    pub name: String,
    pub namespace: i64,
    pub owner: i64,
    pub provider: String,
    pub deterministic: bool,
    pub encoding: i64,
    pub collate: String,
    pub ctype: String,
}

impl Collation {
    fn default_collation() -> Self {
        Collation::named(100, "default".into(), "C".into())
    }

    fn named(oid: i64, name: String, locale: String) -> Self {
        Collation {
            oid,
            name,
            namespace: 11,
            owner: 10,
            provider: "c".into(),
            deterministic: true,
            encoding: -1,
            collate: locale.clone(),
            ctype: locale,
        }
    }
}

impl Tablespace {
    fn pg_default() -> Self {
        Tablespace {
            oid: 1663,
            name: "pg_default".into(),
            owner: 10,
            location: String::new(),
        }
    }

    fn pg_global() -> Self {
        Tablespace {
            oid: 1664,
            name: "pg_global".into(),
            owner: 10,
            location: String::new(),
        }
    }

    fn new(oid: i64, name: String, location: String) -> Self {
        Tablespace {
            oid,
            name,
            owner: 10,
            location,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Sequence {
    pub name: String,
    pub start: i64,
    pub increment: i64,
    pub last_value: i64,
    pub called: bool,
}

impl Sequence {
    fn new(name: String, start: i64, increment: i64) -> Self {
        Sequence {
            name,
            start,
            increment,
            last_value: start,
            called: false,
        }
    }

    fn next_value(&mut self) -> i64 {
        if self.called {
            self.last_value += self.increment;
        } else {
            self.called = true;
        }
        self.last_value
    }

    fn restart(&mut self, value: i64) {
        self.start = value;
        self.last_value = value;
        self.called = false;
    }
}

#[derive(Debug, Clone)]
pub struct Role {
    pub oid: i64,
    pub name: String,
    pub superuser: bool,
    pub inherit: bool,
    pub create_role: bool,
    pub create_db: bool,
    pub login: bool,
    pub replication: bool,
    pub connection_limit: i64,
    pub password: Option<String>,
    pub valid_until: Option<String>,
    pub bypass_rls: bool,
}

#[derive(Debug, Clone)]
pub struct DatabaseInfo {
    pub oid: i64,
    pub name: String,
    pub owner: i64,
    pub encoding: i64,
    pub is_template: bool,
    pub allow_connections: bool,
    pub connection_limit: i64,
    pub collate: String,
    pub ctype: String,
}

impl DatabaseInfo {
    fn postgres() -> Self {
        DatabaseInfo {
            oid: 5,
            name: "postgres".into(),
            owner: 10,
            encoding: 6,
            is_template: false,
            allow_connections: true,
            connection_limit: -1,
            collate: "C".into(),
            ctype: "C".into(),
        }
    }

    fn new(oid: i64, name: String) -> Self {
        DatabaseInfo {
            oid,
            name,
            owner: 10,
            encoding: 6,
            is_template: false,
            allow_connections: true,
            connection_limit: -1,
            collate: "C".into(),
            ctype: "C".into(),
        }
    }
}

impl Role {
    fn postgres() -> Self {
        Role {
            oid: 10,
            name: "postgres".into(),
            superuser: true,
            inherit: true,
            create_role: true,
            create_db: true,
            login: true,
            replication: false,
            connection_limit: -1,
            password: None,
            valid_until: None,
            bypass_rls: true,
        }
    }

    fn new(oid: i64, name: String, login: bool) -> Self {
        Role {
            oid,
            name,
            superuser: false,
            inherit: true,
            create_role: false,
            create_db: false,
            login,
            replication: false,
            connection_limit: -1,
            password: None,
            valid_until: None,
            bypass_rls: false,
        }
    }

    fn apply_options(&mut self, options: RoleOptions) {
        if let Some(value) = options.superuser {
            self.superuser = value;
        }
        if let Some(value) = options.inherit {
            self.inherit = value;
        }
        if let Some(value) = options.create_role {
            self.create_role = value;
        }
        if let Some(value) = options.create_db {
            self.create_db = value;
        }
        if let Some(value) = options.login {
            self.login = value;
        }
        if let Some(value) = options.replication {
            self.replication = value;
        }
        if let Some(value) = options.bypass_rls {
            self.bypass_rls = value;
        }
        if let Some(value) = options.connection_limit {
            self.connection_limit = value;
        }
        if let Some(value) = options.password {
            self.password = value;
        }
        if let Some(value) = options.valid_until {
            self.valid_until = value;
        }
    }
}

#[derive(Debug, Clone)]
pub struct View {
    pub name: String,
    pub select: Select,
    pub fields: Vec<(String, DataType)>,
}

#[derive(Debug, Clone)]
pub struct MaterializedView {
    pub name: String,
    pub select: Select,
    pub fields: Vec<(String, DataType)>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Debug, Clone)]
pub struct Cursor {
    pub fields: Vec<(String, DataType)>,
    pub rows: Vec<Vec<Value>>,
    pub position: usize,
}

impl Database {
    pub fn new() -> Self {
        Database::default()
    }

    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(name)
    }

    pub fn table_mut(&mut self, name: &str) -> Option<&mut Table> {
        self.tables.get_mut(name)
    }

    pub fn contains_table(&self, name: &str) -> bool {
        self.tables.contains_key(name)
    }

    pub fn view(&self, name: &str) -> Option<&View> {
        self.views.get(name)
    }

    pub fn materialized_view(&self, name: &str) -> Option<&MaterializedView> {
        self.materialized_views.get(name)
    }

    pub fn cursor_fields(&self, name: &str) -> Option<Vec<(String, DataType)>> {
        self.cursors.get(name).map(|cursor| cursor.fields.clone())
    }

    pub fn declare_cursor(
        &mut self,
        name: String,
        fields: Vec<(String, DataType)>,
        rows: Vec<Vec<Value>>,
    ) -> Result<(), String> {
        if self.cursors.contains_key(&name) {
            return Err(format!("cursor \"{name}\" already exists"));
        }
        self.cursors.insert(
            name,
            Cursor {
                fields,
                rows,
                position: 0,
            },
        );
        Ok(())
    }

    pub fn fetch_cursor(
        &mut self,
        name: &str,
        count: Option<usize>,
    ) -> Result<(Vec<(String, DataType)>, Vec<Vec<Value>>), String> {
        let cursor = self
            .cursors
            .get_mut(name)
            .ok_or_else(|| format!("cursor \"{name}\" does not exist"))?;
        let remaining = cursor.rows.len().saturating_sub(cursor.position);
        let take = count.unwrap_or(remaining).min(remaining);
        let start = cursor.position;
        let end = start + take;
        cursor.position = end;
        Ok((cursor.fields.clone(), cursor.rows[start..end].to_vec()))
    }

    /// Create a table. Errors if it already exists (caller handles
    /// `IF NOT EXISTS` before calling).
    pub fn create_table(&mut self, table: Table) -> Result<(), String> {
        if self.tables.contains_key(&table.name)
            || self.views.contains_key(&table.name)
            || self.materialized_views.contains_key(&table.name)
        {
            return Err(format!("relation \"{}\" already exists", table.name));
        }
        self.tables.insert(table.name.clone(), table);
        Ok(())
    }

    /// Drop a table, returning whether it existed.
    pub fn drop_table(&mut self, name: &str) -> bool {
        let existed = self.tables.remove(name).is_some();
        if existed {
            self.drop_relation_comments(name);
        }
        existed
    }

    pub fn create_view(&mut self, view: View, or_replace: bool) -> Result<(), String> {
        if self.tables.contains_key(&view.name) || self.materialized_views.contains_key(&view.name)
        {
            return Err(format!("relation \"{}\" already exists", view.name));
        }
        if self.views.contains_key(&view.name) && !or_replace {
            return Err(format!("relation \"{}\" already exists", view.name));
        }
        self.views.insert(view.name.clone(), view);
        Ok(())
    }

    pub fn drop_view(&mut self, name: &str) -> bool {
        let existed = self.views.remove(name).is_some();
        if existed {
            self.drop_relation_comments(name);
        }
        existed
    }

    pub fn view_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.views.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn create_materialized_view(
        &mut self,
        view: MaterializedView,
        if_not_exists: bool,
    ) -> Result<bool, String> {
        if self.tables.contains_key(&view.name) || self.views.contains_key(&view.name) {
            return Err(format!("relation \"{}\" already exists", view.name));
        }
        if self.materialized_views.contains_key(&view.name) {
            if if_not_exists {
                return Ok(false);
            }
            return Err(format!("relation \"{}\" already exists", view.name));
        }
        self.materialized_views.insert(view.name.clone(), view);
        Ok(true)
    }

    pub fn replace_materialized_view_rows(
        &mut self,
        name: &str,
        rows: Vec<Vec<Value>>,
    ) -> Result<(), String> {
        let view = self
            .materialized_views
            .get_mut(name)
            .ok_or_else(|| format!("materialized view \"{name}\" does not exist"))?;
        view.rows = rows;
        Ok(())
    }

    pub fn drop_materialized_view(&mut self, name: &str) -> bool {
        let existed = self.materialized_views.remove(name).is_some();
        if existed {
            self.drop_relation_comments(name);
        }
        existed
    }

    pub fn set_comment(&mut self, object: CommentObject, comment: Option<String>) {
        if let Some(comment) = comment {
            self.comments.insert(object, comment);
        } else {
            self.comments.remove(&object);
        }
    }

    pub fn comments(&self) -> Vec<(CommentObject, String)> {
        self.comments
            .iter()
            .map(|(object, comment)| (object.clone(), comment.clone()))
            .collect()
    }

    pub fn set_security_label(
        &mut self,
        provider: String,
        object: CommentObject,
        label: Option<String>,
    ) {
        let key = (provider, object);
        if let Some(label) = label {
            self.security_labels.insert(key, label);
        } else {
            self.security_labels.remove(&key);
        }
    }

    pub fn security_labels(&self) -> Vec<(String, CommentObject, String)> {
        self.security_labels
            .iter()
            .map(|((provider, object), label)| (provider.clone(), object.clone(), label.clone()))
            .collect()
    }

    pub fn set_system_setting(&mut self, name: String, value: String) {
        self.system_settings
            .insert(name.to_ascii_lowercase(), value);
    }

    pub fn reset_system_setting(&mut self, name: Option<&str>) {
        if let Some(name) = name {
            self.system_settings.remove(&name.to_ascii_lowercase());
        } else {
            self.system_settings.clear();
        }
    }

    pub fn system_setting(&self, name: &str) -> Option<&String> {
        self.system_settings.get(&name.to_ascii_lowercase())
    }

    pub fn system_settings(&self) -> Vec<(String, String)> {
        let mut settings: Vec<(String, String)> = self
            .system_settings
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        settings.sort_by(|a, b| a.0.cmp(&b.0));
        settings
    }

    fn drop_relation_comments(&mut self, name: &str) {
        self.comments.retain(|object, _| match object {
            CommentObject::Relation { name: relation } => relation != name,
            CommentObject::Column { table, .. } => table != name,
        });
        self.security_labels.retain(|(_, object), _| match object {
            CommentObject::Relation { name: relation } => relation != name,
            CommentObject::Column { table, .. } => table != name,
        });
    }

    pub fn materialized_view_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.materialized_views.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn schemas(&self) -> Vec<String> {
        let mut schemas: Vec<String> = self.schemas.iter().cloned().collect();
        schemas.sort();
        schemas
    }

    pub fn create_schema(&mut self, name: String, if_not_exists: bool) -> Result<bool, String> {
        if self.schemas.contains(&name) {
            if if_not_exists {
                return Ok(false);
            }
            return Err(format!("schema \"{name}\" already exists"));
        }
        self.schemas.insert(name);
        Ok(true)
    }

    pub fn drop_schema(&mut self, name: &str, if_exists: bool) -> Result<bool, String> {
        if matches!(name, "public" | "pg_catalog" | "information_schema") {
            return Err(format!("cannot drop schema \"{name}\""));
        }
        if self.schemas.remove(name) {
            self.search_path.retain(|entry| entry != name);
            return Ok(true);
        }
        if if_exists {
            Ok(false)
        } else {
            Err(format!("schema \"{name}\" does not exist"))
        }
    }

    pub fn set_search_path(&mut self, value: &str) {
        let paths: Vec<String> = value
            .split(',')
            .map(|part| part.trim().trim_matches('"').trim_matches('\''))
            .filter(|part| !part.is_empty())
            .map(String::from)
            .collect();
        if !paths.is_empty() {
            self.search_path = paths;
        }
    }

    pub fn search_path(&self) -> String {
        self.search_path.join(", ")
    }

    pub fn current_schema(&self) -> String {
        self.search_path
            .iter()
            .find(|name| name.as_str() != "$user" && self.schemas.contains(*name))
            .cloned()
            .unwrap_or_else(|| "public".into())
    }

    pub fn database_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.databases.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn databases(&self) -> Vec<DatabaseInfo> {
        let mut databases: Vec<DatabaseInfo> = self.databases.values().cloned().collect();
        databases.sort_by(|a, b| a.oid.cmp(&b.oid));
        databases
    }

    pub fn create_database(&mut self, name: String) -> Result<(), String> {
        if self.databases.contains_key(&name) {
            return Err(format!("database \"{name}\" already exists"));
        }
        let oid = self.databases.values().map(|db| db.oid).max().unwrap_or(4) + 1;
        self.databases
            .insert(name.clone(), DatabaseInfo::new(oid, name));
        Ok(())
    }

    pub fn alter_database_rename(&mut self, name: &str, to: String) -> Result<(), String> {
        if name == "postgres" {
            return Err("cannot rename the current database".into());
        }
        if self.databases.contains_key(&to) {
            return Err(format!("database \"{to}\" already exists"));
        }
        let mut database = self
            .databases
            .remove(name)
            .ok_or_else(|| format!("database \"{name}\" does not exist"))?;
        database.name = to.clone();
        self.databases.insert(to, database);
        Ok(())
    }

    pub fn alter_database_connection_limit(
        &mut self,
        name: &str,
        limit: i64,
    ) -> Result<(), String> {
        let database = self
            .databases
            .get_mut(name)
            .ok_or_else(|| format!("database \"{name}\" does not exist"))?;
        database.connection_limit = limit;
        Ok(())
    }

    pub fn drop_database(&mut self, name: &str, if_exists: bool) -> Result<bool, String> {
        if name == "postgres" {
            return Err("cannot drop the current database".into());
        }
        if self.databases.remove(name).is_some() {
            return Ok(true);
        }
        if if_exists {
            Ok(false)
        } else {
            Err(format!("database \"{name}\" does not exist"))
        }
    }

    pub fn tablespaces(&self) -> Vec<Tablespace> {
        let mut tablespaces: Vec<Tablespace> = self.tablespaces.values().cloned().collect();
        tablespaces.sort_by(|a, b| a.oid.cmp(&b.oid));
        tablespaces
    }

    pub fn create_tablespace(&mut self, name: String, location: String) -> Result<(), String> {
        if self.tablespaces.contains_key(&name) {
            return Err(format!("tablespace \"{name}\" already exists"));
        }
        let oid = self
            .tablespaces
            .values()
            .map(|ts| ts.oid)
            .max()
            .unwrap_or(1664)
            + 1;
        self.tablespaces
            .insert(name.clone(), Tablespace::new(oid, name, location));
        Ok(())
    }

    pub fn drop_tablespace(&mut self, name: &str, if_exists: bool) -> Result<bool, String> {
        if matches!(name, "pg_default" | "pg_global") {
            return Err(format!("cannot drop tablespace \"{name}\""));
        }
        if self.tablespaces.remove(name).is_some() {
            return Ok(true);
        }
        if if_exists {
            Ok(false)
        } else {
            Err(format!("tablespace \"{name}\" does not exist"))
        }
    }

    pub fn collations(&self) -> Vec<Collation> {
        let mut collations: Vec<Collation> = self.collations.values().cloned().collect();
        collations.sort_by(|a, b| a.oid.cmp(&b.oid));
        collations
    }

    pub fn create_collation(
        &mut self,
        name: String,
        if_not_exists: bool,
        locale: String,
    ) -> Result<bool, String> {
        if self.collations.contains_key(&name) {
            if if_not_exists {
                return Ok(false);
            }
            return Err(format!("collation \"{name}\" already exists"));
        }
        let oid = self.collations.values().map(|c| c.oid).max().unwrap_or(951) + 1;
        self.collations
            .insert(name.clone(), Collation::named(oid, name, locale));
        Ok(true)
    }

    pub fn drop_collation(&mut self, name: &str, if_exists: bool) -> Result<bool, String> {
        if matches!(name, "default" | "C" | "POSIX") {
            return Err(format!("cannot drop collation \"{name}\""));
        }
        if self.collations.remove(name).is_some() {
            return Ok(true);
        }
        if if_exists {
            Ok(false)
        } else {
            Err(format!("collation \"{name}\" does not exist"))
        }
    }

    pub fn user_type(&self, name: &str) -> Option<&UserType> {
        self.user_types.get(&name.to_ascii_lowercase())
    }

    pub fn domain(&self, name: &str) -> Option<&Domain> {
        self.domains.get(&name.to_ascii_lowercase())
    }

    pub fn create_user_type(&mut self, name: String, ty: UserType) -> Result<(), String> {
        let key = name.to_ascii_lowercase();
        if self.user_types.contains_key(&key) || self.domains.contains_key(&key) {
            return Err(format!("type \"{name}\" already exists"));
        }
        self.user_types.insert(key, ty);
        Ok(())
    }

    pub fn drop_user_type(&mut self, name: &str, if_exists: bool) -> Result<bool, String> {
        let key = name.to_ascii_lowercase();
        if self.user_types.remove(&key).is_some() {
            Ok(true)
        } else if if_exists {
            Ok(false)
        } else {
            Err(format!("type \"{name}\" does not exist"))
        }
    }

    pub fn create_domain(&mut self, domain: Domain) -> Result<(), String> {
        let key = domain.name.to_ascii_lowercase();
        if self.user_types.contains_key(&key) || self.domains.contains_key(&key) {
            return Err(format!("type \"{}\" already exists", domain.name));
        }
        self.domains.insert(key, domain);
        Ok(())
    }

    pub fn drop_domain(&mut self, name: &str, if_exists: bool) -> Result<bool, String> {
        let key = name.to_ascii_lowercase();
        if self.domains.remove(&key).is_some() {
            Ok(true)
        } else if if_exists {
            Ok(false)
        } else {
            Err(format!("type \"{name}\" does not exist"))
        }
    }

    // --- functions -----------------------------------------------------------

    /// All overloads registered under `name` (lowercased lookup).
    pub fn functions(&self, name: &str) -> Option<&[SqlFunction]> {
        self.functions
            .get(&name.to_ascii_lowercase())
            .map(|v| v.as_slice())
    }

    /// Look up a function by name and argument arity (the common dispatch path
    /// for a call site, where we know how many arguments were supplied).
    pub fn function_by_arity(&self, name: &str, arity: usize) -> Option<&SqlFunction> {
        self.functions
            .get(&name.to_ascii_lowercase())?
            .iter()
            .find(|f| f.arg_types.len() == arity)
    }

    /// All functions, flattened and sorted by name (for catalog introspection).
    pub fn all_functions(&self) -> Vec<SqlFunction> {
        let mut out: Vec<SqlFunction> =
            self.functions.values().flat_map(|v| v.iter().cloned()).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Register a function. With `or_replace`, an existing overload with the
    /// same argument-type signature is replaced; otherwise it errors.
    pub fn create_function(
        &mut self,
        func: SqlFunction,
        or_replace: bool,
    ) -> Result<(), String> {
        let key = func.name.to_ascii_lowercase();
        let overloads = self.functions.entry(key).or_default();
        if let Some(slot) = overloads
            .iter_mut()
            .find(|f| f.arg_type_names == func.arg_type_names)
        {
            if or_replace {
                *slot = func;
                return Ok(());
            }
            return Err(format!(
                "function \"{}\" already exists with same argument types",
                func.name
            ));
        }
        overloads.push(func);
        Ok(())
    }

    /// Drop a function. When `arg_types` is `Some`, the overload with that exact
    /// signature is removed; when `None`, the name must be unambiguous.
    pub fn drop_function(
        &mut self,
        name: &str,
        arg_types: Option<&[String]>,
        if_exists: bool,
    ) -> Result<bool, String> {
        let key = name.to_ascii_lowercase();
        let Some(overloads) = self.functions.get_mut(&key) else {
            if if_exists {
                return Ok(false);
            }
            return Err(format!("function {name} does not exist"));
        };
        match arg_types {
            Some(sig) => {
                let before = overloads.len();
                overloads.retain(|f| f.arg_type_names != sig);
                if overloads.len() == before {
                    if if_exists {
                        return Ok(false);
                    }
                    return Err(format!("function {name}(...) does not exist"));
                }
            }
            None => {
                if overloads.len() > 1 {
                    return Err(format!(
                        "function name \"{name}\" is not unique; specify the argument list"
                    ));
                }
                overloads.clear();
            }
        }
        if overloads.is_empty() {
            self.functions.remove(&key);
        }
        Ok(true)
    }

    // --- triggers ------------------------------------------------------------

    /// Triggers attached to `table` that fire for `event` (`"insert"` etc.),
    /// matching `before`. Returned in deterministic (name) order.
    pub fn triggers_for(&self, table: &str, event: &str, before: bool) -> Vec<Trigger> {
        let mut out: Vec<Trigger> = self
            .triggers
            .values()
            .filter(|t| {
                t.table.eq_ignore_ascii_case(table)
                    && t.before == before
                    && t.for_each_row
                    && t.events.iter().any(|e| e == event)
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn all_triggers(&self) -> Vec<Trigger> {
        let mut out: Vec<Trigger> = self.triggers.values().cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn create_trigger(&mut self, trigger: Trigger) -> Result<(), String> {
        let key = trigger.name.to_ascii_lowercase();
        if self.triggers.contains_key(&key) {
            return Err(format!(
                "trigger \"{}\" for relation \"{}\" already exists",
                trigger.name, trigger.table
            ));
        }
        self.triggers.insert(key, trigger);
        Ok(())
    }

    pub fn drop_trigger(
        &mut self,
        name: &str,
        table: &str,
        if_exists: bool,
    ) -> Result<bool, String> {
        let key = name.to_ascii_lowercase();
        match self.triggers.get(&key) {
            Some(t) if t.table.eq_ignore_ascii_case(table) => {
                self.triggers.remove(&key);
                Ok(true)
            }
            _ if if_exists => Ok(false),
            _ => Err(format!(
                "trigger \"{name}\" for table \"{table}\" does not exist"
            )),
        }
    }

    // --- rules ---------------------------------------------------------------

    pub fn all_rules(&self) -> Vec<Rule> {
        let mut out: Vec<Rule> = self.rules.values().cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn create_rule(&mut self, rule: Rule, or_replace: bool) -> Result<(), String> {
        let key = rule.name.to_ascii_lowercase();
        if self.rules.contains_key(&key) && !or_replace {
            return Err(format!("rule \"{}\" already exists", rule.name));
        }
        self.rules.insert(key, rule);
        Ok(())
    }

    pub fn drop_rule(
        &mut self,
        name: &str,
        table: &str,
        if_exists: bool,
    ) -> Result<bool, String> {
        let key = name.to_ascii_lowercase();
        match self.rules.get(&key) {
            Some(r) if r.table.eq_ignore_ascii_case(table) => {
                self.rules.remove(&key);
                Ok(true)
            }
            _ if if_exists => Ok(false),
            _ => Err(format!("rule \"{name}\" for relation \"{table}\" does not exist")),
        }
    }

    // --- aggregates ----------------------------------------------------------

    pub fn all_aggregates(&self) -> Vec<Aggregate> {
        let mut out: Vec<Aggregate> = self.aggregates.values().cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn create_aggregate(&mut self, agg: Aggregate, or_replace: bool) -> Result<(), String> {
        let key = agg.name.to_ascii_lowercase();
        if self.aggregates.contains_key(&key) && !or_replace {
            return Err(format!("aggregate \"{}\" already exists", agg.name));
        }
        self.aggregates.insert(key, agg);
        Ok(())
    }

    pub fn drop_aggregate(&mut self, name: &str, if_exists: bool) -> Result<bool, String> {
        let key = name.to_ascii_lowercase();
        if self.aggregates.remove(&key).is_some() {
            Ok(true)
        } else if if_exists {
            Ok(false)
        } else {
            Err(format!("aggregate {name} does not exist"))
        }
    }

    pub fn extensions(&self) -> Vec<Extension> {
        let mut extensions: Vec<Extension> = self.extensions.values().cloned().collect();
        extensions.sort_by(|a, b| a.name.cmp(&b.name));
        extensions
    }

    pub fn roles(&self) -> Vec<Role> {
        let mut roles: Vec<Role> = self.roles.values().cloned().collect();
        roles.sort_by(|a, b| a.oid.cmp(&b.oid));
        roles
    }

    pub fn create_role(
        &mut self,
        name: String,
        login: bool,
        options: RoleOptions,
    ) -> Result<(), String> {
        if self.roles.contains_key(&name) {
            return Err(format!("role \"{name}\" already exists"));
        }
        let oid = self.roles.values().map(|role| role.oid).max().unwrap_or(9) + 1;
        let mut role = Role::new(oid, name.clone(), login);
        // Capture membership options before they're consumed by apply_options.
        let in_roles = options.in_role.clone();
        let role_members = options.role_members.clone();
        let admin_members = options.admin_members.clone();
        role.apply_options(options);
        self.roles.insert(name.clone(), role);
        // `IN ROLE g`: new role becomes a member of each g.
        for group in in_roles {
            self.role_memberships.insert((name.clone(), group));
        }
        // `ROLE m` / `ADMIN m`: each m becomes a member of the new role.
        for member in role_members.into_iter().chain(admin_members) {
            self.role_memberships.insert((member, name.clone()));
        }
        Ok(())
    }

    /// Grant table privileges to a grantee. Returns silently; acceptance only.
    pub fn grant_table_privilege(&mut self, table: &str, grantee: &str, privilege: &str) {
        self.table_privileges
            .insert((table.to_string(), grantee.to_string(), privilege.to_string()));
    }

    /// Revoke a table privilege from a grantee.
    pub fn revoke_table_privilege(&mut self, table: &str, grantee: &str, privilege: &str) {
        self.table_privileges
            .remove(&(table.to_string(), grantee.to_string(), privilege.to_string()));
    }

    /// Record that `member` is a member of `group` (role membership).
    pub fn grant_role_membership(&mut self, member: &str, group: &str) {
        self.role_memberships
            .insert((member.to_string(), group.to_string()));
    }

    /// Remove a role membership.
    pub fn revoke_role_membership(&mut self, member: &str, group: &str) {
        self.role_memberships
            .remove(&(member.to_string(), group.to_string()));
    }

    /// All role memberships as `(member_oid, group_oid, member_name, group_name)`,
    /// sorted for deterministic output. Unknown role names are skipped.
    pub fn role_memberships(&self) -> Vec<(i64, i64, String, String)> {
        let mut out: Vec<(i64, i64, String, String)> = self
            .role_memberships
            .iter()
            .filter_map(|(member, group)| {
                let m = self.roles.get(member)?;
                let g = self.roles.get(group)?;
                Some((m.oid, g.oid, member.clone(), group.clone()))
            })
            .collect();
        out.sort();
        out
    }

    pub fn alter_role(&mut self, name: &str, options: RoleOptions) -> Result<(), String> {
        let role = self
            .roles
            .get_mut(name)
            .ok_or_else(|| format!("role \"{name}\" does not exist"))?;
        role.apply_options(options);
        Ok(())
    }

    pub fn drop_role(&mut self, name: &str, if_exists: bool) -> Result<bool, String> {
        if name == "postgres" {
            return Err("cannot drop role \"postgres\"".into());
        }
        if self.roles.remove(name).is_some() {
            Ok(true)
        } else if if_exists {
            Ok(false)
        } else {
            Err(format!("role \"{name}\" does not exist"))
        }
    }

    pub fn create_extension(
        &mut self,
        name: String,
        version: Option<String>,
        if_not_exists: bool,
    ) -> Result<bool, String> {
        if self.extensions.contains_key(&name) {
            if if_not_exists {
                return Ok(false);
            }
            return Err(format!("extension \"{name}\" already exists"));
        }
        self.extensions.insert(
            name.clone(),
            Extension {
                name,
                version: version.unwrap_or_else(|| "1.0".into()),
            },
        );
        Ok(true)
    }

    pub fn drop_extension(&mut self, name: &str, if_exists: bool) -> Result<bool, String> {
        if self.extensions.remove(name).is_some() {
            return Ok(true);
        }
        if if_exists {
            Ok(false)
        } else {
            Err(format!("extension \"{name}\" does not exist"))
        }
    }

    pub fn sequences(&self) -> Vec<Sequence> {
        let mut sequences: Vec<Sequence> = self.sequences.values().cloned().collect();
        sequences.sort_by(|a, b| a.name.cmp(&b.name));
        sequences
    }

    pub fn create_sequence(
        &mut self,
        name: String,
        if_not_exists: bool,
        start: i64,
        increment: i64,
    ) -> Result<bool, String> {
        if increment == 0 {
            return Err("INCREMENT must not be zero".into());
        }
        if self.sequences.contains_key(&name) {
            if if_not_exists {
                return Ok(false);
            }
            return Err(format!("relation \"{name}\" already exists"));
        }
        self.sequences
            .insert(name.clone(), Sequence::new(name, start, increment));
        Ok(true)
    }

    pub fn alter_sequence(
        &mut self,
        name: &str,
        restart: Option<i64>,
        increment: Option<i64>,
    ) -> Result<(), String> {
        let sequence = self
            .sequences
            .get_mut(name)
            .ok_or_else(|| format!("relation \"{name}\" does not exist"))?;
        if let Some(increment) = increment {
            if increment == 0 {
                return Err("INCREMENT must not be zero".into());
            }
            sequence.increment = increment;
        }
        if let Some(restart) = restart {
            sequence.restart(restart);
        }
        Ok(())
    }

    pub fn drop_sequence(&mut self, name: &str, if_exists: bool) -> Result<bool, String> {
        if self.sequences.remove(name).is_some() {
            Ok(true)
        } else if if_exists {
            Ok(false)
        } else {
            Err(format!("sequence \"{name}\" does not exist"))
        }
    }

    pub fn next_sequence_value(&mut self, name: &str) -> Result<i64, String> {
        let sequence = self
            .sequences
            .get_mut(name)
            .ok_or_else(|| format!("relation \"{name}\" does not exist"))?;
        Ok(sequence.next_value())
    }

    pub fn current_sequence_value(&self, name: &str) -> Result<i64, String> {
        let sequence = self
            .sequences
            .get(name)
            .ok_or_else(|| format!("relation \"{name}\" does not exist"))?;
        if !sequence.called {
            return Err(format!("currval of sequence \"{name}\" is not yet defined"));
        }
        Ok(sequence.last_value)
    }

    pub fn set_sequence_value(
        &mut self,
        name: &str,
        value: i64,
        called: bool,
    ) -> Result<i64, String> {
        let sequence = self
            .sequences
            .get_mut(name)
            .ok_or_else(|| format!("relation \"{name}\" does not exist"))?;
        sequence.last_value = value;
        sequence.called = called;
        Ok(value)
    }

    pub fn advisory_lock(&mut self, classid: i64, objid: i64) {
        self.advisory_locks.insert((classid, objid));
    }

    pub fn try_advisory_lock(&mut self, classid: i64, objid: i64) -> bool {
        self.advisory_locks.insert((classid, objid));
        true
    }

    pub fn advisory_unlock(&mut self, classid: i64, objid: i64) -> bool {
        self.advisory_locks.remove(&(classid, objid))
    }

    pub fn advisory_unlock_all(&mut self) {
        self.advisory_locks.clear();
    }

    pub fn advisory_locks(&self) -> Vec<AdvisoryLock> {
        let mut locks: Vec<AdvisoryLock> = self
            .advisory_locks
            .iter()
            .map(|(classid, objid)| AdvisoryLock {
                classid: *classid,
                objid: *objid,
            })
            .collect();
        locks.sort_by(|a, b| (a.classid, a.objid).cmp(&(b.classid, b.objid)));
        locks
    }

    pub fn vacuum_table_storage(&mut self, name: &str) -> Result<VacuumStats, String> {
        let table = self
            .tables
            .get_mut(name)
            .ok_or_else(|| format!("relation \"{name}\" does not exist"))?;
        Ok(table.vacuum_storage())
    }

    pub fn vacuum_storage(&mut self) -> VacuumStats {
        let mut stats = VacuumStats::empty();
        for table in self.tables.values_mut() {
            stats.absorb(table.vacuum_storage());
        }
        stats
    }

    /// Rename a table (and re-key its `serial` sequences).
    pub fn rename_table(&mut self, from: &str, to: &str) -> Result<(), String> {
        if !self.tables.contains_key(from) {
            return Err(format!("relation \"{from}\" does not exist"));
        }
        if self.tables.contains_key(to) {
            return Err(format!("relation \"{to}\" already exists"));
        }
        let mut table = self.tables.remove(from).unwrap();
        table.name = to.to_string();
        self.tables.insert(to.to_string(), table);

        // Re-key sequences "from.col" -> "to.col".
        let moved: Vec<(String, Sequence)> = self
            .sequences
            .iter()
            .filter_map(|(k, v)| {
                k.strip_prefix(&format!("{from}."))
                    .map(|c| (c.to_string(), v.clone()))
            })
            .collect();
        for (col, mut v) in moved {
            self.sequences.remove(&format!("{from}.{col}"));
            v.name = format!("{to}.{col}");
            self.sequences.insert(v.name.clone(), v);
        }
        Ok(())
    }

    #[allow(dead_code)]
    /// Return the next value of a sequence, advancing it.
    pub fn next_sequence(&mut self, key: &str) -> i64 {
        self.sequences
            .entry(key.to_string())
            .or_insert_with(|| Sequence::new(key.to_string(), 1, 1))
            .next_value()
    }

    /// Ensure the sequence is at least `value` (used when an explicit value is
    /// inserted into a `serial` column, to avoid future collisions).
    pub fn observe_sequence(&mut self, key: &str, value: i64) {
        let entry = self
            .sequences
            .entry(key.to_string())
            .or_insert_with(|| Sequence::new(key.to_string(), 1, 1));
        if value > entry.last_value {
            entry.last_value = value;
            entry.called = true;
        }
    }

    #[allow(dead_code)]
    pub fn table_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tables.keys().cloned().collect();
        names.sort();
        names
    }
}
