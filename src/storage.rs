//! In-memory storage engine.
//!
//! This is the first storage backend: a simple set of named tables, each an
//! ordered `Vec` of rows. It is intentionally minimal but defines the
//! interface (`Database`, `Table`, `Column`) that a future disk-backed,
//! WAL-logged engine will implement.

use std::collections::HashMap;

use crate::index::{Bound, Index, RowId};
use crate::sql::ast::Expr;
use crate::types::{DataType, Value};

/// A table column: a name and its declared type, plus simple constraints.
#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub data_type: DataType,
    pub not_null: bool,
    /// Whether this column is a PRIMARY KEY (enforced via a unique index).
    #[allow(dead_code)]
    pub primary_key: bool,
    /// `DEFAULT` expression applied when the column is omitted from an INSERT.
    pub default: Option<Expr>,
    /// Auto-incrementing (`serial`): values come from a sequence on insert.
    pub serial: bool,
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
}

impl Table {
    /// Create an empty table with the given schema and no indexes.
    pub fn new(name: String, columns: Vec<Column>) -> Self {
        Table {
            name,
            columns,
            rows: Vec::new(),
            row_ids: Vec::new(),
            row_pos: HashMap::new(),
            next_row_id: 0,
            indexes: Vec::new(),
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

    /// Append a row, assigning it a fresh stable id and updating all indexes.
    pub fn push_row(&mut self, row: Vec<Value>) {
        let id = self.next_row_id;
        self.next_row_id += 1;
        let pos = self.rows.len();
        for idx in &mut self.indexes {
            idx.insert(&row[idx.column], id);
        }
        self.rows.push(row);
        self.row_ids.push(id);
        self.row_pos.insert(id, pos);
    }

    /// Replace the row at position `pos` with `new_row`, keeping its id and
    /// repairing every index whose column changed.
    pub fn update_row(&mut self, pos: usize, new_row: Vec<Value>) {
        let id = self.row_ids[pos];
        for idx in &mut self.indexes {
            let old = &self.rows[pos][idx.column];
            let new = &new_row[idx.column];
            if old != new {
                idx.remove(old, id);
                idx.insert(new, id);
            }
        }
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
        for idx in &mut self.indexes {
            for (p, dropped) in drop_mask.iter().enumerate() {
                if *dropped {
                    idx.remove(&self.rows[p][idx.column], self.row_ids[p]);
                }
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

    /// Current position of a row id, if it still exists.
    pub fn position_of(&self, id: RowId) -> Option<usize> {
        self.row_pos.get(&id).copied()
    }

    // --- index management ----------------------------------------------------

    /// Find an index over `column`, preferring a unique one when both exist.
    pub fn index_on(&self, column: usize) -> Option<&Index> {
        let mut chosen: Option<&Index> = None;
        for idx in &self.indexes {
            if idx.column == column {
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

    /// If inserting/updating `row` would collide with an existing row on a
    /// unique index, return that index's name. `exclude` skips a position (the
    /// row being updated, so it doesn't conflict with itself). NULLs never
    /// conflict (SQL permits multiple NULLs in a unique index).
    pub fn unique_violation(&self, row: &[Value], exclude: Option<usize>) -> Option<String> {
        for idx in &self.indexes {
            if !idx.unique {
                continue;
            }
            let value = &row[idx.column];
            if value.is_null() {
                continue;
            }
            if let Some(positions) = self.index_eq(idx.column, value) {
                if positions.iter().any(|&p| Some(p) != exclude) {
                    return Some(idx.name.clone());
                }
            }
        }
        None
    }

    /// Build and populate a new index over `column` from the current rows.
    pub fn create_index(&mut self, name: String, column: usize, unique: bool) {
        let mut idx = Index::new(name, column, unique);
        for (row, &id) in self.rows.iter().zip(&self.row_ids) {
            idx.insert(&row[column], id);
        }
        self.indexes.push(idx);
    }

    /// Columns covered by a unique index (for batch duplicate checks).
    pub fn unique_index_columns(&self) -> Vec<usize> {
        self.indexes.iter().filter(|i| i.unique).map(|i| i.column).collect()
    }

    /// Drop an index by name, returning whether it existed.
    pub fn drop_index(&mut self, name: &str) -> bool {
        let before = self.indexes.len();
        self.indexes.retain(|i| i.name != name);
        self.indexes.len() != before
    }

    // --- index-accelerated scans ---------------------------------------------

    /// Row positions whose `column` equals `value`, via an index if one exists.
    /// Positions are returned for the caller to read from `rows`.
    pub fn index_eq(&self, column: usize, value: &Value) -> Option<Vec<usize>> {
        let idx = self.index_on(column)?;
        Some(self.ids_to_positions(idx.lookup_eq(value)))
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

    /// Translate a set of row ids into current row positions, dropping any that
    /// no longer exist (defensive; ids in an index should always be present).
    fn ids_to_positions(&self, ids: &[RowId]) -> Vec<usize> {
        ids.iter().filter_map(|&id| self.position_of(id)).collect()
    }
}

/// The whole database: a flat namespace of tables.
///
/// `Clone` is used to snapshot the database when a transaction begins, so a
/// `ROLLBACK` can restore the prior state.
#[derive(Debug, Default, Clone)]
pub struct Database {
    tables: HashMap<String, Table>,
    /// Sequence counters for `serial` columns, keyed by `"table.column"`,
    /// storing the last-issued value (next value is this + 1).
    sequences: HashMap<String, i64>,
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

    /// Create a table. Errors if it already exists (caller handles
    /// `IF NOT EXISTS` before calling).
    pub fn create_table(&mut self, table: Table) -> Result<(), String> {
        if self.tables.contains_key(&table.name) {
            return Err(format!("relation \"{}\" already exists", table.name));
        }
        self.tables.insert(table.name.clone(), table);
        Ok(())
    }

    /// Drop a table, returning whether it existed.
    pub fn drop_table(&mut self, name: &str) -> bool {
        self.tables.remove(name).is_some()
    }

    #[allow(dead_code)]
    /// Return the next value of a sequence, advancing it.
    pub fn next_sequence(&mut self, key: &str) -> i64 {
        let entry = self.sequences.entry(key.to_string()).or_insert(0);
        *entry += 1;
        *entry
    }

    /// Ensure the sequence is at least `value` (used when an explicit value is
    /// inserted into a `serial` column, to avoid future collisions).
    pub fn observe_sequence(&mut self, key: &str, value: i64) {
        let entry = self.sequences.entry(key.to_string()).or_insert(0);
        if value > *entry {
            *entry = value;
        }
    }

    #[allow(dead_code)]
    pub fn table_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tables.keys().cloned().collect();
        names.sort();
        names
    }
}
