//! Secondary indexes built on the standard library's [`BTreeMap`] (B-tree
//! indexes) or [`HashMap`] (hash indexes).
//!
//! An index maps the value(s) of one or more columns — or the result of an
//! indexed *expression* — to the *stable row ids* of the rows that hold that
//! key (see [`crate::storage::Table`] for the row-id scheme). A `BTreeMap`
//! keeps its keys in sorted order, so a single structure answers both equality
//! lookups (`col = const`, `col IN (...)`) and range scans (`col < c`,
//! `BETWEEN`, ...) in `O(log n + k)` instead of the `O(n)` full scan. A hash
//! index supports equality only.
//!
//! Indexes are maintained incrementally: every INSERT/UPDATE/DELETE that the
//! executor performs also patches the indexes of the affected table, so the
//! index never drifts from the heap. They are part of [`crate::storage::Table`]
//! and therefore clone with it (used for transaction snapshots).
//!
//! Index *keys* are computed by [`crate::storage::Table`] (which can read the
//! row columns and, for expression indexes, evaluate the stored expression).
//! The index structure itself is value-based and knows nothing about SQL
//! expressions — it just stores `Vec<Value>` keys.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

use crate::sql::ast::Expr;
use crate::types::Value;

/// A stable identifier for a row within one table.
///
/// Unlike a positional index into the `rows` `Vec`, a `RowId` does not change
/// when other rows are deleted, so it is safe to store inside an index.
pub type RowId = u64;

/// The access method used to physically organise an index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMethod {
    /// Sorted B-tree: supports equality, `IN`, and range scans.
    Btree,
    /// Hash table: supports equality lookups only (no range scans).
    Hash,
}

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

/// A multi-column key: the wrapped values of the indexed columns/expression in
/// index order, compared lexicographically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyVec(pub Vec<IndexKey>);

impl PartialOrd for KeyVec {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for KeyVec {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

/// A canonical, hashable encoding of a key vector for use as a `HashMap` key.
///
/// Built from each value's text representation prefixed by a type tag so that
/// distinct types never alias. NULL keys are never inserted into a hash index
/// (equality never matches NULL), so they need no encoding here.
fn hash_key(values: &[Value]) -> String {
    let mut s = String::new();
    for v in values {
        match v {
            Value::Null => s.push_str("N|"),
            Value::Int(i) => {
                s.push('i');
                s.push_str(&i.to_string());
            }
            // Normalise integral floats so 1 and 1.0 collide the same way the
            // B-tree's numeric comparison treats them as equal.
            Value::Float(f) => {
                s.push('i');
                s.push_str(&(*f as i64).to_string());
                if f.fract() != 0.0 {
                    s.push('f');
                    s.push_str(&format!("{f}"));
                }
            }
            Value::Text(t) => {
                s.push('s');
                s.push_str(t);
            }
            Value::Bool(b) => {
                s.push('b');
                s.push(if *b { '1' } else { '0' });
            }
        }
        s.push('|');
    }
    s
}

/// Whether any value in a key is NULL (such keys are excluded from hash/range
/// equality matching, matching SQL semantics).
fn key_has_null(values: &[Value]) -> bool {
    values.iter().any(|v| v.is_null())
}

/// The backing store of an index: either a sorted B-tree or a hash table.
#[derive(Debug, Clone)]
enum Store {
    Btree(BTreeMap<KeyVec, Vec<RowId>>),
    Hash(HashMap<String, Vec<RowId>>),
}

/// A secondary index over one or more columns (or an expression) of a table.
#[derive(Debug, Clone)]
pub struct Index {
    /// Index name (used by `DROP INDEX`). Auto-generated for primary keys.
    pub name: String,
    /// The 0-based column positions this index covers, in index order. Empty
    /// for a pure expression index.
    pub columns: Vec<usize>,
    /// For an expression index, the expression whose value is the key. The
    /// table evaluates it per row; `columns` is then empty.
    pub expr: Option<Expr>,
    /// For a partial index, the predicate a row must satisfy to be indexed.
    pub predicate: Option<Expr>,
    /// Covering / `INCLUDE` columns. Stored for introspection; not used for
    /// index-only scans.
    pub include: Vec<usize>,
    /// Whether this index enforces uniqueness (true for PRIMARY KEY indexes).
    pub unique: bool,
    /// The access method.
    pub method: IndexMethod,
    /// Backing store keyed by the index key.
    store: Store,
}

impl Index {
    /// Create an empty B-tree index over a single `column` (the legacy shape
    /// used by primary keys and single-column `CREATE INDEX`).
    pub fn new(name: String, column: usize, unique: bool) -> Self {
        Index::new_multi(name, vec![column], unique, IndexMethod::Btree)
    }

    /// Create an empty index over `columns` with the given method.
    pub fn new_multi(
        name: String,
        columns: Vec<usize>,
        unique: bool,
        method: IndexMethod,
    ) -> Self {
        Index {
            name,
            columns,
            expr: None,
            predicate: None,
            include: Vec::new(),
            unique,
            method,
            store: empty_store(method),
        }
    }

    /// The single leading column of this index (used by older single-column
    /// call sites and by uniqueness checks). For a pure expression index there
    /// is no such column, so this returns `None`.
    pub fn leading_column(&self) -> Option<usize> {
        self.columns.first().copied()
    }

    /// Record that `row_id` now holds `key` (already-evaluated key values).
    pub fn insert(&mut self, key: Vec<Value>, row_id: RowId) {
        match &mut self.store {
            Store::Btree(tree) => {
                tree.entry(KeyVec(key.into_iter().map(IndexKey).collect()))
                    .or_default()
                    .push(row_id);
            }
            Store::Hash(map) => {
                // NULL keys never equality-match, so don't store them.
                if key_has_null(&key) {
                    return;
                }
                map.entry(hash_key(&key)).or_default().push(row_id);
            }
        }
    }

    /// Remove the `(key, row_id)` association (used on UPDATE/DELETE).
    pub fn remove(&mut self, key: Vec<Value>, row_id: RowId) {
        match &mut self.store {
            Store::Btree(tree) => {
                let k = KeyVec(key.into_iter().map(IndexKey).collect());
                if let Some(ids) = tree.get_mut(&k) {
                    if let Some(pos) = ids.iter().position(|&r| r == row_id) {
                        ids.swap_remove(pos);
                    }
                    if ids.is_empty() {
                        tree.remove(&k);
                    }
                }
            }
            Store::Hash(map) => {
                if key_has_null(&key) {
                    return;
                }
                let k = hash_key(&key);
                if let Some(ids) = map.get_mut(&k) {
                    if let Some(pos) = ids.iter().position(|&r| r == row_id) {
                        ids.swap_remove(pos);
                    }
                    if ids.is_empty() {
                        map.remove(&k);
                    }
                }
            }
        }
    }

    pub fn clear(&mut self) {
        self.store = empty_store(self.method);
    }

    /// Row ids whose full key equals `key` (point lookup). Works for both
    /// B-tree and hash indexes.
    pub fn lookup_eq(&self, key: &[Value]) -> Vec<RowId> {
        match &self.store {
            Store::Btree(tree) => {
                let k = KeyVec(key.iter().cloned().map(IndexKey).collect());
                tree.get(&k).cloned().unwrap_or_default()
            }
            Store::Hash(map) => {
                if key_has_null(key) {
                    return Vec::new();
                }
                map.get(&hash_key(key)).cloned().unwrap_or_default()
            }
        }
    }

    /// All row ids currently held by this index (used to scan a partial index
    /// whose predicate fully covers the query).
    pub fn all_row_ids(&self) -> Vec<RowId> {
        let mut out = Vec::new();
        match &self.store {
            Store::Btree(tree) => {
                for ids in tree.values() {
                    out.extend_from_slice(ids);
                }
            }
            Store::Hash(map) => {
                for ids in map.values() {
                    out.extend_from_slice(ids);
                }
            }
        }
        out
    }

    /// Row ids whose key has the given leading prefix. Only valid for B-tree
    /// indexes; a hash index returns an empty result (the planner never asks).
    pub fn lookup_prefix(&self, prefix: &[Value]) -> Vec<RowId> {
        let Store::Btree(tree) = &self.store else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (key, ids) in tree.iter() {
            if key.0.len() < prefix.len() {
                continue;
            }
            let matches = prefix
                .iter()
                .zip(&key.0)
                .all(|(p, k)| IndexKey(p.clone()) == *k);
            if matches {
                out.extend_from_slice(ids);
            }
        }
        out
    }

    /// Row ids whose (single-column) key falls in the (optionally bounded)
    /// range. Only meaningful for single-column B-tree indexes; NULLs are never
    /// returned. Returns ids in ascending key order.
    pub fn lookup_range(&self, lo: Option<Bound>, hi: Option<Bound>) -> Vec<RowId> {
        use std::ops::Bound as B;
        let Store::Btree(tree) = &self.store else {
            return Vec::new();
        };
        let start = match &lo {
            Some(b) if b.inclusive => B::Included(KeyVec(vec![IndexKey(b.value.clone())])),
            Some(b) => B::Excluded(KeyVec(vec![IndexKey(b.value.clone())])),
            None => B::Unbounded,
        };
        // Cap the high end *below* NULL so NULL keys (which sort last) are
        // excluded even on an unbounded-high range scan.
        let end = match &hi {
            Some(b) if b.inclusive => B::Included(KeyVec(vec![IndexKey(b.value.clone())])),
            Some(b) => B::Excluded(KeyVec(vec![IndexKey(b.value.clone())])),
            None => B::Excluded(KeyVec(vec![IndexKey(Value::Null)])),
        };
        let mut out = Vec::new();
        for (key, ids) in tree.range((start, end)) {
            // Defensive: skip NULL even if a bound somehow included it.
            if key.0.first().map(|k| k.0.is_null()).unwrap_or(true) {
                continue;
            }
            out.extend_from_slice(ids);
        }
        out
    }
}

fn empty_store(method: IndexMethod) -> Store {
    match method {
        IndexMethod::Btree => Store::Btree(BTreeMap::new()),
        IndexMethod::Hash => Store::Hash(HashMap::new()),
    }
}

/// One end of a range scan: the bound value and whether it is inclusive.
#[derive(Debug, Clone)]
pub struct Bound {
    pub value: Value,
    pub inclusive: bool,
}
