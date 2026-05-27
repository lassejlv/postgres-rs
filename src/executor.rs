//! Query executor: turns a parsed [`Statement`] into a result against the
//! in-memory [`Database`].

use std::cmp::Ordering;

use crate::index::Bound;
use crate::sql::ast::*;
use crate::storage::{Column, Database, Table};
use crate::types::{DataType, Value};

/// A column heading in a result set.
#[derive(Debug, Clone)]
pub struct FieldDescription {
    pub name: String,
    pub data_type: DataType,
}

/// The outcome of executing a statement.
pub enum ExecResult {
    /// A result set: column descriptions, row data, and the command tag to
    /// report on completion (e.g. `"SELECT 3"`, `"INSERT 0 1"` for RETURNING).
    Rows { fields: Vec<FieldDescription>, rows: Vec<Vec<Value>>, tag: String },
    /// A command completed with the given PostgreSQL command tag,
    /// e.g. `"INSERT 0 3"`, `"CREATE TABLE"`, `"UPDATE 2"`.
    Command(String),
    /// An empty query (no statement).
    Empty,
}

pub fn execute(db: &mut Database, stmt: Statement) -> Result<ExecResult, String> {
    match stmt {
        Statement::CreateTable(c) => exec_create_table(db, c),
        Statement::DropTable(d) => exec_drop_table(db, d),
        Statement::AlterTable(a) => exec_alter_table(db, a),
        Statement::CreateIndex(c) => exec_create_index(db, c),
        Statement::DropIndex(d) => exec_drop_index(db, d),
        Statement::Insert(i) => exec_insert(db, i),
        Statement::Select(s) => exec_select(db, s),
        Statement::Update(u) => exec_update(db, u),
        Statement::Delete(d) => exec_delete(db, d),
        Statement::Begin => Ok(ExecResult::Command("BEGIN".into())),
        Statement::Commit => Ok(ExecResult::Command("COMMIT".into())),
        Statement::Rollback => Ok(ExecResult::Command("ROLLBACK".into())),
        Statement::Set { .. } => Ok(ExecResult::Command("SET".into())),
        Statement::Show { name } => exec_show(name),
        Statement::Empty => Ok(ExecResult::Empty),
    }
}

/// Compute the result columns a statement would produce, without running it.
/// Returns `None` for statements that yield no row set (DML/DDL).
///
/// Used by the extended-query protocol's Describe step.
pub fn describe_statement(
    db: &Database,
    stmt: &Statement,
) -> Result<Option<Vec<FieldDescription>>, String> {
    match stmt {
        Statement::Select(sel) => Ok(Some(select_fields(db, sel)?)),
        Statement::Show { name } => Ok(Some(vec![FieldDescription {
            name: name.clone(),
            data_type: DataType::Text,
        }])),
        _ => Ok(None),
    }
}

/// Derive the output field list of a SELECT from the schema alone.
fn select_fields(db: &Database, sel: &Select) -> Result<Vec<FieldDescription>, String> {
    let (col_names, col_types) = match &sel.from {
        Some(fc) => from_schema(db, fc)?,
        None => (Vec::new(), Vec::new()),
    };
    let mut fields = Vec::new();
    for item in &sel.projection {
        match item {
            SelectItem::Wildcard => {
                for (i, name) in col_names.iter().enumerate() {
                    fields.push(FieldDescription { name: bare_name(name), data_type: col_types[i] });
                }
            }
            SelectItem::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| default_column_name(expr));
                let data_type = infer_expr_type(expr, &col_names, &col_types);
                fields.push(FieldDescription { name, data_type });
            }
        }
    }
    Ok(fields)
}

/// Compute the qualified column names and types of a FROM clause, without
/// caring about row data (used to describe a query's result shape).
fn from_schema(db: &Database, from: &FromClause) -> Result<(Vec<String>, Vec<DataType>), String> {
    let (mut names, mut types, _) = resolve_source_table(db, &from.base)?;
    for j in &from.joins {
        let (rn, rt, _) = resolve_source_table(db, &j.table)?;
        names.extend(rn);
        types.extend(rt);
    }
    Ok((names, types))
}

/// Materialize a FROM clause (base table + nested-loop joins) into a flat
/// rowset with qualified column names and types.
///
/// `filter` is the SELECT's WHERE predicate, used purely as an optimization
/// hint: when the base table has a usable index it prunes the base scan to the
/// matching rows. The predicate is still applied in full by the caller, so a
/// missed or over-broad prune never changes the result.
fn build_source(
    db: &Database,
    from: &FromClause,
    filter: Option<&Expr>,
) -> Result<(Vec<String>, Vec<DataType>, Vec<Vec<Value>>), String> {
    // Base pruning is only safe to drive from the WHERE clause when there is
    // no join (a join's WHERE could reference other tables' columns, and the
    // filter runs after the join). With joins, the base is fully scanned and
    // any indexed join is handled per-inner-side below.
    let base_filter = if from.joins.is_empty() { filter } else { None };
    let (mut names, mut types, mut rows) = resolve_base_source(db, &from.base, base_filter)?;

    for j in &from.joins {
        let (right_names, right_types, _) = resolve_source_table(db, &j.table)?;
        let right_width = right_names.len();
        let left_width = names.len();

        // The schema visible to the ON predicate is left columns ++ right.
        let mut combined_names = names.clone();
        combined_names.extend(right_names.iter().cloned());

        // Try an indexed nested-loop join: if the inner (right) side is a real
        // table whose join column is indexed and the ON clause is a simple
        // equality between a left column and that right column, we can probe
        // the index per left row instead of scanning every right row.
        let inner = indexed_join_inner(db, j, &names);

        let mut joined = Vec::new();
        // Resolve the right rows once (used by the scan path and to map index
        // hits back to row data for the indexed path).
        let (_, _, right_rows) = resolve_source_table(db, &j.table)?;
        let mut right_matched = vec![false; right_rows.len()];

        for left_row in &rows {
            let mut matched = false;
            // Choose the candidate right-row indices for this left row.
            let candidates: Vec<usize> = match &inner {
                Some(probe) => probe.candidates_for(db, left_row, &names)?,
                None => (0..right_rows.len()).collect(),
            };
            for ri in candidates {
                let right_row = &right_rows[ri];
                let mut combo = left_row.clone();
                combo.extend(right_row.iter().cloned());
                let on_true = match &j.on {
                    None => true, // CROSS JOIN
                    Some(pred) => eval_expr(pred, &combined_names, &combo)?.is_true(),
                };
                if on_true {
                    joined.push(combo);
                    matched = true;
                    right_matched[ri] = true;
                }
            }
            // LEFT/FULL: emit the left row NULL-extended when nothing matched.
            if !matched && matches!(j.kind, JoinKind::Left | JoinKind::Full) {
                let mut combo = left_row.clone();
                combo.extend(std::iter::repeat_n(Value::Null, right_width));
                joined.push(combo);
            }
        }
        // RIGHT/FULL: emit unmatched right rows with a NULL-extended left side.
        if matches!(j.kind, JoinKind::Right | JoinKind::Full) {
            for (ri, right_row) in right_rows.iter().enumerate() {
                if !right_matched[ri] {
                    let mut combo: Vec<Value> = std::iter::repeat_n(Value::Null, left_width).collect();
                    combo.extend(right_row.iter().cloned());
                    joined.push(combo);
                }
            }
        }

        names = combined_names;
        types.extend(right_types);
        rows = joined;
    }

    Ok((names, types, rows))
}

/// Resolve the base table of a FROM clause, pruning to index candidates when
/// the WHERE predicate permits and there is no join that could reference other
/// tables (we still re-check the predicate later, so this only narrows rows).
fn resolve_base_source(
    db: &Database,
    tref: &TableRef,
    filter: Option<&Expr>,
) -> Result<(Vec<String>, Vec<DataType>, Vec<Vec<Value>>), String> {
    // Only real tables (not the virtual catalog views) carry indexes.
    let is_real = tref.schema.as_deref() != Some("information_schema")
        && tref.schema.as_deref() != Some("pg_catalog")
        && !is_pg_catalog_table(&tref.name);
    if let (true, Some(pred)) = (is_real, filter) {
        if let Some(table) = db.table(&tref.name) {
            if let Some(positions) = index_candidate_positions(table, pred) {
                let mut names = Vec::with_capacity(table.columns.len());
                let mut types = Vec::with_capacity(table.columns.len());
                for c in &table.columns {
                    names.push(format!("{}.{}", tref.qualifier(), c.name));
                    types.push(c.data_type);
                }
                let rows = positions.into_iter().map(|p| table.rows[p].clone()).collect();
                return Ok((names, types, rows));
            }
        }
    }
    resolve_source_table(db, tref)
}

/// An indexed inner side of a join: which right-table column is indexed and
/// which left column feeds the probe.
struct IndexedJoinProbe {
    /// The inner (right) table's name.
    table: String,
    /// Indexed column position within the right table.
    right_col: usize,
    /// The left column index (into the current left schema) used as the key.
    left_col: usize,
}

impl IndexedJoinProbe {
    /// Candidate right-row positions for a given left row, via the index.
    fn candidates_for(
        &self,
        db: &Database,
        left_row: &[Value],
        _left_names: &[String],
    ) -> Result<Vec<usize>, String> {
        let key = &left_row[self.left_col];
        // A NULL key never equality-matches, so probe yields nothing.
        if key.is_null() {
            return Ok(Vec::new());
        }
        let table = db.table(&self.table).expect("inner table existed at planning");
        Ok(table.index_eq(self.right_col, key).unwrap_or_default())
    }
}

/// Detect an indexed nested-loop opportunity for join `j` whose left schema is
/// `left_names`. Requires an INNER/LEFT join with an `ON left.x = right.y`
/// equality where `right.y` is indexed. Returns `None` to fall back to the
/// nested-loop scan.
fn indexed_join_inner(db: &Database, j: &Join, left_names: &[String]) -> Option<IndexedJoinProbe> {
    // RIGHT/FULL joins need the unmatched-right bookkeeping that a per-left
    // probe complicates; keep them on the scan path. CROSS has no ON clause.
    if !matches!(j.kind, JoinKind::Inner | JoinKind::Left) {
        return None;
    }
    let on = j.on.as_ref()?;
    let Expr::Binary { op: BinaryOp::Eq, left, right } = on else {
        return None;
    };
    let table = db.table(&j.table.name)?;
    let right_qual = j.table.qualifier();

    // Identify which operand is the right (inner) column and which is the left.
    let try_dir = |a: &Expr, b: &Expr| -> Option<IndexedJoinProbe> {
        // `a` must be the inner (right) table's column, `b` a left column.
        let right_col = column_ref_of_table(a, right_qual, table)?;
        table.index_on(right_col)?;
        let left_col = resolve_left_column(b, left_names)?;
        Some(IndexedJoinProbe { table: j.table.name.clone(), right_col, left_col })
    };
    try_dir(left, right).or_else(|| try_dir(right, left))
}

/// If `expr` names a column of `table` (qualified by `qual` or bare), return
/// its column index within that table.
fn column_ref_of_table(expr: &Expr, qual: &str, table: &Table) -> Option<usize> {
    match expr {
        Expr::QualifiedColumn { qualifier, name } if qualifier == qual => table.column_index(name),
        // A bare column resolves only if it is unambiguously in this table.
        Expr::Column(name) => table.column_index(name),
        _ => None,
    }
}

/// Resolve a join-key expression to an index into the current left schema.
fn resolve_left_column(expr: &Expr, left_names: &[String]) -> Option<usize> {
    match expr {
        Expr::Column(name) => resolve_column(left_names, None, name).ok(),
        Expr::QualifiedColumn { qualifier, name } => {
            resolve_column(left_names, Some(qualifier), name).ok()
        }
        _ => None,
    }
}

/// Resolve one table reference (real or a virtual catalog table) into its
/// qualified column names, types, and rows.
fn resolve_source_table(
    db: &Database,
    tref: &TableRef,
) -> Result<(Vec<String>, Vec<DataType>, Vec<Vec<Value>>), String> {
    if tref.schema.as_deref() == Some("information_schema") {
        return virtual_information_schema(db, &tref.name, tref.qualifier());
    }
    if tref.schema.as_deref() == Some("pg_catalog") || is_pg_catalog_table(&tref.name) {
        return virtual_pg_catalog(db, &tref.name, tref.qualifier());
    }
    let table = db
        .table(&tref.name)
        .ok_or_else(|| format!("relation \"{}\" does not exist", tref.name))?;
    let mut names = Vec::new();
    let mut types = Vec::new();
    for c in &table.columns {
        names.push(format!("{}.{}", tref.qualifier(), c.name));
        types.push(c.data_type);
    }
    Ok((names, types, table.rows.clone()))
}

// --- index planning ----------------------------------------------------------

/// An access path an index can satisfy for a single column.
enum IndexPlan {
    /// `col = value`.
    Eq(Value),
    /// `col IN (v1, v2, ...)`.
    In(Vec<Value>),
    /// A (possibly half-open) range; bounds carry inclusivity.
    Range(Option<Bound>, Option<Bound>),
}

/// Inspect a WHERE predicate for an index-eligible access path on a single
/// column of `target`. Returns the column index plus the plan, or `None` to
/// fall back to a full scan.
///
/// Only the *outermost* shape is considered, plus AND-conjuncts (we may use one
/// conjunct's index and re-check the whole predicate afterward). The executor
/// always re-evaluates the original filter on the candidate rows, so an
/// over-broad plan can never return wrong rows — only a slower-than-ideal one.
fn plan_index_access(filter: &Expr, target: &Table) -> Option<(usize, IndexPlan)> {
    match filter {
        // `col = const` (either operand order). Only a constant RHS qualifies.
        Expr::Binary { op: BinaryOp::Eq, left, right } => {
            if let (Some(col), Some(v)) = (column_index_of(left, target), const_value(right)) {
                return Some((col, IndexPlan::Eq(v)));
            }
            if let (Some(col), Some(v)) = (column_index_of(right, target), const_value(left)) {
                return Some((col, IndexPlan::Eq(v)));
            }
            None
        }
        // Range comparisons: `col < c`, `c > col`, etc.
        Expr::Binary { op: op @ (BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq), left, right } => {
            if let (Some(col), Some(v)) = (column_index_of(left, target), const_value(right)) {
                return Some((col, range_from_op(*op, v)));
            }
            if let (Some(col), Some(v)) = (column_index_of(right, target), const_value(left)) {
                // Flip the operator since the column is on the right.
                return Some((col, range_from_op(flip_op(*op), v)));
            }
            None
        }
        // `col IN (consts...)` — all list elements must be constant.
        Expr::InList { expr, list, negated: false } => {
            let col = column_index_of(expr, target)?;
            let mut vals = Vec::with_capacity(list.len());
            for item in list {
                vals.push(const_value(item)?);
            }
            Some((col, IndexPlan::In(vals)))
        }
        // `col BETWEEN lo AND hi` — inclusive range on both ends.
        Expr::Between { expr, low, high, negated: false } => {
            let col = column_index_of(expr, target)?;
            let lo = const_value(low)?;
            let hi = const_value(high)?;
            Some((
                col,
                IndexPlan::Range(
                    Some(Bound { value: lo, inclusive: true }),
                    Some(Bound { value: hi, inclusive: true }),
                ),
            ))
        }
        // AND: try each side; the first index-eligible conjunct wins.
        Expr::Binary { op: BinaryOp::And, left, right } => {
            plan_index_access(left, target).or_else(|| plan_index_access(right, target))
        }
        _ => None,
    }
}

/// Resolve an expression that is a (possibly qualified) column reference to its
/// column index within `target`, or `None` if it isn't a plain column of it.
fn column_index_of(expr: &Expr, target: &Table) -> Option<usize> {
    match expr {
        Expr::Column(name) => target.column_index(name),
        // A qualifier is accepted regardless of value: a single-table scan has
        // exactly one source, so any qualifier must refer to it.
        Expr::QualifiedColumn { name, .. } => target.column_index(name),
        _ => None,
    }
}

/// Evaluate an expression that must be a constant (no column references), used
/// for the right-hand side of an indexable predicate.
fn const_value(expr: &Expr) -> Option<Value> {
    match expr {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Null => {
            eval_expr(expr, &[], &[]).ok()
        }
        // Casts/negation of constants are still constants (e.g. `-5`, `'5'::int`).
        Expr::Unary { .. } | Expr::Cast { .. } => eval_expr(expr, &[], &[]).ok(),
        _ => None,
    }
}

/// Build a `Range` plan from a comparison operator and bound value.
fn range_from_op(op: BinaryOp, v: Value) -> IndexPlan {
    match op {
        BinaryOp::Lt => IndexPlan::Range(None, Some(Bound { value: v, inclusive: false })),
        BinaryOp::LtEq => IndexPlan::Range(None, Some(Bound { value: v, inclusive: true })),
        BinaryOp::Gt => IndexPlan::Range(Some(Bound { value: v, inclusive: false }), None),
        BinaryOp::GtEq => IndexPlan::Range(Some(Bound { value: v, inclusive: true }), None),
        _ => unreachable!("range_from_op called with non-range operator"),
    }
}

/// Mirror a comparison operator when its operands are swapped (`c < col`
/// becomes `col > c`).
fn flip_op(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        other => other,
    }
}

/// Candidate row positions for a filter, using an index when one applies.
///
/// Used by UPDATE/DELETE (which then re-check the full predicate). Returns a
/// position list; positions index into `table.rows`. When no index applies (or
/// there is no filter), returns all positions — i.e. the full scan.
fn candidate_positions(
    table: &Table,
    filter: &Option<Expr>,
    _col_names: &[String],
) -> Result<Vec<usize>, String> {
    if let Some(pred) = filter {
        if let Some(positions) = index_candidate_positions(table, pred) {
            return Ok(positions);
        }
    }
    Ok((0..table.rows.len()).collect())
}

/// If `filter` yields an index plan over `table`, execute it and return the
/// matching row positions. `None` means no usable index → caller full-scans.
fn index_candidate_positions(table: &Table, filter: &Expr) -> Option<Vec<usize>> {
    let (col, plan) = plan_index_access(filter, table)?;
    // Only proceed if an index actually exists on that column.
    table.index_on(col)?;
    let positions = match plan {
        IndexPlan::Eq(v) => table.index_eq(col, &v)?,
        IndexPlan::In(vals) => {
            let mut all = Vec::new();
            for v in &vals {
                if let Some(p) = table.index_eq(col, v) {
                    all.extend(p);
                }
            }
            all
        }
        IndexPlan::Range(lo, hi) => table.index_range(col, lo, hi)?,
    };
    // Distinct `IN` values (e.g. from a subquery) can map to the same row, so
    // deduplicate positions to avoid emitting a row more than once.
    let mut seen = std::collections::HashSet::new();
    Some(positions.into_iter().filter(|p| seen.insert(*p)).collect())
}

/// Generate the rows of a supported `information_schema` view from the live
/// schema, so tools and ORMs can introspect tables and columns.
fn virtual_information_schema(
    db: &Database,
    name: &str,
    qualifier: &str,
) -> Result<(Vec<String>, Vec<DataType>, Vec<Vec<Value>>), String> {
    let txt = |s: &str| Value::Text(s.to_string());
    match name.to_ascii_lowercase().as_str() {
        "tables" => {
            let cols = [
                ("table_catalog", DataType::Text),
                ("table_schema", DataType::Text),
                ("table_name", DataType::Text),
                ("table_type", DataType::Text),
            ];
            let rows = db
                .table_names()
                .into_iter()
                .map(|t| vec![txt("postgres"), txt("public"), Value::Text(t), txt("BASE TABLE")])
                .collect();
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "columns" => {
            let cols = [
                ("table_catalog", DataType::Text),
                ("table_schema", DataType::Text),
                ("table_name", DataType::Text),
                ("column_name", DataType::Text),
                ("ordinal_position", DataType::Int4),
                ("data_type", DataType::Text),
                ("is_nullable", DataType::Text),
            ];
            let mut rows = Vec::new();
            for t in db.table_names() {
                if let Some(table) = db.table(&t) {
                    for (i, c) in table.columns.iter().enumerate() {
                        rows.push(vec![
                            txt("postgres"),
                            txt("public"),
                            Value::Text(t.clone()),
                            Value::Text(c.name.clone()),
                            Value::Int(i as i64 + 1),
                            Value::Text(c.data_type.sql_name().to_string()),
                            txt(if c.not_null { "NO" } else { "YES" }),
                        ]);
                    }
                }
            }
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        other => Err(format!("information_schema.{other} is not supported")),
    }
}

/// Whether a bare table name refers to a supported `pg_catalog` relation
/// (clients sometimes reference these unqualified).
fn is_pg_catalog_table(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "pg_class" | "pg_namespace" | "pg_am")
}

/// OID assigned to the `public` namespace (matches real PostgreSQL).
const PUBLIC_NAMESPACE_OID: i64 = 2200;
/// Base OID for synthesized user-table OIDs.
const USER_TABLE_OID_BASE: i64 = 16384;

/// Generate the supported `pg_catalog` relations from the live schema, enough
/// for `psql`'s `\dt` to list tables.
fn virtual_pg_catalog(
    db: &Database,
    name: &str,
    qualifier: &str,
) -> Result<(Vec<String>, Vec<DataType>, Vec<Vec<Value>>), String> {
    match name.to_ascii_lowercase().as_str() {
        "pg_class" => {
            let cols = [
                ("oid", DataType::Int8),
                ("relname", DataType::Text),
                ("relnamespace", DataType::Int8),
                ("relkind", DataType::Text),
                ("relowner", DataType::Int8),
                ("relam", DataType::Int8),
            ];
            let rows = db
                .table_names()
                .into_iter()
                .enumerate()
                .map(|(i, t)| {
                    vec![
                        Value::Int(USER_TABLE_OID_BASE + i as i64),
                        Value::Text(t),
                        Value::Int(PUBLIC_NAMESPACE_OID),
                        Value::Text("r".to_string()), // ordinary table
                        Value::Int(10),               // owner oid (= postgres)
                        Value::Int(0),                // access method
                    ]
                })
                .collect();
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_namespace" => {
            let cols = [("oid", DataType::Int8), ("nspname", DataType::Text)];
            let rows = vec![
                vec![Value::Int(PUBLIC_NAMESPACE_OID), Value::Text("public".into())],
                vec![Value::Int(11), Value::Text("pg_catalog".into())],
                vec![Value::Int(99), Value::Text("information_schema".into())],
            ];
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_am" => {
            // Access methods: empty is fine (referenced only via LEFT JOIN).
            let cols = [("oid", DataType::Int8), ("amname", DataType::Text)];
            Ok(qualify_virtual(qualifier, &cols, Vec::new()))
        }
        other => Err(format!("pg_catalog.{other} is not supported")),
    }
}

/// Build the (qualified names, types) for a virtual table's column spec.
fn qualify_virtual(
    qualifier: &str,
    cols: &[(&str, DataType)],
    rows: Vec<Vec<Value>>,
) -> (Vec<String>, Vec<DataType>, Vec<Vec<Value>>) {
    let names = cols.iter().map(|(n, _)| format!("{qualifier}.{n}")).collect();
    let types = cols.iter().map(|(_, t)| *t).collect();
    (names, types, rows)
}

/// The bare column name from a possibly-qualified `qualifier.name` string.
fn bare_name(qualified: &str) -> String {
    match qualified.rsplit_once('.') {
        Some((_, name)) => name.to_string(),
        None => qualified.to_string(),
    }
}

/// Resolve a (possibly qualified) column reference to its index in `col_names`,
/// where stored names may be qualified (`qual.name`) or bare (`name`).
fn resolve_column(
    col_names: &[String],
    qualifier: Option<&str>,
    name: &str,
) -> Result<usize, String> {
    let mut found: Option<usize> = None;
    let mut ambiguous = false;
    for (i, c) in col_names.iter().enumerate() {
        let (cq, cn) = match c.rsplit_once('.') {
            Some((q, n)) => (Some(q), n),
            None => (None, c.as_str()),
        };
        let matches = match qualifier {
            // Qualified ref: require the qualifier to match, but tolerate
            // bare-stored names (single-table queries) by matching on name.
            Some(q) => (cq == Some(q) && cn == name) || (cq.is_none() && cn == name),
            // Unqualified ref: match on the bare name.
            None => cn == name,
        };
        if matches {
            if found.is_some() {
                ambiguous = true;
            }
            found = Some(i);
        }
    }
    match (found, ambiguous) {
        (Some(_), true) => Err(format!("column reference \"{name}\" is ambiguous")),
        (Some(i), false) => Ok(i),
        (None, _) => Err(format!("column \"{name}\" does not exist")),
    }
}

fn exec_create_table(db: &mut Database, c: CreateTable) -> Result<ExecResult, String> {
    if db.contains_table(&c.name) {
        if c.if_not_exists {
            return Ok(ExecResult::Command("CREATE TABLE".into()));
        }
        return Err(format!("relation \"{}\" already exists", c.name));
    }
    let columns: Vec<Column> = c
        .columns
        .into_iter()
        .map(|cd| Column {
            name: cd.name,
            data_type: cd.data_type,
            not_null: cd.not_null,
            primary_key: cd.primary_key,
            default: cd.default,
            serial: cd.serial,
        })
        .collect();
    // Auto-create a unique index for each PRIMARY KEY column so point lookups
    // on it are fast out of the box (mirrors PostgreSQL's implicit pkey index).
    let pk_indexes: Vec<(usize, String)> = columns
        .iter()
        .enumerate()
        .filter(|(_, col)| col.primary_key)
        .map(|(i, col)| (i, format!("{}_{}_pkey", c.name, col.name)))
        .collect();
    let mut table = Table::new(c.name.clone(), columns);
    for (col_idx, name) in pk_indexes {
        table.create_index(name, col_idx, true);
    }
    db.create_table(table)?;
    Ok(ExecResult::Command("CREATE TABLE".into()))
}

fn exec_create_index(db: &mut Database, c: CreateIndex) -> Result<ExecResult, String> {
    let table = db
        .table_mut(&c.table)
        .ok_or_else(|| format!("relation \"{}\" does not exist", c.table))?;
    let column = table
        .column_index(&c.column)
        .ok_or_else(|| format!("column \"{}\" does not exist", c.column))?;
    // Generate a deterministic name when none is given, matching PostgreSQL's
    // `<table>_<column>_idx` convention so replay is stable.
    let name = c.name.unwrap_or_else(|| format!("{}_{}_idx", c.table, c.column));
    if table.has_index_named(&name) {
        if c.if_not_exists {
            return Ok(ExecResult::Command("CREATE INDEX".into()));
        }
        return Err(format!("relation \"{name}\" already exists"));
    }
    table.create_index(name, column, c.unique);
    Ok(ExecResult::Command("CREATE INDEX".into()))
}

fn exec_drop_index(db: &mut Database, d: DropIndex) -> Result<ExecResult, String> {
    // Index names are not globally unique in our flat model, so search every
    // table for one bearing this name.
    let mut dropped = false;
    for name in db.table_names() {
        if let Some(table) = db.table_mut(&name) {
            if table.drop_index(&d.name) {
                dropped = true;
                break;
            }
        }
    }
    if !dropped && !d.if_exists {
        return Err(format!("index \"{}\" does not exist", d.name));
    }
    Ok(ExecResult::Command("DROP INDEX".into()))
}

fn exec_drop_table(db: &mut Database, d: DropTable) -> Result<ExecResult, String> {
    let existed = db.drop_table(&d.name);
    if !existed && !d.if_exists {
        return Err(format!("table \"{}\" does not exist", d.name));
    }
    Ok(ExecResult::Command("DROP TABLE".into()))
}

fn exec_alter_table(db: &mut Database, alter: AlterTable) -> Result<ExecResult, String> {
    match alter.action {
        AlterAction::RenameTable { to } => {
            db.rename_table(&alter.table, &to)?;
            return Ok(ExecResult::Command("ALTER TABLE".into()));
        }
        AlterAction::AddColumn { column, if_not_exists } => {
            // Evaluate a constant default once; serial fills per row.
            let default_val = match &column.default {
                Some(e) => coerce(eval_expr(e, &[], &[])?, column.data_type)?,
                None => Value::Null,
            };
            // Validate against the existing schema (immutable borrow first).
            {
                let table = db
                    .table(&alter.table)
                    .ok_or_else(|| format!("relation \"{}\" does not exist", alter.table))?;
                if table.columns.iter().any(|c| c.name == column.name) {
                    if if_not_exists {
                        return Ok(ExecResult::Command("ALTER TABLE".into()));
                    }
                    return Err(format!(
                        "column \"{}\" of relation \"{}\" already exists",
                        column.name, alter.table
                    ));
                }
                // A NOT NULL column with no default can't be added to a
                // non-empty table (existing rows would violate it).
                if column.not_null && column.default.is_none() && !column.serial && !table.rows.is_empty() {
                    return Err(format!("column \"{}\" contains null values", column.name));
                }
            }
            let col = Column {
                name: column.name.clone(),
                data_type: column.data_type,
                not_null: column.not_null,
                primary_key: column.primary_key,
                default: column.default.clone(),
                serial: column.serial,
            };
            if column.serial {
                // Pre-compute one sequence value per existing row, then fill.
                let key = format!("{}.{}", alter.table, column.name);
                let n = db.table(&alter.table).map(|t| t.rows.len()).unwrap_or(0);
                let fills: Vec<Value> = (0..n).map(|_| Value::Int(db.next_sequence(&key))).collect();
                let table = db.table_mut(&alter.table).unwrap();
                table.add_column(col, &|pos| fills[pos].clone());
            } else {
                let table = db.table_mut(&alter.table).unwrap();
                table.add_column(col, &|_| default_val.clone());
            }
            Ok(ExecResult::Command("ALTER TABLE".into()))
        }
        AlterAction::DropColumn { name, if_exists } => {
            let table = db
                .table_mut(&alter.table)
                .ok_or_else(|| format!("relation \"{}\" does not exist", alter.table))?;
            match table.column_index(&name) {
                Some(idx) => {
                    table.drop_column(idx);
                    Ok(ExecResult::Command("ALTER TABLE".into()))
                }
                None if if_exists => Ok(ExecResult::Command("ALTER TABLE".into())),
                None => Err(format!("column \"{name}\" of relation \"{}\" does not exist", alter.table)),
            }
        }
        AlterAction::RenameColumn { from, to } => {
            let table = db
                .table_mut(&alter.table)
                .ok_or_else(|| format!("relation \"{}\" does not exist", alter.table))?;
            match table.column_index(&from) {
                Some(idx) => {
                    table.columns[idx].name = to;
                    Ok(ExecResult::Command("ALTER TABLE".into()))
                }
                None => Err(format!("column \"{from}\" of relation \"{}\" does not exist", alter.table)),
            }
        }
    }
}

fn exec_insert(db: &mut Database, ins: Insert) -> Result<ExecResult, String> {
    // Resolve schema first (immutable borrow), then mutate.
    let table = db
        .table(&ins.table)
        .ok_or_else(|| format!("relation \"{}\" does not exist", ins.table))?;
    let columns = table.columns.clone();

    // Map each VALUES position to a target column index.
    let target_indices: Vec<usize> = match &ins.columns {
        Some(names) => {
            let mut idx = Vec::with_capacity(names.len());
            for n in names {
                let i = columns
                    .iter()
                    .position(|c| &c.name == n)
                    .ok_or_else(|| format!("column \"{n}\" of relation \"{}\" does not exist", ins.table))?;
                idx.push(i);
            }
            idx
        }
        None => (0..columns.len()).collect(),
    };

    let mut new_rows = Vec::with_capacity(ins.rows.len());
    for tuple in &ins.rows {
        if tuple.len() != target_indices.len() {
            return Err(format!(
                "INSERT has {} expressions but {} target columns",
                tuple.len(),
                target_indices.len()
            ));
        }
        // Start with all-NULL, fill specified columns.
        let mut row = vec![Value::Null; columns.len()];
        for (expr, &col_idx) in tuple.iter().zip(&target_indices) {
            let val = eval_expr(expr, &[], &[])?;
            row[col_idx] = coerce(val, columns[col_idx].data_type)?;
        }
        // Apply DEFAULTs for columns the INSERT didn't target.
        for (i, col) in columns.iter().enumerate() {
            if !target_indices.contains(&i) {
                if let Some(default) = &col.default {
                    let val = eval_expr(default, &[], &[])?;
                    row[i] = coerce(val, col.data_type)?;
                }
            }
        }
        // serial columns: fill NULLs from the sequence; advance the sequence
        // past any explicitly-inserted value to avoid future collisions.
        for (i, col) in columns.iter().enumerate() {
            if !col.serial {
                continue;
            }
            let key = format!("{}.{}", ins.table, col.name);
            match row[i] {
                Value::Int(v) => db.observe_sequence(&key, v),
                Value::Null => row[i] = Value::Int(db.next_sequence(&key)),
                _ => {}
            }
        }
        // Enforce NOT NULL.
        for (i, col) in columns.iter().enumerate() {
            if col.not_null && row[i].is_null() {
                return Err(format!(
                    "null value in column \"{}\" violates not-null constraint",
                    col.name
                ));
            }
        }
        new_rows.push(row);
    }

    // Enforce unique constraints atomically: check all new rows against
    // existing data and against each other before inserting any.
    {
        let table = db.table(&ins.table).expect("table existed above");
        for row in &new_rows {
            if let Some(name) = table.unique_violation(row, None) {
                return Err(format!("duplicate key value violates unique constraint \"{name}\""));
            }
        }
        for col in table.unique_index_columns() {
            let mut seen = std::collections::HashSet::new();
            for row in &new_rows {
                if let Some(key) = row[col].to_text() {
                    if !seen.insert(key) {
                        return Err("duplicate key value violates unique constraint".into());
                    }
                }
            }
        }
    }

    let n = new_rows.len();
    // PostgreSQL tag form is "INSERT <oid> <count>"; oid is always 0 now.
    let tag = format!("INSERT 0 {n}");
    let result = returning_result(&ins.returning, &columns, &new_rows, tag)?;
    let table = db.table_mut(&ins.table).expect("table existed above");
    // `push_row` assigns each row a stable id and maintains every index.
    for row in new_rows {
        table.push_row(row);
    }
    Ok(result)
}

/// Build the result of a mutating statement: a `RETURNING` row set when
/// `returning` is non-empty, otherwise just the command tag.
fn returning_result(
    returning: &[SelectItem],
    columns: &[Column],
    affected: &[Vec<Value>],
    tag: String,
) -> Result<ExecResult, String> {
    if returning.is_empty() {
        return Ok(ExecResult::Command(tag));
    }
    let col_names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
    let col_types: Vec<DataType> = columns.iter().map(|c| c.data_type).collect();
    let (fields, rows) = project_rows(returning, &col_names, &col_types, affected)?;
    Ok(ExecResult::Rows { fields, rows, tag })
}

/// Evaluate a select list against a set of rows, producing output fields and
/// values. Used by `RETURNING`.
fn project_rows(
    items: &[SelectItem],
    col_names: &[String],
    col_types: &[DataType],
    rows: &[Vec<Value>],
) -> Result<(Vec<FieldDescription>, Vec<Vec<Value>>), String> {
    let mut fields = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard => {
                for (i, name) in col_names.iter().enumerate() {
                    fields.push(FieldDescription { name: bare_name(name), data_type: col_types[i] });
                }
            }
            SelectItem::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| default_column_name(expr));
                fields.push(FieldDescription { name, data_type: infer_expr_type(expr, col_names, col_types) });
            }
        }
    }
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut o = Vec::new();
        for item in items {
            match item {
                SelectItem::Wildcard => o.extend(row.iter().cloned()),
                SelectItem::Expr { expr, .. } => o.push(eval_expr(expr, col_names, row)?),
            }
        }
        out.push(o);
    }
    Ok((fields, out))
}

/// Resolve every uncorrelated subquery within a SELECT's clauses.
fn resolve_subqueries_in_select(db: &mut Database, sel: &mut Select) -> Result<(), String> {
    for item in &mut sel.projection {
        if let SelectItem::Expr { expr, .. } = item {
            resolve_subqueries(db, expr)?;
        }
    }
    if let Some(f) = &mut sel.filter {
        resolve_subqueries(db, f)?;
    }
    if let Some(h) = &mut sel.having {
        resolve_subqueries(db, h)?;
    }
    for g in &mut sel.group_by {
        resolve_subqueries(db, g)?;
    }
    for ob in &mut sel.order_by {
        resolve_subqueries(db, &mut ob.expr)?;
    }
    if let Some(l) = &mut sel.limit {
        resolve_subqueries(db, l)?;
    }
    if let Some(o) = &mut sel.offset {
        resolve_subqueries(db, o)?;
    }
    Ok(())
}

/// Execute uncorrelated subqueries in `expr` once and replace them with the
/// resulting literal (scalar), value-list (`IN`), or boolean (`EXISTS`). A
/// correlated subquery (referencing an outer column) will surface as a
/// "column does not exist" error when executed standalone — they are not yet
/// supported.
fn resolve_subqueries(db: &mut Database, expr: &mut Expr) -> Result<(), String> {
    match expr {
        Expr::ScalarSubquery(sel) => {
            let v = exec_scalar_subquery(db, sel)?;
            *expr = value_to_literal(v);
        }
        Expr::Exists(sel) => {
            let has_rows = subquery_row_count(db, sel)? > 0;
            *expr = Expr::Bool(has_rows);
        }
        Expr::InSubquery { expr: inner, subquery, negated } => {
            resolve_subqueries(db, inner)?;
            let values = subquery_single_column(db, subquery)?;
            let list = values.into_iter().map(value_to_literal).collect();
            *expr = Expr::InList { expr: inner.clone(), list, negated: *negated };
        }
        Expr::Unary { expr, .. } => resolve_subqueries(db, expr)?,
        Expr::Binary { left, right, .. } => {
            resolve_subqueries(db, left)?;
            resolve_subqueries(db, right)?;
        }
        Expr::IsNull { expr, .. } | Expr::Cast { expr, .. } => resolve_subqueries(db, expr)?,
        Expr::Like { expr, pattern, .. } => {
            resolve_subqueries(db, expr)?;
            resolve_subqueries(db, pattern)?;
        }
        Expr::InList { expr, list, .. } => {
            resolve_subqueries(db, expr)?;
            for e in list {
                resolve_subqueries(db, e)?;
            }
        }
        Expr::Between { expr, low, high, .. } => {
            resolve_subqueries(db, expr)?;
            resolve_subqueries(db, low)?;
            resolve_subqueries(db, high)?;
        }
        Expr::Case { operand, whens, else_expr } => {
            if let Some(o) = operand {
                resolve_subqueries(db, o)?;
            }
            for (c, r) in whens {
                resolve_subqueries(db, c)?;
                resolve_subqueries(db, r)?;
            }
            if let Some(e) = else_expr {
                resolve_subqueries(db, e)?;
            }
        }
        Expr::Function { args, .. } => {
            for a in args {
                resolve_subqueries(db, a)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Run a subquery expected to yield a single value (zero rows → NULL).
fn exec_scalar_subquery(db: &mut Database, sel: &Select) -> Result<Value, String> {
    match exec_select(db, sel.clone())? {
        ExecResult::Rows { fields, mut rows, .. } => {
            if fields.len() != 1 {
                return Err("subquery must return only one column".into());
            }
            if rows.len() > 1 {
                return Err("more than one row returned by a subquery used as an expression".into());
            }
            Ok(rows.pop().map(|mut r| r.remove(0)).unwrap_or(Value::Null))
        }
        _ => Err("subquery did not return a result set".into()),
    }
}

/// Number of rows a subquery yields (for `EXISTS`).
fn subquery_row_count(db: &mut Database, sel: &Select) -> Result<usize, String> {
    match exec_select(db, sel.clone())? {
        ExecResult::Rows { rows, .. } => Ok(rows.len()),
        _ => Ok(0),
    }
}

/// Collect a single-column subquery's values (for `IN (SELECT ...)`).
fn subquery_single_column(db: &mut Database, sel: &Select) -> Result<Vec<Value>, String> {
    match exec_select(db, sel.clone())? {
        ExecResult::Rows { fields, rows, .. } => {
            if fields.len() != 1 {
                return Err("subquery must return only one column".into());
            }
            Ok(rows.into_iter().map(|mut r| r.remove(0)).collect())
        }
        _ => Err("subquery did not return a result set".into()),
    }
}

fn value_to_literal(v: Value) -> Expr {
    match v {
        Value::Null => Expr::Null,
        Value::Int(i) => Expr::Int(i),
        Value::Float(f) => Expr::Float(f),
        Value::Text(s) => Expr::Str(s),
        Value::Bool(b) => Expr::Bool(b),
    }
}

fn exec_select(db: &mut Database, mut sel: Select) -> Result<ExecResult, String> {
    // Execute uncorrelated subqueries first, splicing their results in as
    // literals/value-lists so the row-evaluation and index-planning paths
    // never see a subquery.
    resolve_subqueries_in_select(db, &mut sel)?;

    // Resolve the source: the (possibly joined) FROM rows with qualified
    // column names, or a single synthetic empty row for `SELECT <exprs>`.
    let (col_names, col_types, source_rows) = match &sel.from {
        Some(fc) => build_source(db, fc, sel.filter.as_ref())?,
        None => (Vec::new(), Vec::new(), vec![Vec::new()]),
    };

    // Apply WHERE.
    let mut rows: Vec<Vec<Value>> = Vec::new();
    for row in &source_rows {
        let keep = match &sel.filter {
            Some(pred) => eval_expr(pred, &col_names, row)?.is_true(),
            None => true,
        };
        if keep {
            rows.push(row.clone());
        }
    }

    // Grouped/aggregate path: triggered by GROUP BY, an aggregate in the
    // projection, or an aggregate in HAVING.
    let has_agg = sel.projection.iter().any(|item| match item {
        SelectItem::Expr { expr, .. } => contains_aggregate(expr),
        SelectItem::Wildcard => false,
    }) || sel.having.as_ref().is_some_and(contains_aggregate);

    if !sel.group_by.is_empty() || has_agg {
        return grouped_select(&sel, &col_names, &col_types, &rows);
    }

    // Build the output fields and the per-column "producers" once.
    let mut fields: Vec<FieldDescription> = Vec::new();
    let mut producers: Vec<Producer> = Vec::new();
    for item in &sel.projection {
        match item {
            SelectItem::Wildcard => {
                for (i, name) in col_names.iter().enumerate() {
                    producers.push(Producer::Col(i));
                    fields.push(FieldDescription { name: bare_name(name), data_type: col_types[i] });
                }
            }
            SelectItem::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| default_column_name(expr));
                let dt = infer_expr_type(expr, &col_names, &col_types);
                producers.push(Producer::Expr(expr.clone()));
                fields.push(FieldDescription { name, data_type: dt });
            }
        }
    }

    // Project each input row, keeping input + output side by side so ORDER BY
    // can reference either input columns or output aliases.
    let mut combined: Vec<(Vec<Value>, Vec<Value>)> = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut out = Vec::with_capacity(producers.len());
        for p in &producers {
            match p {
                Producer::Col(i) => out.push(row[*i].clone()),
                Producer::Expr(e) => out.push(eval_expr(e, &col_names, row)?),
            }
        }
        combined.push((row.clone(), out));
    }

    // DISTINCT: drop later duplicates of the projected row (order-preserving).
    if sel.distinct {
        let mut seen: Vec<Vec<Value>> = Vec::new();
        combined.retain(|(_, out)| {
            if seen.iter().any(|s| s == out) {
                false
            } else {
                seen.push(out.clone());
                true
            }
        });
    }

    // ORDER BY.
    if !sel.order_by.is_empty() {
        let mut sort_keys: Vec<Vec<Value>> = Vec::with_capacity(combined.len());
        for (input, output) in &combined {
            let mut key = Vec::with_capacity(sel.order_by.len());
            for ob in &sel.order_by {
                let v = if let Some(i) = positional_index(&ob.expr, output.len())? {
                    output[i].clone()
                } else if let Some(i) = output_column_index(&ob.expr, &fields) {
                    output[i].clone()
                } else {
                    eval_expr(&ob.expr, &col_names, input)?
                };
                key.push(v);
            }
            sort_keys.push(key);
        }
        let mut idx: Vec<usize> = (0..combined.len()).collect();
        idx.sort_by(|&a, &b| {
            for (i, item) in sel.order_by.iter().enumerate() {
                let ord = compare_values(&sort_keys[a][i], &sort_keys[b][i]).unwrap_or(Ordering::Equal);
                let ord = if item.asc { ord } else { ord.reverse() };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });
        combined = idx.into_iter().map(|i| std::mem::take(&mut combined[i])).collect();
    }

    // OFFSET / LIMIT.
    let offset = eval_count(&sel.offset, &col_names)?.unwrap_or(0);
    let limit = eval_count(&sel.limit, &col_names)?;
    let start = offset.min(combined.len());
    let end = match limit {
        Some(l) => (start + l).min(combined.len()),
        None => combined.len(),
    };
    let out_rows: Vec<Vec<Value>> = combined[start..end].iter().map(|(_, o)| o.clone()).collect();
    let tag = format!("SELECT {}", out_rows.len());
    Ok(ExecResult::Rows { fields, rows: out_rows, tag })
}

/// A single output column's source within the scalar SELECT path.
enum Producer {
    /// Copy input column at this index.
    Col(usize),
    /// Evaluate this expression against the input row.
    Expr(Expr),
}

/// If `expr` is a bare column name matching an output field, return its index.
/// This lets `ORDER BY` reference a SELECT-list alias.
fn output_column_index(expr: &Expr, fields: &[FieldDescription]) -> Option<usize> {
    match expr {
        Expr::Column(name) => fields.iter().position(|f| &f.name == name),
        _ => None,
    }
}

/// If `expr` is an integer literal `n`, interpret it as a 1-based output
/// column position (`ORDER BY 1`), returning its 0-based index.
fn positional_index(expr: &Expr, num_cols: usize) -> Result<Option<usize>, String> {
    if let Expr::Int(n) = expr {
        let n = *n;
        if n >= 1 && (n as usize) <= num_cols {
            Ok(Some(n as usize - 1))
        } else {
            Err(format!("ORDER BY position {n} is not in select list"))
        }
    } else {
        Ok(None)
    }
}

/// Grouped aggregation. With no `GROUP BY` the whole (filtered) set is one
/// group (so `SELECT count(*) FROM empty` still yields a single `0` row).
fn grouped_select(
    sel: &Select,
    col_names: &[String],
    col_types: &[DataType],
    rows: &[Vec<Value>],
) -> Result<ExecResult, String> {
    // Partition rows into groups, preserving first-seen order.
    let groups: Vec<Vec<Vec<Value>>> = if sel.group_by.is_empty() {
        vec![rows.to_vec()]
    } else {
        let mut keys: Vec<Vec<Value>> = Vec::new();
        let mut buckets: Vec<Vec<Vec<Value>>> = Vec::new();
        for row in rows {
            let mut key = Vec::with_capacity(sel.group_by.len());
            for g in &sel.group_by {
                key.push(eval_expr(g, col_names, row)?);
            }
            match keys.iter().position(|k| k == &key) {
                Some(i) => buckets[i].push(row.clone()),
                None => {
                    keys.push(key);
                    buckets.push(vec![row.clone()]);
                }
            }
        }
        buckets
    };

    // Output fields from the projection.
    let mut fields = Vec::new();
    for item in &sel.projection {
        match item {
            SelectItem::Wildcard => {
                return Err("cannot use * with GROUP BY or aggregate functions".into());
            }
            SelectItem::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| default_column_name(expr));
                let data_type = infer_expr_type(expr, col_names, col_types);
                fields.push(FieldDescription { name, data_type });
            }
        }
    }

    // One output row per surviving group, carrying ORDER BY sort keys.
    let mut keyed: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    for group in &groups {
        if let Some(h) = &sel.having {
            if !eval_aggregate_expr(h, col_names, group)?.is_true() {
                continue;
            }
        }
        let mut out = Vec::with_capacity(sel.projection.len());
        for item in &sel.projection {
            if let SelectItem::Expr { expr, .. } = item {
                out.push(eval_aggregate_expr(expr, col_names, group)?);
            }
        }
        let mut sort_key = Vec::with_capacity(sel.order_by.len());
        for ob in &sel.order_by {
            // ORDER BY may use a position, an output alias, or an expression.
            let v = if let Some(i) = positional_index(&ob.expr, out.len())? {
                out[i].clone()
            } else if let Some(i) = output_column_index(&ob.expr, &fields) {
                out[i].clone()
            } else {
                eval_aggregate_expr(&ob.expr, col_names, group)?
            };
            sort_key.push(v);
        }
        keyed.push((sort_key, out));
    }

    // DISTINCT over the grouped output rows.
    if sel.distinct {
        let mut seen: Vec<Vec<Value>> = Vec::new();
        keyed.retain(|(_, out)| {
            if seen.iter().any(|s| s == out) {
                false
            } else {
                seen.push(out.clone());
                true
            }
        });
    }

    // ORDER BY over the grouped output.
    if !sel.order_by.is_empty() {
        keyed.sort_by(|a, b| {
            for (i, item) in sel.order_by.iter().enumerate() {
                let ord = compare_values(&a.0[i], &b.0[i]).unwrap_or(Ordering::Equal);
                let ord = if item.asc { ord } else { ord.reverse() };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });
    }

    let out_rows: Vec<Vec<Value>> = keyed.into_iter().map(|(_, r)| r).collect();

    // OFFSET / LIMIT.
    let offset = eval_count(&sel.offset, col_names)?.unwrap_or(0);
    let limit = eval_count(&sel.limit, col_names)?;
    let start = offset.min(out_rows.len());
    let end = match limit {
        Some(l) => (start + l).min(out_rows.len()),
        None => out_rows.len(),
    };
    let final_rows = out_rows[start..end].to_vec();
    let tag = format!("SELECT {}", final_rows.len());
    Ok(ExecResult::Rows { fields, rows: final_rows, tag })
}

fn exec_update(db: &mut Database, mut upd: Update) -> Result<ExecResult, String> {
    // Resolve any uncorrelated subqueries in SET expressions / WHERE first.
    for (_, e) in &mut upd.assignments {
        resolve_subqueries(db, e)?;
    }
    if let Some(f) = &mut upd.filter {
        resolve_subqueries(db, f)?;
    }
    let table = db
        .table(&upd.table)
        .ok_or_else(|| format!("relation \"{}\" does not exist", upd.table))?;
    let col_names = table.column_names();
    let columns = table.columns.clone();

    // Resolve assignment target indices up front.
    let mut targets = Vec::with_capacity(upd.assignments.len());
    for (name, expr) in &upd.assignments {
        let idx = columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| format!("column \"{name}\" of relation \"{}\" does not exist", upd.table))?;
        targets.push((idx, expr.clone()));
    }

    // Pick the candidate row positions: an index when the filter allows it,
    // otherwise every row. The predicate is re-checked below regardless, so
    // the index can only narrow the set, never change the result.
    let candidates = candidate_positions(table, &upd.filter, &col_names)?;

    let mut new_versions: Vec<(usize, Vec<Value>)> = Vec::new();
    let mut affected = Vec::new();
    for pos in candidates {
        let row = &table.rows[pos];
        let matches = match &upd.filter {
            Some(pred) => eval_expr(pred, &col_names, row)?.is_true(),
            None => true,
        };
        if !matches {
            continue;
        }
        let mut new_row = row.clone();
        for (idx, expr) in &targets {
            let val = eval_expr(expr, &col_names, &new_row)?;
            new_row[*idx] = coerce(val, columns[*idx].data_type)?;
        }
        affected.push(new_row.clone());
        new_versions.push((pos, new_row));
    }

    // Enforce unique constraints before applying any change (atomic): each new
    // row must not collide with another row (excluding its own position) or
    // with another row updated in the same statement.
    for (pos, new_row) in &new_versions {
        if let Some(name) = table.unique_violation(new_row, Some(*pos)) {
            return Err(format!("duplicate key value violates unique constraint \"{name}\""));
        }
    }
    for col in table.unique_index_columns() {
        let mut seen = std::collections::HashSet::new();
        for (_, new_row) in &new_versions {
            if let Some(key) = new_row[col].to_text() {
                if !seen.insert(key) {
                    return Err("duplicate key value violates unique constraint".into());
                }
            }
        }
    }

    let tag = format!("UPDATE {}", affected.len());
    let result = returning_result(&upd.returning, &columns, &affected, tag)?;
    let table = db.table_mut(&upd.table).expect("table existed above");
    // Apply through `update_row` so each touched index is repaired in place.
    for (pos, new_row) in new_versions {
        table.update_row(pos, new_row);
    }
    Ok(result)
}

fn exec_delete(db: &mut Database, mut del: Delete) -> Result<ExecResult, String> {
    if let Some(f) = &mut del.filter {
        resolve_subqueries(db, f)?;
    }
    let table = db
        .table(&del.table)
        .ok_or_else(|| format!("relation \"{}\" does not exist", del.table))?;
    let col_names = table.column_names();
    let columns = table.columns.clone();

    let candidates = candidate_positions(table, &del.filter, &col_names)?;
    // Build the matching positions in ascending row order so RETURNING and the
    // command tag match the full-scan path exactly.
    let mut matching = std::collections::BTreeSet::new();
    for pos in candidates {
        let matches = match &del.filter {
            Some(pred) => eval_expr(pred, &col_names, &table.rows[pos])?.is_true(),
            None => true,
        };
        if matches {
            matching.insert(pos);
        }
    }
    let positions: Vec<usize> = matching.into_iter().collect();
    let affected: Vec<Vec<Value>> = positions.iter().map(|&p| table.rows[p].clone()).collect();

    let tag = format!("DELETE {}", affected.len());
    let result = returning_result(&del.returning, &columns, &affected, tag)?;
    let table = db.table_mut(&del.table).expect("table existed above");
    table.delete_rows(&positions);
    Ok(result)
}

fn exec_show(name: String) -> Result<ExecResult, String> {
    let value = match name.to_ascii_lowercase().as_str() {
        "server_version" => "16.0 (postgres-rs)".to_string(),
        "server_encoding" | "client_encoding" => "UTF8".to_string(),
        "transaction_isolation" => "read committed".to_string(),
        _ => String::new(),
    };
    Ok(ExecResult::Rows {
        fields: vec![FieldDescription { name, data_type: DataType::Text }],
        rows: vec![vec![Value::Text(value)]],
        tag: "SHOW".to_string(),
    })
}

// --- expression evaluation ---------------------------------------------------

/// Evaluate a scalar expression against a row. `col_names`/`row` give the
/// current tuple's columns; both may be empty for constant expressions.
fn eval_expr(expr: &Expr, col_names: &[String], row: &[Value]) -> Result<Value, String> {
    match expr {
        Expr::Int(i) => Ok(Value::Int(*i)),
        Expr::Float(f) => Ok(Value::Float(*f)),
        Expr::Str(s) => Ok(Value::Text(s.clone())),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Null => Ok(Value::Null),
        Expr::Param(n) => Err(format!("unbound parameter ${n}")),
        Expr::Column(name) => {
            let idx = resolve_column(col_names, None, name)?;
            Ok(row[idx].clone())
        }
        Expr::QualifiedColumn { qualifier, name } => {
            let idx = resolve_column(col_names, Some(qualifier), name)?;
            Ok(row[idx].clone())
        }
        Expr::Unary { op, expr } => {
            let v = eval_expr(expr, col_names, row)?;
            eval_unary(*op, v)
        }
        Expr::Binary { op, left, right } => {
            let l = eval_expr(left, col_names, row)?;
            // Short-circuit boolean operators.
            match op {
                BinaryOp::And if !l.is_null() && !l.is_true() => return Ok(Value::Bool(false)),
                BinaryOp::Or if l.is_true() => return Ok(Value::Bool(true)),
                _ => {}
            }
            let r = eval_expr(right, col_names, row)?;
            eval_binary(*op, l, r)
        }
        Expr::IsNull { expr, negated } => {
            let v = eval_expr(expr, col_names, row)?;
            let is_null = v.is_null();
            Ok(Value::Bool(if *negated { !is_null } else { is_null }))
        }
        Expr::Like { expr, pattern, negated, case_insensitive } => {
            let v = eval_expr(expr, col_names, row)?;
            let p = eval_expr(pattern, col_names, row)?;
            if v.is_null() || p.is_null() {
                return Ok(Value::Null);
            }
            let (text, pat) = (v.to_text().unwrap_or_default(), p.to_text().unwrap_or_default());
            let m = like_match(&text, &pat, *case_insensitive);
            Ok(Value::Bool(if *negated { !m } else { m }))
        }
        Expr::InList { expr, list, negated } => eval_in_list(expr, list, *negated, col_names, row),
        Expr::Between { expr, low, high, negated } => {
            let v = eval_expr(expr, col_names, row)?;
            let lo = eval_expr(low, col_names, row)?;
            let hi = eval_expr(high, col_names, row)?;
            if v.is_null() || lo.is_null() || hi.is_null() {
                return Ok(Value::Null);
            }
            let ge = compare_values(&v, &lo).map(|o| o != Ordering::Less).unwrap_or(false);
            let le = compare_values(&v, &hi).map(|o| o != Ordering::Greater).unwrap_or(false);
            let within = ge && le;
            Ok(Value::Bool(if *negated { !within } else { within }))
        }
        Expr::Case { operand, whens, else_expr } => {
            let operand_val = match operand {
                Some(o) => Some(eval_expr(o, col_names, row)?),
                None => None,
            };
            for (cond, result) in whens {
                let hit = match &operand_val {
                    // Simple CASE: compare operand to each WHEN value.
                    Some(o) => {
                        let c = eval_expr(cond, col_names, row)?;
                        !o.is_null() && !c.is_null()
                            && compare_values(o, &c) == Some(Ordering::Equal)
                    }
                    // Searched CASE: each WHEN is a boolean condition.
                    None => eval_expr(cond, col_names, row)?.is_true(),
                };
                if hit {
                    return eval_expr(result, col_names, row);
                }
            }
            match else_expr {
                Some(e) => eval_expr(e, col_names, row),
                None => Ok(Value::Null),
            }
        }
        Expr::Cast { expr, target } => {
            let v = eval_expr(expr, col_names, row)?;
            coerce(v, *target)
        }
        // Uncorrelated subqueries are resolved to literals before evaluation;
        // reaching here means a correlated subquery, which is not yet supported.
        Expr::ScalarSubquery(_) | Expr::Exists(_) | Expr::InSubquery { .. } => {
            Err("correlated subqueries are not supported".into())
        }
        Expr::Function { name, args, star, .. } => {
            eval_scalar_function(name, args, *star, col_names, row)
        }
    }
}

fn eval_unary(op: UnaryOp, v: Value) -> Result<Value, String> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    match op {
        UnaryOp::Neg => match v {
            Value::Int(i) => Ok(Value::Int(-i)),
            Value::Float(f) => Ok(Value::Float(-f)),
            other => Err(format!("cannot negate {}", other.inferred_type().sql_name())),
        },
        UnaryOp::Not => Ok(Value::Bool(!v.is_true())),
    }
}

fn eval_binary(op: BinaryOp, l: Value, r: Value) -> Result<Value, String> {
    use BinaryOp::*;
    // NULL propagation for non-logical operators.
    if matches!(
        op,
        Add | Sub | Mul | Div | Mod | Concat | Eq | NotEq | Lt | LtEq | Gt | GtEq
            | RegexMatch { .. } | RegexNotMatch { .. }
    ) && (l.is_null() || r.is_null())
    {
        return Ok(Value::Null);
    }
    match op {
        Add | Sub | Mul | Div | Mod => arithmetic(op, l, r),
        RegexMatch { ci } => {
            let m = regex_match(&r.to_text().unwrap_or_default(), &l.to_text().unwrap_or_default(), ci);
            Ok(Value::Bool(m))
        }
        RegexNotMatch { ci } => {
            let m = regex_match(&r.to_text().unwrap_or_default(), &l.to_text().unwrap_or_default(), ci);
            Ok(Value::Bool(!m))
        }
        Concat => Ok(Value::Text(format!(
            "{}{}",
            l.to_text().unwrap_or_default(),
            r.to_text().unwrap_or_default()
        ))),
        Eq | NotEq | Lt | LtEq | Gt | GtEq => {
            let ord = compare_values(&l, &r)
                .ok_or_else(|| "cannot compare values of incompatible types".to_string())?;
            let b = match op {
                Eq => ord == Ordering::Equal,
                NotEq => ord != Ordering::Equal,
                Lt => ord == Ordering::Less,
                LtEq => ord != Ordering::Greater,
                Gt => ord == Ordering::Greater,
                GtEq => ord != Ordering::Less,
                _ => unreachable!(),
            };
            Ok(Value::Bool(b))
        }
        And => {
            if l.is_null() || r.is_null() {
                Ok(Value::Null)
            } else {
                Ok(Value::Bool(l.is_true() && r.is_true()))
            }
        }
        Or => {
            if l.is_null() || r.is_null() {
                Ok(Value::Null)
            } else {
                Ok(Value::Bool(l.is_true() || r.is_true()))
            }
        }
    }
}

fn arithmetic(op: BinaryOp, l: Value, r: Value) -> Result<Value, String> {
    use BinaryOp::*;
    // If either is a float, compute in float.
    let both_int = matches!(l, Value::Int(_)) && matches!(r, Value::Int(_));
    if both_int {
        let (Value::Int(a), Value::Int(b)) = (&l, &r) else { unreachable!() };
        let (a, b) = (*a, *b);
        return match op {
            Add => a.checked_add(b).map(Value::Int).ok_or_else(|| "integer out of range".into()),
            Sub => a.checked_sub(b).map(Value::Int).ok_or_else(|| "integer out of range".into()),
            Mul => a.checked_mul(b).map(Value::Int).ok_or_else(|| "integer out of range".into()),
            Div => {
                if b == 0 {
                    Err("division by zero".into())
                } else {
                    Ok(Value::Int(a / b))
                }
            }
            Mod => {
                if b == 0 {
                    Err("division by zero".into())
                } else {
                    Ok(Value::Int(a % b))
                }
            }
            _ => unreachable!(),
        };
    }
    let a = to_f64(&l)?;
    let b = to_f64(&r)?;
    let v = match op {
        Add => a + b,
        Sub => a - b,
        Mul => a * b,
        Div => {
            if b == 0.0 {
                return Err("division by zero".into());
            }
            a / b
        }
        Mod => a % b,
        _ => unreachable!(),
    };
    Ok(Value::Float(v))
}

fn to_f64(v: &Value) -> Result<f64, String> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) => Ok(*f),
        Value::Text(s) => s.parse::<f64>().map_err(|_| format!("invalid number: {s}")),
        Value::Bool(_) | Value::Null => Err("operand is not numeric".into()),
    }
}

/// Total-ish ordering over comparable values. Returns `None` for genuinely
/// incomparable types. NULLs are not handled here (callers special-case them).
fn compare_values(l: &Value, r: &Value) -> Option<Ordering> {
    match (l, r) {
        (Value::Null, Value::Null) => Some(Ordering::Equal),
        (Value::Null, _) => Some(Ordering::Greater), // NULLs sort last
        (_, Value::Null) => Some(Ordering::Less),
        (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
        // Mixed numeric.
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            let a = match l {
                Value::Int(i) => *i as f64,
                Value::Float(f) => *f,
                _ => unreachable!(),
            };
            let b = match r {
                Value::Int(i) => *i as f64,
                Value::Float(f) => *f,
                _ => unreachable!(),
            };
            a.partial_cmp(&b)
        }
        // Number vs text: compare numerically when the text parses as a number
        // (e.g. `oid = '16384'`), otherwise compare as text.
        (Value::Int(_) | Value::Float(_), Value::Text(s)) => {
            let a = match l {
                Value::Int(i) => *i as f64,
                Value::Float(f) => *f,
                _ => unreachable!(),
            };
            match s.parse::<f64>() {
                Ok(b) => a.partial_cmp(&b),
                Err(_) => l.to_text().and_then(|ls| ls.as_str().partial_cmp(s.as_str())),
            }
        }
        (Value::Text(s), Value::Int(_) | Value::Float(_)) => {
            let b = match r {
                Value::Int(i) => *i as f64,
                Value::Float(f) => *f,
                _ => unreachable!(),
            };
            match s.parse::<f64>() {
                Ok(a) => a.partial_cmp(&b),
                Err(_) => r.to_text().and_then(|rs| s.as_str().partial_cmp(rs.as_str())),
            }
        }
        _ => None,
    }
}

/// Evaluate `expr [NOT] IN (list)` with SQL NULL semantics: an unmatched value
/// is UNKNOWN (NULL) if any list element is NULL.
fn eval_in_list(
    expr: &Expr,
    list: &[Expr],
    negated: bool,
    col_names: &[String],
    row: &[Value],
) -> Result<Value, String> {
    let target = eval_expr(expr, col_names, row)?;
    if target.is_null() {
        return Ok(Value::Null);
    }
    let mut matched = false;
    let mut saw_null = false;
    for item in list {
        let v = eval_expr(item, col_names, row)?;
        if v.is_null() {
            saw_null = true;
        } else if compare_values(&target, &v) == Some(Ordering::Equal) {
            matched = true;
            break;
        }
    }
    if matched {
        Ok(Value::Bool(!negated))
    } else if saw_null {
        Ok(Value::Null)
    } else {
        Ok(Value::Bool(negated))
    }
}

/// Minimal POSIX-style regex matcher supporting `^`, `$`, `.`, and `*`
/// (enough for the catalog patterns PostgreSQL clients use, e.g. `^pg_toast`).
fn regex_match(pattern: &str, text: &str, case_insensitive: bool) -> bool {
    let fold = |s: &str| if case_insensitive { s.to_lowercase() } else { s.to_string() };
    let p: Vec<char> = fold(pattern).chars().collect();
    let t: Vec<char> = fold(text).chars().collect();
    if p.first() == Some(&'^') {
        return regex_here(&p[1..], &t);
    }
    // Unanchored: try matching at each position (including the empty tail).
    for start in 0..=t.len() {
        if regex_here(&p, &t[start..]) {
            return true;
        }
    }
    false
}

fn regex_here(p: &[char], t: &[char]) -> bool {
    if p.is_empty() {
        return true;
    }
    // Treat grouping parens as transparent (no alternation support yet),
    // which is enough for client patterns like `^(tablename)$`.
    if p[0] == '(' || p[0] == ')' {
        return regex_here(&p[1..], t);
    }
    if p.len() >= 2 && p[1] == '*' {
        return regex_star(p[0], &p[2..], t);
    }
    if p[0] == '$' && p.len() == 1 {
        return t.is_empty();
    }
    if !t.is_empty() && (p[0] == '.' || p[0] == t[0]) {
        return regex_here(&p[1..], &t[1..]);
    }
    false
}

/// Match zero or more of `c` followed by the rest of the pattern.
fn regex_star(c: char, rest: &[char], t: &[char]) -> bool {
    let mut i = 0;
    loop {
        if regex_here(rest, &t[i..]) {
            return true;
        }
        if i < t.len() && (c == '.' || c == t[i]) {
            i += 1;
        } else {
            return false;
        }
    }
}

/// SQL `LIKE` matching with `%` (any run) and `_` (any single char).
/// No escape character is supported yet.
fn like_match(text: &str, pattern: &str, case_insensitive: bool) -> bool {
    // Work over char vectors for correct Unicode handling.
    let fold = |s: &str| {
        if case_insensitive {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };
    let t: Vec<char> = fold(text).chars().collect();
    let p: Vec<char> = fold(pattern).chars().collect();
    like_match_inner(&t, &p)
}

/// Backtracking glob matcher for `%`/`_`.
fn like_match_inner(t: &[char], p: &[char]) -> bool {
    // ti/pi indices; star tracks the last '%' position for backtracking.
    let (mut ti, mut pi) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            // Backtrack: let the '%' consume one more character.
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, ti));
        } else {
            return false;
        }
    }
    // Consume trailing '%' in the pattern.
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    pi == p.len()
}

fn eval_scalar_function(
    name: &str,
    args: &[Expr],
    _star: bool,
    col_names: &[String],
    row: &[Value],
) -> Result<Value, String> {
    let lname = name.to_ascii_lowercase();
    // Evaluate args eagerly (functions here are not lazy).
    let mut vals = Vec::with_capacity(args.len());
    for a in args {
        vals.push(eval_expr(a, col_names, row)?);
    }
    match lname.as_str() {
        "upper" => str_fn(&vals, |s| s.to_uppercase()),
        "lower" => str_fn(&vals, |s| s.to_lowercase()),
        "length" | "char_length" | "character_length" => {
            let v = arg(&vals, 0)?;
            if v.is_null() {
                Ok(Value::Null)
            } else {
                Ok(Value::Int(v.to_text().unwrap_or_default().chars().count() as i64))
            }
        }
        "abs" => {
            let v = arg(&vals, 0)?;
            match v {
                Value::Int(i) => Ok(Value::Int(i.abs())),
                Value::Float(f) => Ok(Value::Float(f.abs())),
                Value::Null => Ok(Value::Null),
                _ => Err("abs() requires a numeric argument".into()),
            }
        }
        "coalesce" => {
            for v in &vals {
                if !v.is_null() {
                    return Ok(v.clone());
                }
            }
            Ok(Value::Null)
        }
        "concat" => {
            let mut s = String::new();
            for v in &vals {
                if let Some(t) = v.to_text() {
                    s.push_str(&t);
                }
            }
            Ok(Value::Text(s))
        }
        "nullif" => {
            let a = arg(&vals, 0)?;
            let b = arg(&vals, 1)?;
            if !a.is_null() && !b.is_null() && compare_values(a, b) == Some(Ordering::Equal) {
                Ok(Value::Null)
            } else {
                Ok(a.clone())
            }
        }
        "greatest" | "least" => {
            let want_greatest = lname == "greatest";
            let mut best: Option<Value> = None;
            for v in &vals {
                if v.is_null() {
                    continue;
                }
                best = Some(match best {
                    None => v.clone(),
                    Some(cur) => {
                        let ord = compare_values(v, &cur).unwrap_or(Ordering::Equal);
                        let take = if want_greatest { ord == Ordering::Greater } else { ord == Ordering::Less };
                        if take { v.clone() } else { cur }
                    }
                });
            }
            Ok(best.unwrap_or(Value::Null))
        }
        "round" => {
            let v = arg(&vals, 0)?;
            if v.is_null() {
                return Ok(Value::Null);
            }
            let x = to_f64(v)?;
            let digits = match vals.get(1) {
                Some(Value::Int(d)) => *d,
                _ => 0,
            };
            let factor = 10f64.powi(digits as i32);
            let rounded = (x * factor).round() / factor;
            // round(x) with no/zero digits yields an integer in PostgreSQL.
            if digits <= 0 && matches!(v, Value::Int(_)) {
                Ok(Value::Int(rounded as i64))
            } else {
                Ok(Value::Float(rounded))
            }
        }
        "trim" | "btrim" => str_fn(&vals, |s| s.trim().to_string()),
        "ltrim" => str_fn(&vals, |s| s.trim_start().to_string()),
        "rtrim" => str_fn(&vals, |s| s.trim_end().to_string()),
        "replace" => {
            let s = arg(&vals, 0)?;
            let from = arg(&vals, 1)?;
            let to = arg(&vals, 2)?;
            if s.is_null() || from.is_null() || to.is_null() {
                return Ok(Value::Null);
            }
            Ok(Value::Text(s.to_text().unwrap_or_default().replace(
                &from.to_text().unwrap_or_default(),
                &to.to_text().unwrap_or_default(),
            )))
        }
        "substr" | "substring" => {
            let s = arg(&vals, 0)?;
            if s.is_null() {
                return Ok(Value::Null);
            }
            let text: Vec<char> = s.to_text().unwrap_or_default().chars().collect();
            // PostgreSQL substring is 1-based.
            let start = match vals.get(1) {
                Some(Value::Int(i)) => *i,
                _ => 1,
            };
            let start_idx = (start.max(1) - 1) as usize;
            let result: String = match vals.get(2) {
                Some(Value::Int(len)) => {
                    text.iter().skip(start_idx).take((*len).max(0) as usize).collect()
                }
                _ => text.iter().skip(start_idx).collect(),
            };
            Ok(Value::Text(result))
        }
        "version" => Ok(Value::Text(
            "PostgreSQL 16.0 (postgres-rs) on rust".to_string(),
        )),
        "now" | "current_timestamp" => Ok(Value::Text("1970-01-01 00:00:00+00".to_string())),
        "current_date" => Ok(Value::Text("1970-01-01".to_string())),
        "current_database" | "current_catalog" => Ok(Value::Text("postgres".to_string())),
        "current_user" | "current_role" | "session_user" | "user" => {
            Ok(Value::Text("postgres".to_string()))
        }
        "current_schema" => Ok(Value::Text("public".to_string())),
        // Catalog helpers used by psql meta-commands.
        "pg_get_userbyid" => Ok(Value::Text("postgres".to_string())),
        "pg_table_is_visible" | "pg_function_is_visible" | "pg_type_is_visible" => {
            Ok(Value::Bool(true))
        }
        "pg_get_expr" | "pg_get_constraintdef" | "pg_get_indexdef" | "format_type" => {
            Ok(arg(&vals, 0).cloned().unwrap_or(Value::Null))
        }
        "pg_encoding_to_char" => Ok(Value::Text("UTF8".to_string())),
        // Aggregates reaching here means used outside an aggregate context.
        "count" | "sum" | "avg" | "min" | "max" => {
            Err(format!("aggregate function {lname}() is not allowed here"))
        }
        other => Err(format!("function {other}() does not exist")),
    }
}

fn str_fn(vals: &[Value], f: impl Fn(&str) -> String) -> Result<Value, String> {
    let v = arg(vals, 0)?;
    if v.is_null() {
        Ok(Value::Null)
    } else {
        Ok(Value::Text(f(&v.to_text().unwrap_or_default())))
    }
}

fn arg<'a>(vals: &'a [Value], i: usize) -> Result<&'a Value, String> {
    vals.get(i).ok_or_else(|| "missing function argument".to_string())
}

// --- aggregates --------------------------------------------------------------

fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function { name, .. } if is_aggregate_name(name) => true,
        Expr::Function { args, .. } => args.iter().any(contains_aggregate),
        Expr::Unary { expr, .. } => contains_aggregate(expr),
        Expr::Binary { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::IsNull { expr, .. } => contains_aggregate(expr),
        Expr::Cast { expr, .. } => contains_aggregate(expr),
        Expr::Like { expr, pattern, .. } => contains_aggregate(expr) || contains_aggregate(pattern),
        Expr::InList { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        Expr::Between { expr, low, high, .. } => {
            contains_aggregate(expr) || contains_aggregate(low) || contains_aggregate(high)
        }
        Expr::Case { operand, whens, else_expr } => {
            operand.as_deref().is_some_and(contains_aggregate)
                || whens.iter().any(|(c, r)| contains_aggregate(c) || contains_aggregate(r))
                || else_expr.as_deref().is_some_and(contains_aggregate)
        }
        // A subquery's own aggregates don't make the outer expression an
        // aggregate; only the IN-test's left operand matters here.
        Expr::InSubquery { expr, .. } => contains_aggregate(expr),
        _ => false,
    }
}

fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count" | "sum" | "avg" | "min" | "max" | "string_agg"
    )
}

/// Evaluate an expression tree that may contain aggregates over a row set.
/// Non-aggregate leaves are evaluated against the first row (best-effort,
/// since without GROUP BY they should be constants).
fn eval_aggregate_expr(
    expr: &Expr,
    col_names: &[String],
    rows: &[Vec<Value>],
) -> Result<Value, String> {
    match expr {
        Expr::Function { name, args, star, distinct } if is_aggregate_name(name) => {
            eval_aggregate(name, args, *star, *distinct, col_names, rows)
        }
        Expr::Binary { op, left, right } => {
            let l = eval_aggregate_expr(left, col_names, rows)?;
            let r = eval_aggregate_expr(right, col_names, rows)?;
            eval_binary(*op, l, r)
        }
        Expr::Unary { op, expr } => {
            let v = eval_aggregate_expr(expr, col_names, rows)?;
            eval_unary(*op, v)
        }
        // Constant or column leaf: evaluate against the first row if present.
        _ => {
            let empty = Vec::new();
            let row = rows.first().unwrap_or(&empty);
            eval_expr(expr, col_names, row)
        }
    }
}

fn eval_aggregate(
    name: &str,
    args: &[Expr],
    star: bool,
    distinct: bool,
    col_names: &[String],
    rows: &[Vec<Value>],
) -> Result<Value, String> {
    let lname = name.to_ascii_lowercase();

    // count(*) ignores the argument and counts rows.
    if lname == "count" && star {
        return Ok(Value::Int(rows.len() as i64));
    }

    // Collect the (non-null) argument values once, deduplicating for DISTINCT.
    let arg = args.first().ok_or_else(|| format!("{lname}() requires an argument"))?;
    let mut vals: Vec<Value> = Vec::new();
    for row in rows {
        let v = eval_expr(arg, col_names, row)?;
        if !v.is_null() {
            vals.push(v);
        }
    }
    if distinct {
        let mut seen = std::collections::HashSet::new();
        vals.retain(|v| seen.insert(v.to_text().unwrap_or_default()));
    }

    match lname.as_str() {
        "count" => Ok(Value::Int(vals.len() as i64)),
        "sum" => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            let mut int_sum: i64 = 0;
            let mut float_sum: f64 = 0.0;
            let mut is_float = false;
            for v in &vals {
                match v {
                    Value::Int(i) => {
                        int_sum += i;
                        float_sum += *i as f64;
                    }
                    Value::Float(f) => {
                        is_float = true;
                        float_sum += f;
                    }
                    _ => return Err("sum() requires numeric input".into()),
                }
            }
            Ok(if is_float { Value::Float(float_sum) } else { Value::Int(int_sum) })
        }
        "avg" => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            let mut sum = 0.0;
            for v in &vals {
                sum += to_f64(v)?;
            }
            Ok(Value::Float(sum / vals.len() as f64))
        }
        "min" | "max" => {
            let want_min = lname == "min";
            let mut best: Option<Value> = None;
            for v in &vals {
                best = Some(match best {
                    None => v.clone(),
                    Some(cur) => {
                        let ord = compare_values(v, &cur).unwrap_or(Ordering::Equal);
                        let take = if want_min { ord == Ordering::Less } else { ord == Ordering::Greater };
                        if take { v.clone() } else { cur }
                    }
                });
            }
            Ok(best.unwrap_or(Value::Null))
        }
        "string_agg" => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            // The separator is a constant second argument.
            let sep = match args.get(1) {
                Some(e) => {
                    let empty = Vec::new();
                    let row = rows.first().unwrap_or(&empty);
                    eval_expr(e, col_names, row)?.to_text().unwrap_or_default()
                }
                None => String::new(),
            };
            let parts: Vec<String> = vals.iter().map(|v| v.to_text().unwrap_or_default()).collect();
            Ok(Value::Text(parts.join(&sep)))
        }
        _ => Err(format!("unknown aggregate {lname}")),
    }
}

// --- helpers -----------------------------------------------------------------

/// Evaluate a LIMIT/OFFSET expression to a non-negative count.
fn eval_count(expr: &Option<Expr>, col_names: &[String]) -> Result<Option<usize>, String> {
    match expr {
        None => Ok(None),
        Some(e) => match eval_expr(e, col_names, &[])? {
            Value::Int(i) if i >= 0 => Ok(Some(i as usize)),
            Value::Null => Ok(None),
            Value::Int(_) => Err("LIMIT/OFFSET must not be negative".into()),
            _ => Err("LIMIT/OFFSET must be an integer".into()),
        },
    }
}

/// Coerce a value to a target column type, applying lenient conversions that
/// match what PostgreSQL accepts for literals.
fn coerce(v: Value, target: DataType) -> Result<Value, String> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let err = |from: &str| format!("cannot coerce {from} to {}", target.sql_name());
    match target {
        DataType::Int2 | DataType::Int4 | DataType::Int8 => match v {
            Value::Int(i) => Ok(Value::Int(i)),
            // PostgreSQL rounds when casting a float to an integer.
            Value::Float(f) => Ok(Value::Int(f.round() as i64)),
            Value::Text(s) => s
                .trim()
                .parse::<i64>()
                .map(Value::Int)
                .map_err(|_| format!("invalid input syntax for type integer: \"{s}\"")),
            Value::Bool(b) => Ok(Value::Int(b as i64)),
            _ => Err(err("value")),
        },
        DataType::Float4 | DataType::Float8 | DataType::Numeric => match v {
            Value::Float(f) => Ok(Value::Float(f)),
            Value::Int(i) => Ok(Value::Float(i as f64)),
            Value::Text(s) => s
                .trim()
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| format!("invalid input syntax for type {}: \"{s}\"", target.sql_name())),
            _ => Err(err("value")),
        },
        DataType::Bool => match v {
            Value::Bool(b) => Ok(Value::Bool(b)),
            Value::Int(i) => Ok(Value::Bool(i != 0)),
            Value::Text(s) => match s.trim().to_ascii_lowercase().as_str() {
                "t" | "true" | "yes" | "on" | "1" => Ok(Value::Bool(true)),
                "f" | "false" | "no" | "off" | "0" => Ok(Value::Bool(false)),
                _ => Err(format!("invalid input syntax for type boolean: \"{s}\"")),
            },
            _ => Err(err("value")),
        },
        // Text and the date/time/uuid/json family are stored as text.
        DataType::Text
        | DataType::Date
        | DataType::Time
        | DataType::Timestamp
        | DataType::TimestampTz
        | DataType::Uuid
        | DataType::Json
        | DataType::Jsonb => Ok(Value::Text(v.to_text().unwrap_or_default())),
    }
}

/// The default output column name PostgreSQL would assign to an expression.
fn default_column_name(expr: &Expr) -> String {
    match expr {
        Expr::Column(name) => name.clone(),
        Expr::QualifiedColumn { name, .. } => name.clone(),
        Expr::Function { name, .. } => name.to_ascii_lowercase(),
        _ => "?column?".to_string(),
    }
}

/// Best-effort static type inference for an expression, used to fill the
/// RowDescription before any rows are seen.
fn infer_expr_type(expr: &Expr, col_names: &[String], col_types: &[DataType]) -> DataType {
    match expr {
        Expr::Int(_) => DataType::Int8,
        Expr::Float(_) => DataType::Float8,
        Expr::Str(_) => DataType::Text,
        Expr::Bool(_) => DataType::Bool,
        Expr::Null => DataType::Text,
        Expr::Param(_) => DataType::Text,
        Expr::Column(name) => resolve_column(col_names, None, name)
            .ok()
            .map(|i| col_types[i])
            .unwrap_or(DataType::Text),
        Expr::QualifiedColumn { qualifier, name } => {
            resolve_column(col_names, Some(qualifier), name)
                .ok()
                .map(|i| col_types[i])
                .unwrap_or(DataType::Text)
        }
        Expr::IsNull { .. } => DataType::Bool,
        Expr::Cast { target, .. } => *target,
        Expr::Like { .. } | Expr::InList { .. } | Expr::Between { .. } => DataType::Bool,
        Expr::Exists(_) | Expr::InSubquery { .. } => DataType::Bool,
        // A scalar subquery's type is only known once executed; default to text
        // for the pre-execution RowDescription (the value is resolved later).
        Expr::ScalarSubquery(_) => DataType::Text,
        Expr::Case { whens, else_expr, .. } => {
            // Type of the first THEN result (fallback to ELSE, then text).
            if let Some((_, result)) = whens.first() {
                infer_expr_type(result, col_names, col_types)
            } else if let Some(e) = else_expr {
                infer_expr_type(e, col_names, col_types)
            } else {
                DataType::Text
            }
        }
        Expr::Unary { op: UnaryOp::Not, .. } => DataType::Bool,
        Expr::Unary { expr, .. } => infer_expr_type(expr, col_names, col_types),
        Expr::Binary { op, left, right } => match op {
            BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
            | BinaryOp::And
            | BinaryOp::Or
            | BinaryOp::RegexMatch { .. }
            | BinaryOp::RegexNotMatch { .. } => DataType::Bool,
            BinaryOp::Concat => DataType::Text,
            _ => {
                let l = infer_expr_type(left, col_names, col_types);
                let r = infer_expr_type(right, col_names, col_types);
                if l == DataType::Float8 || r == DataType::Float8 {
                    DataType::Float8
                } else {
                    DataType::Int8
                }
            }
        },
        Expr::Function { name, .. } => match name.to_ascii_lowercase().as_str() {
            "count" => DataType::Int8,
            "sum" | "abs" => DataType::Int8,
            "avg" | "round" => DataType::Float8,
            "length" | "char_length" | "character_length" => DataType::Int8,
            _ => DataType::Text,
        },
    }
}
