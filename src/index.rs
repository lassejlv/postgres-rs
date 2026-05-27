//! B-tree secondary indexes built on the standard library's [`BTreeMap`].
//!
//! An index maps the value(s) of one column to the *stable row ids* of the
//! rows that hold that value (see [`crate::storage::Table`] for the row-id
//! scheme). Because a `BTreeMap` keeps its keys in sorted order, a single
//! structure answers both equality lookups (`col = const`, `col IN (...)`) and
//! range scans (`col < c`, `BETWEEN`, ...) in `O(log n + k)` instead of the
//! `O(n)` full scan the executor would otherwise do.
//!
//! Indexes are maintained incrementally: every INSERT/UPDATE/DELETE that the
//! executor performs also patches the indexes of the affected table, so the
//! index never drifts from the heap. They are part of [`crate::storage::Table`]
//! and therefore clone with it (used for transaction snapshots) — a `BTreeMap`
//! clone is a cheap structural copy, no rebuild required.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::types::Value;

/// A stable identifier for a row within one table.
///
/// Unlike a positional index into the `rows` `Vec`, a `RowId` does not change
/// when other rows are deleted, so it is safe to store inside an index.
pub type RowId = u64;

/// A `Value` wrapped so it can serve as a totally-ordered `BTreeMap` key.
///
/// The ordering mirrors the executor's `compare_values`: integers and floats
/// compare numerically (so an `Int` and a `Float` of equal magnitude are
/// "equal" for index purposes), text compares lexically, booleans by `false <
/// true`, and `NULL` sorts last. Crucially the order is *total* (no `None`
/// results), which `BTreeMap` requires — incomparable pairs (which equality
/// SQL would treat as non-matching) are given an arbitrary-but-stable order so
/// the tree stays well-formed; the executor re-checks the actual predicate, so
/// a key collision can never produce a wrong result.
#[derive(Debug, Clone)]
pub struct IndexKey(pub Value);

impl IndexKey {
    /// The total order used for indexing. Kept consistent with the executor's
    /// `compare_values` for the cases SQL can actually compare.
    fn cmp_value(a: &Value, b: &Value) -> Ordering {
        match (a, b) {
            // NULLs sort last and equal to each other.
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Greater,
            (_, Value::Null) => Ordering::Less,
            (Value::Int(x), Value::Int(y)) => x.cmp(y),
            (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
            (Value::Text(x), Value::Text(y)) => x.cmp(y),
            // Any numeric mix compares as f64 (matching `compare_values`).
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                let x = numeric(a);
                let y = numeric(b);
                x.partial_cmp(&y).unwrap_or(Ordering::Equal)
            }
            // Mixed, genuinely-incomparable types: order by a stable type rank
            // so the tree is well-formed. The executor never relies on this
            // order being meaningful — it re-evaluates the real predicate.
            _ => type_rank(a).cmp(&type_rank(b)),
        }
    }
}

/// Map a numeric value to f64 for comparison (only called on Int/Float).
fn numeric(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        _ => unreachable!("numeric() called on non-numeric value"),
    }
}

/// A stable rank per value kind, used only to totally order otherwise
/// incomparable pairs.
fn type_rank(v: &Value) -> u8 {
    match v {
        Value::Bool(_) => 0,
        Value::Int(_) => 1,
        Value::Float(_) => 2,
        Value::Text(_) => 3,
        Value::Null => 4,
    }
}

impl PartialEq for IndexKey {
    fn eq(&self, other: &Self) -> bool {
        IndexKey::cmp_value(&self.0, &other.0) == Ordering::Equal
    }
}
impl Eq for IndexKey {}
impl PartialOrd for IndexKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for IndexKey {
    fn cmp(&self, other: &Self) -> Ordering {
        IndexKey::cmp_value(&self.0, &other.0)
    }
}

/// A secondary B-tree index over a single column of a table.
#[derive(Debug, Clone)]
pub struct Index {
    /// Index name (used by `DROP INDEX`). Auto-generated for primary keys.
    pub name: String,
    /// The 0-based column position this index covers within the table.
    pub column: usize,
    /// Whether this index enforces uniqueness (true for PRIMARY KEY indexes).
    /// Uniqueness is not yet *enforced* on insert, matching the engine's
    /// current constraint behavior, but the flag is tracked for introspection
    /// and future enforcement.
    pub unique: bool,
    /// Sorted map from key value to the row ids that currently hold it.
    /// Multiple row ids per key supports non-unique indexes and duplicates.
    tree: BTreeMap<IndexKey, Vec<RowId>>,
}

impl Index {
    /// Create an empty index over `column`.
    pub fn new(name: String, column: usize, unique: bool) -> Self {
        Index { name, column, unique, tree: BTreeMap::new() }
    }

    /// Record that `row_id` now holds `value` in the indexed column.
    pub fn insert(&mut self, value: &Value, row_id: RowId) {
        self.tree.entry(IndexKey(value.clone())).or_default().push(row_id);
    }

    /// Remove the `(value, row_id)` association (used on UPDATE/DELETE).
    pub fn remove(&mut self, value: &Value, row_id: RowId) {
        if let Some(ids) = self.tree.get_mut(&IndexKey(value.clone())) {
            if let Some(pos) = ids.iter().position(|&r| r == row_id) {
                ids.swap_remove(pos);
            }
            if ids.is_empty() {
                self.tree.remove(&IndexKey(value.clone()));
            }
        }
    }

    /// Row ids whose indexed value equals `value` (point lookup).
    pub fn lookup_eq(&self, value: &Value) -> &[RowId] {
        match self.tree.get(&IndexKey(value.clone())) {
            Some(ids) => ids,
            None => &[],
        }
    }

    /// Row ids whose indexed value falls in the (optionally bounded) range.
    /// `lo`/`hi` carry the bound value and whether it is inclusive. Returns row
    /// ids in ascending key order. NULLs (which sort last) are never returned
    /// from a range scan, matching SQL semantics where comparisons with NULL
    /// are never true.
    pub fn lookup_range(&self, lo: Option<Bound>, hi: Option<Bound>) -> Vec<RowId> {
        use std::ops::Bound as B;
        let start = match &lo {
            Some(b) if b.inclusive => B::Included(IndexKey(b.value.clone())),
            Some(b) => B::Excluded(IndexKey(b.value.clone())),
            None => B::Unbounded,
        };
        // Cap the high end *below* NULL so NULL keys (which sort last) are
        // excluded even on an unbounded-high range scan.
        let end = match &hi {
            Some(b) if b.inclusive => B::Included(IndexKey(b.value.clone())),
            Some(b) => B::Excluded(IndexKey(b.value.clone())),
            None => B::Excluded(IndexKey(Value::Null)),
        };
        let mut out = Vec::new();
        for (key, ids) in self.tree.range((start, end)) {
            // Defensive: skip NULL even if a bound somehow included it.
            if key.0.is_null() {
                continue;
            }
            out.extend_from_slice(ids);
        }
        out
    }
}

/// One end of a range scan: the bound value and whether it is inclusive.
#[derive(Debug, Clone)]
pub struct Bound {
    pub value: Value,
    pub inclusive: bool,
}
