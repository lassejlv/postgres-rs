//! In-memory storage engine.
//!
//! This is the first storage backend: a simple set of named tables, each an
//! ordered `Vec` of rows. It is intentionally minimal but defines the
//! interface (`Database`, `Table`, `Column`) that a future disk-backed,
//! WAL-logged engine will implement.

use std::collections::HashMap;

use crate::sql::ast::Expr;
use crate::types::{DataType, Value};

/// A table column: a name and its declared type, plus simple constraints.
#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub data_type: DataType,
    pub not_null: bool,
    /// Reserved for primary-key/uniqueness enforcement (not yet enforced).
    #[allow(dead_code)]
    pub primary_key: bool,
    /// `DEFAULT` expression applied when the column is omitted from an INSERT.
    pub default: Option<Expr>,
    /// Auto-incrementing (`serial`): values come from a sequence on insert.
    pub serial: bool,
}

/// A stored table: schema plus its rows.
#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
}

impl Table {
    /// Index of a column by name (case-sensitive, matching how it was created).
    #[allow(dead_code)]
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    pub fn column_names(&self) -> Vec<String> {
        self.columns.iter().map(|c| c.name.clone()).collect()
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
