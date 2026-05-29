//! Query executor: turns a parsed [`Statement`] into a result against the
//! in-memory [`Database`].

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::index::Bound;
use crate::sql::ast::*;
use crate::sql::serialize::expr_to_sql;
use crate::storage::{
    Aggregate, CheckConstraint, Column, Database, ForeignKeyConstraint, MaterializedView, Rule,
    SqlFunction, Table, Trigger, UniqueConstraint, View,
};
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
    Rows {
        fields: Vec<FieldDescription>,
        rows: Vec<Vec<Value>>,
        tag: String,
    },
    /// A command completed with the given PostgreSQL command tag,
    /// e.g. `"INSERT 0 3"`, `"CREATE TABLE"`, `"UPDATE 2"`.
    Command(String),
    /// An empty query (no statement).
    Empty,
}

const PARALLEL_SELECT_MIN_ROWS: usize = 128;
const PARALLEL_SELECT_MAX_WORKERS: usize = 4;

/// A scalar user-defined function whose body has been pre-parsed into the
/// single projection expression of a `SELECT <expr>` (no `FROM`). This is the
/// only UDF shape callable from `eval_expr`, which has no `Database` handle.
#[derive(Clone)]
struct ScalarUdf {
    /// Argument names in declaration order (lowercased; empty when unnamed).
    arg_names: Vec<String>,
    /// Argument types, used to coerce the bound values.
    arg_types: Vec<DataType>,
    /// The parsed body projection expression.
    body: Expr,
    /// The declared return type, used to coerce the result.
    return_type: Option<DataType>,
}

thread_local! {
    /// Scalar UDFs visible to the currently-executing statement, keyed by
    /// `(lowercased name, arity)`. Populated once per top-level `execute` so
    /// that `eval_scalar_function` (which receives no `Database`) can resolve a
    /// call to a user-defined scalar SQL function.
    static SCALAR_UDFS: std::cell::RefCell<HashMap<(String, usize), ScalarUdf>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Rebuild the thread-local scalar-UDF registry from the database catalog.
/// Functions whose body is not a bare `SELECT <expr>` (e.g. they need table
/// access, or are non-`sql` language) are simply omitted: a call to one then
/// surfaces the usual "function does not exist" error from `eval_scalar_function`.
fn refresh_scalar_udfs(db: &Database) {
    let mut map = HashMap::new();
    for f in db.all_functions() {
        if f.language != "sql" {
            continue;
        }
        if let Some(body) = scalar_udf_body(&f.body) {
            let arg_names = f
                .arg_names
                .iter()
                .map(|n| n.clone().unwrap_or_default().to_ascii_lowercase())
                .collect();
            map.insert(
                (f.name.to_ascii_lowercase(), f.arg_types.len()),
                ScalarUdf {
                    arg_names,
                    arg_types: f.arg_types.clone(),
                    body,
                    return_type: f.return_type,
                },
            );
        }
    }
    SCALAR_UDFS.with(|cell| *cell.borrow_mut() = map);
}

/// Parse a SQL-function body that is a single `SELECT <expr>` (optionally
/// trailing `;`) and with no `FROM`, returning its projection expression.
/// Returns `None` for any other shape (those bodies are not scalar-callable).
fn scalar_udf_body(body: &str) -> Option<Expr> {
    let trimmed = body.trim().trim_end_matches(';').trim();
    let stmts = crate::sql::Parser::parse_sql(trimmed).ok()?;
    if stmts.len() != 1 {
        return None;
    }
    let Statement::Select(sel) = &stmts[0] else {
        return None;
    };
    if sel.from.is_some() || !sel.set_ops.is_empty() || sel.projection.len() != 1 {
        return None;
    }
    match &sel.projection[0] {
        SelectItem::Expr { expr, .. } => Some(expr.clone()),
        SelectItem::Wildcard => None,
    }
}

/// Substitute UDF argument references (`$1`-style params and named arguments)
/// in a body expression with the bound argument values, returning a rewritten
/// expression evaluable with no columns in scope.
fn substitute_udf_args(expr: &Expr, args: &[Value], arg_names: &[String]) -> Expr {
    let lit = |v: &Value| -> Expr {
        match v {
            Value::Null => Expr::Null,
            Value::Int(i) => Expr::Int(*i),
            Value::Float(f) => Expr::Float(*f),
            Value::Bool(b) => Expr::Bool(*b),
            other => Expr::Str(other.to_text().unwrap_or_default()),
        }
    };
    match expr {
        Expr::Param(n) => {
            let idx = (*n as usize).wrapping_sub(1);
            args.get(idx).map(lit).unwrap_or(Expr::Null)
        }
        Expr::Column(name) => {
            match arg_names.iter().position(|a| a.eq_ignore_ascii_case(name)) {
                Some(idx) => args.get(idx).map(lit).unwrap_or(Expr::Null),
                None => expr.clone(),
            }
        }
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(substitute_udf_args(expr, args, arg_names)),
        },
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(substitute_udf_args(left, args, arg_names)),
            right: Box::new(substitute_udf_args(right, args, arg_names)),
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(substitute_udf_args(expr, args, arg_names)),
            negated: *negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => Expr::Like {
            expr: Box::new(substitute_udf_args(expr, args, arg_names)),
            pattern: Box::new(substitute_udf_args(pattern, args, arg_names)),
            negated: *negated,
            case_insensitive: *case_insensitive,
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(substitute_udf_args(expr, args, arg_names)),
            low: Box::new(substitute_udf_args(low, args, arg_names)),
            high: Box::new(substitute_udf_args(high, args, arg_names)),
            negated: *negated,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(substitute_udf_args(expr, args, arg_names)),
            list: list
                .iter()
                .map(|e| substitute_udf_args(e, args, arg_names))
                .collect(),
            negated: *negated,
        },
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => Expr::Case {
            operand: operand
                .as_ref()
                .map(|o| Box::new(substitute_udf_args(o, args, arg_names))),
            whens: whens
                .iter()
                .map(|(c, r)| {
                    (
                        substitute_udf_args(c, args, arg_names),
                        substitute_udf_args(r, args, arg_names),
                    )
                })
                .collect(),
            else_expr: else_expr
                .as_ref()
                .map(|e| Box::new(substitute_udf_args(e, args, arg_names))),
        },
        Expr::Cast { expr, target } => Expr::Cast {
            expr: Box::new(substitute_udf_args(expr, args, arg_names)),
            target: *target,
        },
        Expr::Function {
            name,
            args: fargs,
            star,
            distinct,
            filter,
            over,
        } => Expr::Function {
            name: name.clone(),
            args: fargs
                .iter()
                .map(|e| substitute_udf_args(e, args, arg_names))
                .collect(),
            star: *star,
            distinct: *distinct,
            filter: filter
                .as_ref()
                .map(|f| Box::new(substitute_udf_args(f, args, arg_names))),
            over: over.clone(),
        },
        Expr::Row(items) => Expr::Row(
            items
                .iter()
                .map(|e| substitute_udf_args(e, args, arg_names))
                .collect(),
        ),
        Expr::Array(items) => Expr::Array(
            items
                .iter()
                .map(|e| substitute_udf_args(e, args, arg_names))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Evaluate a user-defined scalar SQL function call, given already-evaluated
/// argument values. Returns `None` when no scalar UDF matches the name/arity,
/// so the caller can fall through to the normal "unknown function" error.
fn try_eval_scalar_udf(name: &str, vals: &[Value]) -> Option<Result<Value, String>> {
    let key = (name.to_ascii_lowercase(), vals.len());
    let udf = SCALAR_UDFS.with(|cell| cell.borrow().get(&key).cloned())?;
    // Coerce each argument to its declared type before substitution.
    let mut coerced = Vec::with_capacity(vals.len());
    for (v, ty) in vals.iter().zip(udf.arg_types.iter()) {
        match coerce(v.clone(), *ty) {
            Ok(c) => coerced.push(c),
            Err(e) => return Some(Err(e)),
        }
    }
    let substituted = substitute_udf_args(&udf.body, &coerced, &udf.arg_names);
    let result = eval_expr(&substituted, &[], &[]);
    Some(match (result, udf.return_type) {
        (Ok(v), Some(ty)) => coerce(v, ty),
        (other, _) => other,
    })
}

pub fn execute(db: &mut Database, stmt: Statement) -> Result<ExecResult, String> {
    // Make user-defined scalar functions visible to expression evaluation,
    // which has no direct `Database` handle.
    refresh_scalar_udfs(db);
    match stmt {
        Statement::CreateTable(c) => exec_create_table(db, c),
        Statement::CreateExtension(c) => exec_create_extension(db, c),
        Statement::CreateRole(c) => exec_create_role(db, c),
        Statement::CreateSequence(c) => exec_create_sequence(db, c),
        Statement::CreateSchema(c) => exec_create_schema(db, c),
        Statement::CreateDatabase(c) => exec_create_database(db, c),
        Statement::CreateTablespace(c) => exec_create_tablespace(db, c),
        Statement::CreateCollation(c) => exec_create_collation(db, c),
        Statement::CreateType(c) => exec_create_type(db, c),
        Statement::CreateDomain(c) => exec_create_domain(db, c),
        Statement::CreateView(c) => exec_create_view(db, c),
        Statement::CreateMaterializedView(c) => exec_create_materialized_view(db, c),
        Statement::CreateFunction(c) => exec_create_function(db, c),
        Statement::CreateTrigger(c) => exec_create_trigger(db, c),
        Statement::CreateRule(c) => exec_create_rule(db, c),
        Statement::CreateAggregate(c) => exec_create_aggregate(db, c),
        Statement::DropFunction(d) => exec_drop_function(db, d),
        Statement::DropTrigger(d) => exec_drop_trigger(db, d),
        Statement::DropRule(d) => exec_drop_rule(db, d),
        Statement::DropAggregate(d) => exec_drop_aggregate(db, d),
        Statement::DropTable(d) => exec_drop_table(db, d),
        Statement::DropExtension(d) => exec_drop_extension(db, d),
        Statement::DropRole(d) => exec_drop_role(db, d),
        Statement::DropSequence(d) => exec_drop_sequence(db, d),
        Statement::DropSchema(d) => exec_drop_schema(db, d),
        Statement::DropDatabase(d) => exec_drop_database(db, d),
        Statement::DropTablespace(d) => exec_drop_tablespace(db, d),
        Statement::DropCollation(d) => exec_drop_collation(db, d),
        Statement::DropType(d) => exec_drop_type(db, d),
        Statement::DropDomain(d) => exec_drop_domain(db, d),
        Statement::DropView(d) => exec_drop_view(db, d),
        Statement::DropMaterializedView(d) => exec_drop_materialized_view(db, d),
        Statement::AlterTable(a) => exec_alter_table(db, a),
        Statement::AlterRole(a) => exec_alter_role(db, a),
        Statement::AlterSequence(a) => exec_alter_sequence(db, a),
        Statement::CreateIndex(c) => exec_create_index(db, c),
        Statement::DropIndex(d) => exec_drop_index(db, d),
        Statement::Insert(i) => exec_insert(db, i),
        Statement::Copy(_) => Err(
            "COPY ... FROM STDIN / TO STDOUT must be issued via the simple query protocol".into(),
        ),
        Statement::Truncate(t) => exec_truncate(db, t),
        Statement::DeclareCursor(d) => exec_declare_cursor(db, d),
        Statement::Fetch(f) => exec_fetch(db, f),
        Statement::Select(s) => exec_select(db, s),
        Statement::AlterDatabase(a) => exec_alter_database(db, a),
        Statement::Update(u) => exec_update(db, u),
        Statement::Delete(d) => exec_delete(db, d),
        Statement::Merge(m) => exec_merge(db, m),
        Statement::Explain(e) => exec_explain(db, e),
        Statement::Analyze(a) => exec_analyze(db, a),
        Statement::Comment(c) => exec_comment(db, c),
        Statement::SecurityLabel(s) => exec_security_label(db, s),
        Statement::Grant(g) => exec_grant(db, g),
        Statement::Revoke(r) => exec_revoke(db, r),
        Statement::AlterSystem(a) => exec_alter_system(db, a),
        Statement::Vacuum(v) => exec_vacuum(db, v),
        Statement::Reindex(r) => exec_reindex(db, r),
        Statement::Cluster(c) => exec_cluster(db, c),
        Statement::Checkpoint => Ok(ExecResult::Command("CHECKPOINT".into())),
        Statement::Discard(d) => Ok(ExecResult::Command(discard_tag(&d).into())),
        Statement::Listen { .. } => Ok(ExecResult::Command("LISTEN".into())),
        Statement::Notify { .. } => Ok(ExecResult::Command("NOTIFY".into())),
        Statement::Unlisten { .. } => Ok(ExecResult::Command("UNLISTEN".into())),
        Statement::LockTable(l) => exec_lock_table(db, l),
        Statement::RefreshMaterializedView(r) => exec_refresh_materialized_view(db, r),
        Statement::Begin => Ok(ExecResult::Command("BEGIN".into())),
        Statement::Commit => Ok(ExecResult::Command("COMMIT".into())),
        Statement::Rollback => Ok(ExecResult::Command("ROLLBACK".into())),
        Statement::Savepoint { .. } => Ok(ExecResult::Command("SAVEPOINT".into())),
        Statement::ReleaseSavepoint { .. } => Ok(ExecResult::Command("RELEASE".into())),
        Statement::RollbackToSavepoint { .. } => Ok(ExecResult::Command("ROLLBACK".into())),
        Statement::Set { name, value } => exec_set(db, name, value),
        Statement::Show { name } => exec_show(db, name),
        Statement::Empty => Ok(ExecResult::Empty),
    }
}

fn exec_truncate(db: &mut Database, t: Truncate) -> Result<ExecResult, String> {
    for name in &t.tables {
        if db.table(name).is_none() {
            return Err(format!("relation \"{name}\" does not exist"));
        }
    }
    for name in t.tables {
        let table = db
            .table_mut(&name)
            .expect("table existence checked before truncate");
        table.truncate();
    }
    Ok(ExecResult::Command("TRUNCATE TABLE".into()))
}

fn exec_declare_cursor(db: &mut Database, d: DeclareCursor) -> Result<ExecResult, String> {
    let fields = select_fields(db, &d.select)?
        .into_iter()
        .map(|field| (field.name, field.data_type))
        .collect();
    let ExecResult::Rows { rows, .. } = exec_select(db, *d.select)? else {
        return Err("cursor query did not produce rows".into());
    };
    db.declare_cursor(d.name, fields, rows)?;
    Ok(ExecResult::Command("DECLARE CURSOR".into()))
}

fn exec_fetch(db: &mut Database, f: Fetch) -> Result<ExecResult, String> {
    let count = match f.count {
        FetchCount::Next => Some(1),
        FetchCount::All => None,
        FetchCount::Count(n) => Some(n.max(0) as usize),
    };
    let (fields, rows) = db.fetch_cursor(&f.cursor, count)?;
    let fields = fields
        .into_iter()
        .map(|(name, data_type)| FieldDescription { name, data_type })
        .collect();
    let tag = format!("FETCH {}", rows.len());
    Ok(ExecResult::Rows { fields, rows, tag })
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
        Statement::Explain(_) => Ok(Some(vec![FieldDescription {
            name: "QUERY PLAN".into(),
            data_type: DataType::Text,
        }])),
        Statement::Fetch(fetch) => Ok(Some(
            db.cursor_fields(&fetch.cursor)
                .unwrap_or_default()
                .into_iter()
                .map(|(name, data_type)| FieldDescription { name, data_type })
                .collect(),
        )),
        Statement::Show { name } => Ok(Some(vec![FieldDescription {
            name: name.clone(),
            data_type: DataType::Text,
        }])),
        _ => Ok(None),
    }
}

fn exec_explain(db: &mut Database, explain: Explain) -> Result<ExecResult, String> {
    let mut lines = explain_statement(&explain.statement);
    if explain.analyze {
        let result = execute(db, *explain.statement)?;
        let observed = match result {
            ExecResult::Rows { rows, .. } => format!("actual rows={}", rows.len()),
            ExecResult::Command(tag) => format!("actual command={tag}"),
            ExecResult::Empty => "actual empty".to_string(),
        };
        if let Some(first) = lines.first_mut() {
            first.push_str(&format!(" ({observed})"));
        } else {
            lines.push(format!("Result ({observed})"));
        }
    }
    let rows: Vec<Vec<Value>> = lines.into_iter().map(|l| vec![Value::Text(l)]).collect();
    let tag = format!("EXPLAIN {}", rows.len());
    Ok(ExecResult::Rows {
        fields: vec![FieldDescription {
            name: "QUERY PLAN".into(),
            data_type: DataType::Text,
        }],
        rows,
        tag,
    })
}

fn explain_statement(stmt: &Statement) -> Vec<String> {
    match stmt {
        Statement::Select(sel) => explain_select(sel),
        Statement::Insert(i) => vec![format!("Insert on {}", i.table)],
        Statement::Copy(c) => vec![format!("Copy {}", c.table)],
        Statement::Update(u) => vec![format!("Update on {}", u.table)],
        Statement::Delete(d) => vec![format!("Delete on {}", d.table)],
        Statement::Merge(m) => vec![format!("Merge on {}", m.target)],
        Statement::CreateTable(c) => vec![format!("Create Table {}", c.name)],
        Statement::CreateExtension(c) => vec![format!("Create Extension {}", c.name)],
        Statement::CreateRole(c) => vec![format!("Create Role {}", c.name)],
        Statement::CreateSequence(c) => vec![format!("Create Sequence {}", c.name)],
        Statement::CreateSchema(c) => vec![format!("Create Schema {}", c.name)],
        Statement::CreateDatabase(c) => vec![format!("Create Database {}", c.name)],
        Statement::CreateTablespace(c) => vec![format!("Create Tablespace {}", c.name)],
        Statement::CreateCollation(c) => vec![format!("Create Collation {}", c.name)],
        Statement::CreateType(c) => vec![format!("Create Type {}", c.name)],
        Statement::CreateDomain(c) => vec![format!("Create Domain {}", c.name)],
        Statement::CreateView(c) => vec![format!("Create View {}", c.name)],
        Statement::CreateMaterializedView(c) => {
            vec![format!("Create Materialized View {}", c.name)]
        }
        Statement::CreateFunction(c) => vec![format!("Create Function {}", c.name)],
        Statement::CreateTrigger(c) => vec![format!("Create Trigger {}", c.name)],
        Statement::CreateRule(c) => vec![format!("Create Rule {}", c.name)],
        Statement::CreateAggregate(c) => vec![format!("Create Aggregate {}", c.name)],
        Statement::DropFunction(d) => vec![format!("Drop Function {}", d.name)],
        Statement::DropTrigger(d) => vec![format!("Drop Trigger {}", d.name)],
        Statement::DropRule(d) => vec![format!("Drop Rule {}", d.name)],
        Statement::DropAggregate(d) => vec![format!("Drop Aggregate {}", d.name)],
        Statement::DropTable(d) => vec![format!("Drop Table {}", d.name)],
        Statement::DropExtension(d) => vec![format!("Drop Extension {}", d.name)],
        Statement::DropRole(d) => vec![format!("Drop Role {}", d.name)],
        Statement::DropSequence(d) => vec![format!("Drop Sequence {}", d.name)],
        Statement::DropSchema(d) => vec![format!("Drop Schema {}", d.name)],
        Statement::DropDatabase(d) => vec![format!("Drop Database {}", d.name)],
        Statement::DropTablespace(d) => vec![format!("Drop Tablespace {}", d.name)],
        Statement::DropCollation(d) => vec![format!("Drop Collation {}", d.name)],
        Statement::DropType(d) => vec![format!("Drop Type {}", d.name)],
        Statement::DropDomain(d) => vec![format!("Drop Domain {}", d.name)],
        Statement::DropView(d) => vec![format!("Drop View {}", d.name)],
        Statement::DropMaterializedView(d) => vec![format!("Drop Materialized View {}", d.name)],
        Statement::AlterTable(a) => vec![format!("Alter Table {}", a.table)],
        Statement::AlterRole(a) => vec![format!("Alter Role {}", a.name)],
        Statement::AlterSequence(a) => vec![format!("Alter Sequence {}", a.name)],
        Statement::CreateIndex(c) => vec![format!("Create Index on {}", c.table)],
        Statement::DropIndex(d) => vec![format!("Drop Index {}", d.name)],
        Statement::Truncate(t) => vec![format!("Truncate {}", t.tables.join(", "))],
        Statement::DeclareCursor(d) => vec![format!("Declare Cursor {}", d.name)],
        Statement::Fetch(f) => vec![format!("Fetch {}", f.cursor)],
        Statement::AlterDatabase(a) => vec![format!("Alter Database {}", a.name)],
        Statement::Explain(e) => explain_statement(&e.statement),
        Statement::Analyze(a) => match &a.table {
            Some(table) => vec![format!("Analyze {table}")],
            None => vec!["Analyze".into()],
        },
        Statement::Comment(c) => vec![format!("Comment on {:?}", c.object)],
        Statement::SecurityLabel(s) => vec![format!("Security Label on {:?}", s.object)],
        Statement::AlterSystem(_) => vec!["Alter System".into()],
        Statement::Vacuum(v) => match &v.table {
            Some(table) => vec![format!("Vacuum {table}")],
            None => vec!["Vacuum".into()],
        },
        Statement::Reindex(r) => vec![format!("Reindex {:?}", r.target)],
        Statement::Cluster(c) => match &c.table {
            Some(table) => vec![format!("Cluster {table}")],
            None => vec!["Cluster".into()],
        },
        Statement::Checkpoint => vec!["Checkpoint".into()],
        Statement::Discard(d) => vec![format!("Discard {:?}", d.target)],
        Statement::Listen { channel } => vec![format!("Listen {channel}")],
        Statement::Notify { channel, .. } => vec![format!("Notify {channel}")],
        Statement::Unlisten { channel } => match channel {
            Some(channel) => vec![format!("Unlisten {channel}")],
            None => vec!["Unlisten all".into()],
        },
        Statement::LockTable(l) => vec![format!("Lock Table {}", l.tables.join(", "))],
        Statement::RefreshMaterializedView(r) => {
            vec![format!("Refresh Materialized View {}", r.name)]
        }
        Statement::Show { name } => vec![format!("Show {name}")],
        Statement::Set { name, .. } => vec![format!("Set {name}")],
        Statement::Grant(_) => vec!["Grant".into()],
        Statement::Revoke(_) => vec!["Revoke".into()],
        Statement::Begin
        | Statement::Commit
        | Statement::Rollback
        | Statement::Savepoint { .. }
        | Statement::ReleaseSavepoint { .. }
        | Statement::RollbackToSavepoint { .. }
        | Statement::Empty => vec!["Result".into()],
    }
}

fn exec_analyze(db: &Database, analyze: Analyze) -> Result<ExecResult, String> {
    if let Some(table) = &analyze.table {
        if !db.contains_table(table) {
            return Err(format!("relation \"{table}\" does not exist"));
        }
    }
    Ok(ExecResult::Command("ANALYZE".into()))
}

fn exec_comment(db: &mut Database, c: Comment) -> Result<ExecResult, String> {
    match &c.object {
        CommentObject::Relation { name } => {
            if relation_oid_by_name(db, name).is_none() {
                return Err(format!("relation \"{name}\" does not exist"));
            }
        }
        CommentObject::Column { table, column } => {
            if column_number_by_name(db, table, column).is_none() {
                return Err(format!(
                    "column \"{column}\" of relation \"{table}\" does not exist"
                ));
            }
        }
    }
    db.set_comment(c.object, c.comment);
    Ok(ExecResult::Command("COMMENT".into()))
}

fn exec_security_label(db: &mut Database, s: SecurityLabel) -> Result<ExecResult, String> {
    match &s.object {
        CommentObject::Relation { name } => {
            if relation_oid_by_name(db, name).is_none() {
                return Err(format!("relation \"{name}\" does not exist"));
            }
        }
        CommentObject::Column { table, column } => {
            if column_number_by_name(db, table, column).is_none() {
                return Err(format!(
                    "column \"{column}\" of relation \"{table}\" does not exist"
                ));
            }
        }
    }
    db.set_security_label(s.provider, s.object, s.label);
    Ok(ExecResult::Command("SECURITY LABEL".into()))
}

fn exec_grant(db: &mut Database, g: Grant) -> Result<ExecResult, String> {
    apply_grant(db, &g.object, &g.grantees, false)?;
    Ok(ExecResult::Command("GRANT".into()))
}

fn exec_revoke(db: &mut Database, r: Revoke) -> Result<ExecResult, String> {
    apply_grant(db, &r.object, &r.grantees, true)?;
    Ok(ExecResult::Command("REVOKE".into()))
}

/// Apply a GRANT (`revoke == false`) or REVOKE (`revoke == true`) to the catalog.
fn apply_grant(
    db: &mut Database,
    object: &GrantObject,
    grantees: &[Grantee],
    revoke: bool,
) -> Result<(), String> {
    match object {
        GrantObject::Table { privileges, table } => {
            if db.table(table).is_none() && db.view(table).is_none() {
                return Err(format!("relation \"{table}\" does not exist"));
            }
            let privs: Vec<&str> = match privileges {
                Privileges::All => vec![
                    "SELECT",
                    "INSERT",
                    "UPDATE",
                    "DELETE",
                    "TRUNCATE",
                    "REFERENCES",
                    "TRIGGER",
                ],
                Privileges::List(list) => list.iter().map(|p| p.as_str()).collect(),
            };
            for grantee in grantees {
                let name = grantee_name(grantee);
                for priv_name in &privs {
                    if revoke {
                        db.revoke_table_privilege(table, name, priv_name);
                    } else {
                        db.grant_table_privilege(table, name, priv_name);
                    }
                }
            }
        }
        GrantObject::Roles { roles } => {
            for role in roles {
                for grantee in grantees {
                    let Grantee::Role(member) = grantee else {
                        return Err("cannot grant role membership to PUBLIC".into());
                    };
                    if revoke {
                        db.revoke_role_membership(member, role);
                    } else {
                        db.grant_role_membership(member, role);
                    }
                }
            }
        }
    }
    Ok(())
}

fn grantee_name(grantee: &Grantee) -> &str {
    match grantee {
        Grantee::Role(name) => name,
        Grantee::Public => "PUBLIC",
    }
}

fn exec_alter_system(db: &mut Database, a: AlterSystem) -> Result<ExecResult, String> {
    match a.action {
        AlterSystemAction::Set { name, value } => db.set_system_setting(name, value),
        AlterSystemAction::Reset { name } => db.reset_system_setting(name.as_deref()),
    }
    Ok(ExecResult::Command("ALTER SYSTEM".into()))
}

fn exec_vacuum(db: &mut Database, vacuum: Vacuum) -> Result<ExecResult, String> {
    if let Some(table) = &vacuum.table {
        db.vacuum_table_storage(table)?;
    } else {
        db.vacuum_storage();
    }
    Ok(ExecResult::Command("VACUUM".into()))
}

fn exec_reindex(db: &Database, reindex: Reindex) -> Result<ExecResult, String> {
    match &reindex.target {
        ReindexTarget::Table(table) => {
            if !db.contains_table(table) {
                return Err(format!("relation \"{table}\" does not exist"));
            }
        }
        ReindexTarget::Index(index) => {
            let found = db
                .table_names()
                .iter()
                .filter_map(|table| db.table(table))
                .any(|table| table.indexes().iter().any(|idx| idx.name == *index));
            if !found {
                return Err(format!("index \"{index}\" does not exist"));
            }
        }
        ReindexTarget::Database(_) | ReindexTarget::System(_) => {}
    }
    Ok(ExecResult::Command("REINDEX".into()))
}

fn exec_cluster(db: &Database, cluster: Cluster) -> Result<ExecResult, String> {
    if let Some(table) = &cluster.table {
        let table_ref = db
            .table(table)
            .ok_or_else(|| format!("relation \"{table}\" does not exist"))?;
        if let Some(index) = &cluster.index {
            let found = table_ref.indexes().iter().any(|idx| idx.name == *index);
            if !found {
                return Err(format!("index \"{index}\" does not exist"));
            }
        }
    }
    Ok(ExecResult::Command("CLUSTER".into()))
}

fn exec_lock_table(db: &Database, lock: LockTable) -> Result<ExecResult, String> {
    for table in &lock.tables {
        if !db.contains_table(table) {
            return Err(format!("relation \"{table}\" does not exist"));
        }
    }
    Ok(ExecResult::Command("LOCK TABLE".into()))
}

fn discard_tag(discard: &Discard) -> &'static str {
    match discard.target {
        DiscardTarget::All => "DISCARD ALL",
        DiscardTarget::Plans => "DISCARD PLANS",
        DiscardTarget::Sequences => "DISCARD SEQUENCES",
        DiscardTarget::Temp => "DISCARD TEMP",
    }
}

fn explain_select(sel: &Select) -> Vec<String> {
    let mut lines = Vec::new();
    let root = if sel.from.is_none() {
        "Result".to_string()
    } else if sel.from.as_ref().is_some_and(|from| !from.joins.is_empty()) {
        "Nested Loop".to_string()
    } else if let Some(from) = &sel.from {
        if !from.base.args.is_empty() {
            format!("Function Scan on {}", from.base.name)
        } else {
            format!("Seq Scan on {}", from.base.name)
        }
    } else {
        "Result".to_string()
    };
    lines.push(root);
    if sel.filter.is_some() {
        lines.push("  Filter".into());
    }
    if !sel.group_by.is_empty() {
        lines.push("  HashAggregate".into());
    }
    if sel.distinct || !sel.distinct_on.is_empty() {
        lines.push("  Unique".into());
    }
    if !sel.order_by.is_empty() {
        lines.push("  Sort".into());
    }
    if sel.limit.is_some() {
        lines.push("  Limit".into());
    }
    lines
}

/// Derive the output field list of a SELECT from the schema alone.
fn select_fields(db: &Database, sel: &Select) -> Result<Vec<FieldDescription>, String> {
    let ctes = describe_ctes(db, &sel.ctes)?;
    select_fields_with_ctes(db, sel, &ctes)
}

type CteMap = HashMap<String, CteRelation>;

#[derive(Debug, Clone)]
struct CteRelation {
    fields: Vec<(String, DataType)>,
    rows: Vec<Vec<Value>>,
}

fn describe_ctes(db: &Database, ctes: &[Cte]) -> Result<CteMap, String> {
    let mut map = CteMap::new();
    for cte in ctes {
        let fields = select_fields_with_ctes(db, &cte.select, &map)?;
        let mut fields: Vec<(String, DataType)> = fields
            .into_iter()
            .map(|field| (field.name, field.data_type))
            .collect();
        apply_cte_column_aliases(cte, &mut fields)?;
        map.insert(
            cte.name.clone(),
            CteRelation {
                fields,
                rows: Vec::new(),
            },
        );
    }
    Ok(map)
}

fn apply_cte_column_aliases(cte: &Cte, fields: &mut [(String, DataType)]) -> Result<(), String> {
    if cte.columns.is_empty() {
        return Ok(());
    }
    if cte.columns.len() != fields.len() {
        return Err(format!(
            "WITH query \"{}\" has {} columns available but {} columns specified",
            cte.name,
            fields.len(),
            cte.columns.len()
        ));
    }
    for (field, alias) in fields.iter_mut().zip(&cte.columns) {
        field.0 = alias.clone();
    }
    Ok(())
}

fn cte_qualified_schema(cte: &CteRelation, qualifier: &str) -> (Vec<String>, Vec<DataType>) {
    let mut names = Vec::with_capacity(cte.fields.len());
    let mut types = Vec::with_capacity(cte.fields.len());
    for (name, data_type) in &cte.fields {
        names.push(format!("{qualifier}.{name}"));
        types.push(*data_type);
    }
    (names, types)
}

fn select_fields_with_ctes(
    db: &Database,
    sel: &Select,
    ctes: &CteMap,
) -> Result<Vec<FieldDescription>, String> {
    let (col_names, col_types) = match &sel.from {
        Some(fc) => from_schema_with_ctes(db, fc, ctes)?,
        None => (Vec::new(), Vec::new()),
    };
    let mut fields = Vec::new();
    for item in &sel.projection {
        match item {
            SelectItem::Wildcard => {
                for (i, name) in col_names.iter().enumerate() {
                    fields.push(FieldDescription {
                        name: bare_name(name),
                        data_type: col_types[i],
                    });
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

fn from_schema_with_ctes(
    db: &Database,
    from: &FromClause,
    ctes: &CteMap,
) -> Result<(Vec<String>, Vec<DataType>), String> {
    let (mut names, mut types) = resolve_source_schema(db, &from.base, ctes)?;
    for j in &from.joins {
        let (rn, rt) = resolve_source_schema(db, &j.table, ctes)?;
        names.extend(rn);
        types.extend(rt);
    }
    Ok((names, types))
}

fn resolve_source_schema(
    db: &Database,
    tref: &TableRef,
    ctes: &CteMap,
) -> Result<(Vec<String>, Vec<DataType>), String> {
    if let Some(cte) = ctes.get(&tref.name) {
        return Ok(cte_qualified_schema(cte, tref.qualifier()));
    }
    if !tref.args.is_empty() {
        let (names, types, _) = virtual_set_returning_function(tref)?;
        return Ok((names, types));
    }
    if tref.schema.as_deref() == Some("information_schema") {
        let (names, types, _) = virtual_information_schema(db, &tref.name, tref.qualifier())?;
        return Ok((names, types));
    }
    if tref.schema.as_deref() == Some("pg_catalog") || is_pg_catalog_table(&tref.name) {
        let (names, types, _) = virtual_pg_catalog(db, &tref.name, tref.qualifier())?;
        return Ok((names, types));
    }
    if let Some(view) = db.view(&tref.name) {
        let mut names = Vec::with_capacity(view.fields.len());
        let mut types = Vec::with_capacity(view.fields.len());
        for (name, data_type) in &view.fields {
            names.push(format!("{}.{}", tref.qualifier(), name));
            types.push(*data_type);
        }
        return Ok((names, types));
    }
    if let Some(view) = db.materialized_view(&tref.name) {
        let mut names = Vec::with_capacity(view.fields.len());
        let mut types = Vec::with_capacity(view.fields.len());
        for (name, data_type) in &view.fields {
            names.push(format!("{}.{}", tref.qualifier(), name));
            types.push(*data_type);
        }
        return Ok((names, types));
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
    db: &mut Database,
    from: &FromClause,
    filter: Option<&Expr>,
) -> Result<(Vec<String>, Vec<DataType>, Vec<Vec<Value>>), String> {
    let ctes = CteMap::new();
    build_source_with_ctes(db, from, filter, &ctes)
}

fn build_source_with_ctes(
    db: &mut Database,
    from: &FromClause,
    filter: Option<&Expr>,
    ctes: &CteMap,
) -> Result<(Vec<String>, Vec<DataType>, Vec<Vec<Value>>), String> {
    // Base pruning is only safe to drive from the WHERE clause when there is
    // no join (a join's WHERE could reference other tables' columns, and the
    // filter runs after the join). With joins, the base is fully scanned and
    // any indexed join is handled per-inner-side below.
    let base_filter = if from.joins.is_empty() { filter } else { None };
    let (mut names, mut types, mut rows) = resolve_base_source(db, &from.base, base_filter, ctes)?;

    for j in &from.joins {
        // A LATERAL join re-evaluates its right side per left row, with the
        // left row's columns substituted into the lateral subquery / function
        // arguments. Handled separately from the ordinary (non-correlated) join.
        if j.table.lateral {
            let (n, t, r) = eval_lateral_join(db, j, &names, &rows, ctes)?;
            names = n;
            types.extend(t);
            rows = r;
            continue;
        }
        let (right_names, right_types, _) = resolve_source_table(db, &j.table, ctes)?;
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
        let (_, _, right_rows) = resolve_source_table(db, &j.table, ctes)?;
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
                    let mut combo: Vec<Value> =
                        std::iter::repeat_n(Value::Null, left_width).collect();
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

/// Evaluate a LATERAL join: for each row of the left side, specialise the
/// right-side subquery (or set-returning function arguments) with that row's
/// column values, evaluate it, and combine. Supports INNER (`ON`/CROSS) and
/// LEFT lateral joins.
fn eval_lateral_join(
    db: &mut Database,
    j: &Join,
    left_names: &[String],
    left_rows: &[Vec<Value>],
    ctes: &CteMap,
) -> Result<(Vec<String>, Vec<DataType>, Vec<Vec<Value>>), String> {
    // Resolve the right schema from a specialised reference. The correlated
    // values cannot change the column names/types, so a NULL-filled left row
    // suffices when there are no actual left rows to drive evaluation.
    let schema_row: Vec<Value> = left_rows
        .first()
        .cloned()
        .unwrap_or_else(|| vec![Value::Null; left_names.len()]);
    let schema_ref = specialize_table_ref(db, &j.table, left_names, &schema_row, ctes)?;
    let (right_names, right_types, _) = resolve_source_table(db, &schema_ref, ctes)?;
    let right_width = right_names.len();

    let mut combined_names = left_names.to_vec();
    combined_names.extend(right_names.iter().cloned());

    let mut joined = Vec::new();
    for left_row in left_rows {
        // Build a per-row specialised reference and resolve its rows.
        let specialised = specialize_table_ref(db, &j.table, left_names, left_row, ctes)?;
        let (_, _, right_rows) = resolve_source_table(db, &specialised, ctes)?;

        let mut matched = false;
        for right_row in &right_rows {
            let mut combo = left_row.clone();
            combo.extend(right_row.iter().cloned());
            let on_true = match &j.on {
                None => true, // CROSS / `LATERAL (...)` with no ON
                Some(pred) => eval_expr(pred, &combined_names, &combo)?.is_true(),
            };
            if on_true {
                joined.push(combo);
                matched = true;
            }
        }
        if !matched && matches!(j.kind, JoinKind::Left | JoinKind::Full) {
            let mut combo = left_row.clone();
            combo.extend(std::iter::repeat_n(Value::Null, right_width));
            joined.push(combo);
        }
    }

    let mut types: Vec<DataType> = Vec::new();
    types.extend(right_types);
    Ok((combined_names, types, joined))
}

/// Produce a copy of `tref` with any correlated references (in a derived-table
/// subquery, or in set-returning function arguments) substituted with the
/// values from `left_row`.
fn specialize_table_ref(
    db: &mut Database,
    tref: &TableRef,
    left_names: &[String],
    left_row: &[Value],
    ctes: &CteMap,
) -> Result<TableRef, String> {
    let mut out = tref.clone();
    if let Some(sub) = &out.subquery {
        let specialised = specialize_select(db, sub, left_names, left_row, ctes)?;
        out.subquery = Some(Box::new(specialised));
    }
    for arg in &mut out.args {
        let mut e = arg.clone();
        specialize_expr(db, &mut e, &[], left_names, left_row, ctes)?;
        *arg = e;
    }
    Ok(out)
}

/// Resolve the base table of a FROM clause, pruning to index candidates when
/// the WHERE predicate permits and there is no join that could reference other
/// tables (we still re-check the predicate later, so this only narrows rows).
fn resolve_base_source(
    db: &mut Database,
    tref: &TableRef,
    filter: Option<&Expr>,
    ctes: &CteMap,
) -> Result<(Vec<String>, Vec<DataType>, Vec<Vec<Value>>), String> {
    if ctes.contains_key(&tref.name) {
        return resolve_source_table(db, tref, ctes);
    }
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
                let rows = positions
                    .into_iter()
                    .map(|p| table.rows[p].clone())
                    .collect();
                return Ok((names, types, rows));
            }
        }
    }
    resolve_source_table(db, tref, ctes)
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
        let table = db
            .table(&self.table)
            .expect("inner table existed at planning");
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
    let Expr::Binary {
        op: BinaryOp::Eq,
        left,
        right,
    } = on
    else {
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
        Some(IndexedJoinProbe {
            table: j.table.name.clone(),
            right_col,
            left_col,
        })
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
    db: &mut Database,
    tref: &TableRef,
    ctes: &CteMap,
) -> Result<(Vec<String>, Vec<DataType>, Vec<Vec<Value>>), String> {
    // A derived table: a parenthesised subquery in FROM.
    if let Some(sub) = &tref.subquery {
        return resolve_subquery_source(db, sub, tref.qualifier(), ctes);
    }
    if let Some(cte) = ctes.get(&tref.name) {
        let (names, types) = cte_qualified_schema(cte, tref.qualifier());
        return Ok((names, types, cte.rows.clone()));
    }
    if !tref.args.is_empty() {
        return virtual_set_returning_function(tref);
    }
    if tref.schema.as_deref() == Some("information_schema") {
        return virtual_information_schema(db, &tref.name, tref.qualifier());
    }
    if tref.schema.as_deref() == Some("pg_catalog") || is_pg_catalog_table(&tref.name) {
        return virtual_pg_catalog(db, &tref.name, tref.qualifier());
    }
    if let Some(view) = db.view(&tref.name).cloned() {
        let ExecResult::Rows { rows, .. } = exec_select(db, view.select)? else {
            return Err(format!("view \"{}\" did not produce rows", tref.name));
        };
        let mut names = Vec::with_capacity(view.fields.len());
        let mut types = Vec::with_capacity(view.fields.len());
        for (name, data_type) in &view.fields {
            names.push(format!("{}.{}", tref.qualifier(), name));
            types.push(*data_type);
        }
        return Ok((names, types, rows));
    }
    if let Some(view) = db.materialized_view(&tref.name) {
        let mut names = Vec::with_capacity(view.fields.len());
        let mut types = Vec::with_capacity(view.fields.len());
        for (name, data_type) in &view.fields {
            names.push(format!("{}.{}", tref.qualifier(), name));
            types.push(*data_type);
        }
        return Ok((names, types, view.rows.clone()));
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

/// Execute a derived-table subquery and return its rows with each output column
/// qualified by `qual` (the derived table's alias).
fn resolve_subquery_source(
    db: &mut Database,
    sub: &Select,
    qual: &str,
    ctes: &CteMap,
) -> Result<(Vec<String>, Vec<DataType>, Vec<Vec<Value>>), String> {
    let ExecResult::Rows { fields, rows, .. } = exec_select_with_ctes(db, sub.clone(), ctes)? else {
        return Err("subquery in FROM did not produce rows".into());
    };
    let mut names = Vec::with_capacity(fields.len());
    let mut types = Vec::with_capacity(fields.len());
    for f in &fields {
        names.push(format!("{qual}.{}", f.name));
        types.push(f.data_type);
    }
    Ok((names, types, rows))
}

fn virtual_set_returning_function(
    tref: &TableRef,
) -> Result<(Vec<String>, Vec<DataType>, Vec<Vec<Value>>), String> {
    match tref.name.to_ascii_lowercase().as_str() {
        "generate_series" => {
            let values = eval_generate_series(&tref.args)?;
            let name = format!("{}.generate_series", tref.qualifier());
            let rows = values.into_iter().map(|v| vec![Value::Int(v)]).collect();
            Ok((vec![name], vec![DataType::Int8], rows))
        }
        other => Err(format!("set-returning function {other}() is not supported")),
    }
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
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => {
            if let (Some(col), Some(v)) = (column_index_of(left, target), const_value(right)) {
                return Some((col, IndexPlan::Eq(v)));
            }
            if let (Some(col), Some(v)) = (column_index_of(right, target), const_value(left)) {
                return Some((col, IndexPlan::Eq(v)));
            }
            None
        }
        // Range comparisons: `col < c`, `c > col`, etc.
        Expr::Binary {
            op: op @ (BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq),
            left,
            right,
        } => {
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
        Expr::InList {
            expr,
            list,
            negated: false,
        } => {
            let col = column_index_of(expr, target)?;
            let mut vals = Vec::with_capacity(list.len());
            for item in list {
                vals.push(const_value(item)?);
            }
            Some((col, IndexPlan::In(vals)))
        }
        // `col BETWEEN lo AND hi` — inclusive range on both ends.
        Expr::Between {
            expr,
            low,
            high,
            negated: false,
        } => {
            let col = column_index_of(expr, target)?;
            let lo = const_value(low)?;
            let hi = const_value(high)?;
            Some((
                col,
                IndexPlan::Range(
                    Some(Bound {
                        value: lo,
                        inclusive: true,
                    }),
                    Some(Bound {
                        value: hi,
                        inclusive: true,
                    }),
                ),
            ))
        }
        // AND: try each side; the first index-eligible conjunct wins.
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => plan_index_access(left, target).or_else(|| plan_index_access(right, target)),
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
        BinaryOp::Lt => IndexPlan::Range(
            None,
            Some(Bound {
                value: v,
                inclusive: false,
            }),
        ),
        BinaryOp::LtEq => IndexPlan::Range(
            None,
            Some(Bound {
                value: v,
                inclusive: true,
            }),
        ),
        BinaryOp::Gt => IndexPlan::Range(
            Some(Bound {
                value: v,
                inclusive: false,
            }),
            None,
        ),
        BinaryOp::GtEq => IndexPlan::Range(
            Some(Bound {
                value: v,
                inclusive: true,
            }),
            None,
        ),
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
    // Prefer a multi-column / expression / partial index when one matches the
    // predicate. These return exact-or-superset candidate rows; the executor
    // re-checks the original filter, so a superset is always safe.
    if let Some(positions) = advanced_index_positions(table, filter) {
        let mut seen = std::collections::HashSet::new();
        return Some(positions.into_iter().filter(|p| seen.insert(*p)).collect());
    }
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

/// Flatten an AND-tree into its top-level conjuncts.
fn and_conjuncts<'a>(filter: &'a Expr, out: &mut Vec<&'a Expr>) {
    match filter {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            and_conjuncts(left, out);
            and_conjuncts(right, out);
        }
        other => out.push(other),
    }
}

/// Equality facts extracted from a WHERE predicate: a map from a table column
/// position to a constant value it is equated with, plus the raw `(lhs, rhs)`
/// equality pairs (for expression-index matching).
struct EqFacts<'a> {
    by_column: std::collections::HashMap<usize, Value>,
    pairs: Vec<(&'a Expr, &'a Expr)>,
}

/// Collect `col = const` and `expr = const` equalities from the AND-conjuncts.
fn collect_eq_facts<'a>(conjuncts: &[&'a Expr], target: &Table) -> EqFacts<'a> {
    let mut by_column = std::collections::HashMap::new();
    let mut pairs = Vec::new();
    for c in conjuncts {
        if let Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } = c
        {
            pairs.push((left.as_ref(), right.as_ref()));
            // Normalise `col = const` (either operand order) into the map.
            if let (Some(col), Some(v)) = (column_index_of(left, target), const_value(right)) {
                by_column.entry(col).or_insert(v);
            } else if let (Some(col), Some(v)) =
                (column_index_of(right, target), const_value(left))
            {
                by_column.entry(col).or_insert(v);
            }
        }
    }
    EqFacts { by_column, pairs }
}

/// Try to satisfy `filter` using a multi-column, expression, or partial index.
/// Returns candidate row positions (a superset is acceptable — the caller
/// re-checks the filter), or `None` if no such index applies.
fn advanced_index_positions(table: &Table, filter: &Expr) -> Option<Vec<usize>> {
    let mut conjuncts = Vec::new();
    and_conjuncts(filter, &mut conjuncts);
    let facts = collect_eq_facts(&conjuncts, table);

    for (i, idx) in table.indexes().iter().enumerate() {
        // Partial index: only usable when the query WHERE contains the index's
        // predicate verbatim as one of its conjuncts (a conservative implication
        // check that is always sound).
        if idx
            .predicate
            .as_ref()
            .is_some_and(|pred| !conjuncts.contains(&pred))
        {
            continue;
        }

        // Expression index: match when the same expression appears in a
        // `expr = const` (or `const = expr`) conjunct.
        if let Some(iexpr) = &idx.expr {
            for (l, r) in &facts.pairs {
                let key = if exprs_equal(l, iexpr) {
                    const_value(r)
                } else if exprs_equal(r, iexpr) {
                    const_value(l)
                } else {
                    None
                };
                if let Some(v) = key {
                    return Some(table.index_eq_expr(i, &v));
                }
            }
            continue;
        }

        // Single-column index: handled here only when it is *partial* (the
        // plain case is left to the single-column planner below). When the
        // partial predicate matched, narrow further by any equality on the
        // column, else return every row the partial index holds.
        if idx.columns.len() == 1 {
            if idx.predicate.is_none() {
                continue;
            }
            let col = idx.columns[0];
            if let Some(v) = facts.by_column.get(&col) {
                return Some(table.index_eq_multi(i, std::slice::from_ref(v)));
            }
            return Some(table.index_all_positions(i));
        }

        // Multi-column index: need at least a full-key or leading-prefix match
        // of equality predicates.
        // Build the longest leading prefix of equality-bound columns.
        let mut key = Vec::new();
        for &col in &idx.columns {
            match facts.by_column.get(&col) {
                Some(v) => key.push(v.clone()),
                None => break,
            }
        }
        if key.is_empty() {
            continue;
        }
        if key.len() == idx.columns.len() {
            return Some(table.index_eq_multi(i, &key));
        }
        // Leading-prefix match: use a prefix scan over the B-tree.
        if idx.method == crate::index::IndexMethod::Btree {
            return Some(table.index_prefix_multi(i, &key));
        }
    }
    None
}

/// Structural equality of two expressions, used to match an indexed expression
/// against a predicate's operand. Relies on the AST's derived `PartialEq`.
fn exprs_equal(a: &Expr, b: &Expr) -> bool {
    a == b
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
                .map(|t| {
                    let table_type = match db
                        .table(&t)
                        .map(|table| table.persistence())
                        .unwrap_or(TablePersistence::Permanent)
                    {
                        TablePersistence::Temporary => "LOCAL TEMPORARY",
                        _ => "BASE TABLE",
                    };
                    vec![
                        txt("postgres"),
                        txt("public"),
                        Value::Text(t),
                        txt(table_type),
                    ]
                })
                .chain(
                    db.view_names()
                        .into_iter()
                        .map(|v| vec![txt("postgres"), txt("public"), Value::Text(v), txt("VIEW")]),
                )
                .chain(db.materialized_view_names().into_iter().map(|v| {
                    vec![
                        txt("postgres"),
                        txt("public"),
                        Value::Text(v),
                        txt("MATERIALIZED VIEW"),
                    ]
                }))
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
    matches!(
        name.to_ascii_lowercase().as_str(),
        "pg_class"
            | "pg_namespace"
            | "pg_am"
            | "pg_type"
            | "pg_attribute"
            | "pg_index"
            | "pg_constraint"
            | "pg_sequence"
            | "pg_attrdef"
            | "pg_description"
            | "pg_seclabel"
            | "pg_depend"
            | "pg_roles"
            | "pg_auth_members"
            | "pg_user"
            | "pg_database"
            | "pg_tablespace"
            | "pg_collation"
            | "pg_settings"
            | "pg_proc"
            | "pg_operator"
            | "pg_locks"
            | "pg_extension"
    )
}

/// OID assigned to the `public` namespace (matches real PostgreSQL).
const PUBLIC_NAMESPACE_OID: i64 = 2200;
/// OID of `pg_class`, used as `pg_description.classoid` for relations/columns.
const PG_CLASS_OID: i64 = 1259;
/// Base OID for synthesized user-table OIDs.
const USER_TABLE_OID_BASE: i64 = 16384;
/// Base OID for synthesized index relation OIDs.
const USER_INDEX_OID_BASE: i64 = 32768;
/// Base OID for synthesized table constraint OIDs.
const USER_CONSTRAINT_OID_BASE: i64 = 49152;
/// Base OID for synthesized column default OIDs.
const USER_ATTRDEF_OID_BASE: i64 = 57344;
/// Base OID for synthesized sequence relation OIDs.
const USER_SEQUENCE_OID_BASE: i64 = 65536;

fn user_table_oid(index: usize) -> i64 {
    USER_TABLE_OID_BASE + index as i64
}

fn user_index_oid(table_index: usize, index_index: usize) -> i64 {
    USER_INDEX_OID_BASE + (table_index as i64 * 100) + index_index as i64
}

fn user_constraint_oid(table_index: usize, index_index: usize) -> i64 {
    USER_CONSTRAINT_OID_BASE + (table_index as i64 * 100) + index_index as i64
}

fn user_attrdef_oid(table_index: usize, column_index: usize) -> i64 {
    USER_ATTRDEF_OID_BASE + (table_index as i64 * 100) + column_index as i64
}

fn user_sequence_oid(index: usize) -> i64 {
    USER_SEQUENCE_OID_BASE + index as i64
}

fn relation_oid_by_name(db: &Database, name: &str) -> Option<i64> {
    let table_names = db.table_names();
    if let Some(index) = table_names.iter().position(|table| table == name) {
        return Some(user_table_oid(index));
    }
    let view_names = db.view_names();
    if let Some(index) = view_names.iter().position(|view| view == name) {
        return Some(user_table_oid(table_names.len() + index));
    }
    let materialized_names = db.materialized_view_names();
    if let Some(index) = materialized_names.iter().position(|view| view == name) {
        return Some(user_table_oid(table_names.len() + view_names.len() + index));
    }
    db.sequences()
        .iter()
        .position(|sequence| sequence.name == name)
        .map(user_sequence_oid)
}

fn column_number_by_name(db: &Database, relation: &str, column: &str) -> Option<i64> {
    if let Some(table) = db.table(relation) {
        return table
            .columns
            .iter()
            .position(|c| c.name == column)
            .map(|idx| idx as i64 + 1);
    }
    if let Some(view) = db.view(relation) {
        return view
            .fields
            .iter()
            .position(|(name, _)| name == column)
            .map(|idx| idx as i64 + 1);
    }
    if let Some(view) = db.materialized_view(relation) {
        return view
            .fields
            .iter()
            .position(|(name, _)| name == column)
            .map(|idx| idx as i64 + 1);
    }
    None
}

fn relpersistence(persistence: TablePersistence) -> &'static str {
    match persistence {
        TablePersistence::Permanent => "p",
        TablePersistence::Unlogged => "u",
        TablePersistence::Temporary => "t",
    }
}

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
                ("relpersistence", DataType::Text),
            ];
            let mut rows = Vec::new();
            for (table_idx, table_name) in db.table_names().into_iter().enumerate() {
                let table_persistence = db
                    .table(&table_name)
                    .map(|table| relpersistence(table.persistence()))
                    .unwrap_or("p");
                rows.push(vec![
                    Value::Int(user_table_oid(table_idx)),
                    Value::Text(table_name.clone()),
                    Value::Int(PUBLIC_NAMESPACE_OID),
                    Value::Text("r".to_string()), // ordinary table
                    Value::Int(10),               // owner oid (= postgres)
                    Value::Int(0),                // access method
                    Value::Text(table_persistence.into()),
                ]);
                if let Some(table) = db.table(&table_name) {
                    for (index_idx, index) in table.indexes().iter().enumerate() {
                        rows.push(vec![
                            Value::Int(user_index_oid(table_idx, index_idx)),
                            Value::Text(index.name.clone()),
                            Value::Int(PUBLIC_NAMESPACE_OID),
                            Value::Text("i".to_string()), // index relation
                            Value::Int(10),
                            Value::Int(403), // btree access method oid
                            Value::Text(relpersistence(table.persistence()).into()),
                        ]);
                    }
                    let unique_base = table.indexes().len();
                    for (constraint_idx, constraint) in
                        table.unique_constraints().iter().enumerate()
                    {
                        rows.push(vec![
                            Value::Int(user_index_oid(table_idx, unique_base + constraint_idx)),
                            Value::Text(constraint.name.clone()),
                            Value::Int(PUBLIC_NAMESPACE_OID),
                            Value::Text("i".to_string()), // constraint-backed index relation
                            Value::Int(10),
                            Value::Int(403),
                            Value::Text(relpersistence(table.persistence()).into()),
                        ]);
                    }
                }
            }
            let view_base = db.table_names().len();
            for (view_idx, view_name) in db.view_names().into_iter().enumerate() {
                rows.push(vec![
                    Value::Int(user_table_oid(view_base + view_idx)),
                    Value::Text(view_name),
                    Value::Int(PUBLIC_NAMESPACE_OID),
                    Value::Text("v".to_string()), // view
                    Value::Int(10),
                    Value::Int(0),
                    Value::Text("p".into()),
                ]);
            }
            let materialized_base = view_base + db.view_names().len();
            for (view_idx, view_name) in db.materialized_view_names().into_iter().enumerate() {
                rows.push(vec![
                    Value::Int(user_table_oid(materialized_base + view_idx)),
                    Value::Text(view_name),
                    Value::Int(PUBLIC_NAMESPACE_OID),
                    Value::Text("m".to_string()), // materialized view
                    Value::Int(10),
                    Value::Int(0),
                    Value::Text("p".into()),
                ]);
            }
            for (sequence_idx, sequence) in db.sequences().into_iter().enumerate() {
                rows.push(vec![
                    Value::Int(user_sequence_oid(sequence_idx)),
                    Value::Text(sequence.name),
                    Value::Int(PUBLIC_NAMESPACE_OID),
                    Value::Text("S".to_string()), // sequence
                    Value::Int(10),
                    Value::Int(0),
                    Value::Text("p".into()),
                ]);
            }
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_namespace" => {
            let cols = [("oid", DataType::Int8), ("nspname", DataType::Text)];
            let rows = db
                .schemas()
                .into_iter()
                .enumerate()
                .map(|(i, schema)| {
                    let oid = match schema.as_str() {
                        "public" => PUBLIC_NAMESPACE_OID,
                        "pg_catalog" => 11,
                        "information_schema" => 99,
                        _ => 16000 + i as i64,
                    };
                    vec![Value::Int(oid), Value::Text(schema)]
                })
                .collect();
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_tablespace" => {
            let cols = [
                ("oid", DataType::Int8),
                ("spcname", DataType::Text),
                ("spcowner", DataType::Int8),
                ("spcacl", DataType::Text),
                ("spcoptions", DataType::Text),
                ("spclocation", DataType::Text),
            ];
            let rows = db
                .tablespaces()
                .into_iter()
                .map(|tablespace| {
                    vec![
                        Value::Int(tablespace.oid),
                        Value::Text(tablespace.name),
                        Value::Int(tablespace.owner),
                        Value::Null,
                        Value::Null,
                        Value::Text(tablespace.location),
                    ]
                })
                .collect();
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_collation" => {
            let cols = [
                ("oid", DataType::Int8),
                ("collname", DataType::Text),
                ("collnamespace", DataType::Int8),
                ("collowner", DataType::Int8),
                ("collprovider", DataType::Text),
                ("collisdeterministic", DataType::Bool),
                ("collencoding", DataType::Int4),
                ("collcollate", DataType::Text),
                ("collctype", DataType::Text),
                ("colliculocale", DataType::Text),
                ("collversion", DataType::Text),
            ];
            let rows = db
                .collations()
                .into_iter()
                .map(|collation| {
                    vec![
                        Value::Int(collation.oid),
                        Value::Text(collation.name),
                        Value::Int(collation.namespace),
                        Value::Int(collation.owner),
                        Value::Text(collation.provider),
                        Value::Bool(collation.deterministic),
                        Value::Int(collation.encoding),
                        Value::Text(collation.collate),
                        Value::Text(collation.ctype),
                        Value::Null,
                        Value::Null,
                    ]
                })
                .collect();
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_am" => {
            // Access methods: empty is fine (referenced only via LEFT JOIN).
            let cols = [("oid", DataType::Int8), ("amname", DataType::Text)];
            Ok(qualify_virtual(qualifier, &cols, Vec::new()))
        }
        "pg_type" => {
            let cols = [
                ("oid", DataType::Int8),
                ("typname", DataType::Text),
                ("typnamespace", DataType::Int8),
                ("typlen", DataType::Int2),
                ("typbyval", DataType::Bool),
                ("typtype", DataType::Text),
                ("typcategory", DataType::Text),
            ];
            let rows = DataType::ALL
                .iter()
                .map(|dt| {
                    vec![
                        Value::Int(dt.oid() as i64),
                        Value::Text(dt.pg_type_name().into()),
                        Value::Int(11),
                        Value::Int(dt.type_size() as i64),
                        Value::Bool(dt.type_size() > 0 && dt.type_size() <= 8),
                        Value::Text("b".into()),
                        Value::Text(type_category(*dt).into()),
                    ]
                })
                .collect();
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_attribute" => {
            let cols = [
                ("attrelid", DataType::Int8),
                ("attname", DataType::Text),
                ("atttypid", DataType::Int8),
                ("attlen", DataType::Int2),
                ("attnum", DataType::Int2),
                ("attnotnull", DataType::Bool),
                ("atttypmod", DataType::Int4),
                ("attisdropped", DataType::Bool),
                ("attidentity", DataType::Text),
                ("attgenerated", DataType::Text),
            ];
            let mut rows = Vec::new();
            for (table_idx, table_name) in db.table_names().into_iter().enumerate() {
                if let Some(table) = db.table(&table_name) {
                    for (column_idx, column) in table.columns.iter().enumerate() {
                        rows.push(vec![
                            Value::Int(user_table_oid(table_idx)),
                            Value::Text(column.name.clone()),
                            Value::Int(column.data_type.oid() as i64),
                            Value::Int(column.data_type.type_size() as i64),
                            Value::Int(column_idx as i64 + 1),
                            Value::Bool(column.not_null),
                            Value::Int(-1),
                            Value::Bool(false),
                            Value::Text(
                                if column.identity {
                                    if column.identity_always { "a" } else { "d" }
                                } else {
                                    ""
                                }
                                .into(),
                            ),
                            Value::Text(if column.generated.is_some() { "s" } else { "" }.into()),
                        ]);
                    }
                }
            }
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_index" => {
            let cols = [
                ("indexrelid", DataType::Int8),
                ("indrelid", DataType::Int8),
                ("indnatts", DataType::Int2),
                ("indnkeyatts", DataType::Int2),
                ("indisunique", DataType::Bool),
                ("indisprimary", DataType::Bool),
                ("indisvalid", DataType::Bool),
                ("indkey", DataType::Text),
            ];
            let mut rows = Vec::new();
            for (table_idx, table_name) in db.table_names().into_iter().enumerate() {
                if let Some(table) = db.table(&table_name) {
                    for (index_idx, index) in table.indexes().iter().enumerate() {
                        let is_pk = index.unique
                            && index
                                .leading_column()
                                .is_some_and(|c| table.columns[c].primary_key);
                        let natts = (index.columns.len() + index.include.len()).max(1) as i64;
                        let indkey = index
                            .columns
                            .iter()
                            .map(|c| (c + 1).to_string())
                            .collect::<Vec<_>>()
                            .join(" ");
                        rows.push(vec![
                            Value::Int(user_index_oid(table_idx, index_idx)),
                            Value::Int(user_table_oid(table_idx)),
                            Value::Int(natts),
                            Value::Int(index.columns.len().max(1) as i64),
                            Value::Bool(index.unique),
                            Value::Bool(is_pk),
                            Value::Bool(true),
                            Value::Text(indkey),
                        ]);
                    }
                    let unique_base = table.indexes().len();
                    for (constraint_idx, constraint) in
                        table.unique_constraints().iter().enumerate()
                    {
                        let indkey = constraint
                            .columns
                            .iter()
                            .map(|column| (column + 1).to_string())
                            .collect::<Vec<_>>()
                            .join(" ");
                        rows.push(vec![
                            Value::Int(user_index_oid(table_idx, unique_base + constraint_idx)),
                            Value::Int(user_table_oid(table_idx)),
                            Value::Int(constraint.columns.len() as i64),
                            Value::Int(constraint.columns.len() as i64),
                            Value::Bool(true),
                            Value::Bool(constraint.primary_key),
                            Value::Bool(true),
                            Value::Text(indkey),
                        ]);
                    }
                }
            }
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_constraint" => {
            let cols = [
                ("oid", DataType::Int8),
                ("conname", DataType::Text),
                ("connamespace", DataType::Int8),
                ("contype", DataType::Text),
                ("conrelid", DataType::Int8),
                ("conindid", DataType::Int8),
                ("conkey", DataType::Text),
                ("convalidated", DataType::Bool),
            ];
            let mut rows = Vec::new();
            for (table_idx, table_name) in db.table_names().into_iter().enumerate() {
                if let Some(table) = db.table(&table_name) {
                    for (index_idx, index) in table.indexes().iter().enumerate() {
                        if !index.unique {
                            continue;
                        }
                        let Some(col) = index.leading_column() else {
                            continue;
                        };
                        let column = &table.columns[col];
                        rows.push(vec![
                            Value::Int(user_constraint_oid(table_idx, index_idx)),
                            Value::Text(index.name.clone()),
                            Value::Int(PUBLIC_NAMESPACE_OID),
                            Value::Text(if column.primary_key { "p" } else { "u" }.into()),
                            Value::Int(user_table_oid(table_idx)),
                            Value::Int(user_index_oid(table_idx, index_idx)),
                            Value::Text((col + 1).to_string()),
                            Value::Bool(true),
                        ]);
                    }
                    let unique_base = table.indexes().len();
                    for (constraint_idx, constraint) in
                        table.unique_constraints().iter().enumerate()
                    {
                        let catalog_idx = unique_base + constraint_idx;
                        let conkey = constraint
                            .columns
                            .iter()
                            .map(|column| (column + 1).to_string())
                            .collect::<Vec<_>>()
                            .join(" ");
                        rows.push(vec![
                            Value::Int(user_constraint_oid(table_idx, catalog_idx)),
                            Value::Text(constraint.name.clone()),
                            Value::Int(PUBLIC_NAMESPACE_OID),
                            Value::Text(if constraint.primary_key { "p" } else { "u" }.into()),
                            Value::Int(user_table_oid(table_idx)),
                            Value::Int(user_index_oid(table_idx, catalog_idx)),
                            Value::Text(conkey),
                            Value::Bool(true),
                        ]);
                    }
                    let check_base = table.indexes().len() + table.unique_constraints().len();
                    for (check_idx, constraint) in table.check_constraints().iter().enumerate() {
                        rows.push(vec![
                            Value::Int(user_constraint_oid(table_idx, check_base + check_idx)),
                            Value::Text(constraint.name.clone()),
                            Value::Int(PUBLIC_NAMESPACE_OID),
                            Value::Text("c".into()),
                            Value::Int(user_table_oid(table_idx)),
                            Value::Int(0),
                            Value::Text(String::new()),
                            Value::Bool(constraint.validated),
                        ]);
                    }
                    let fk_base = check_base + table.check_constraints().len();
                    for (fk_idx, constraint) in table.foreign_key_constraints().iter().enumerate() {
                        rows.push(vec![
                            Value::Int(user_constraint_oid(table_idx, fk_base + fk_idx)),
                            Value::Text(constraint.name.clone()),
                            Value::Int(PUBLIC_NAMESPACE_OID),
                            Value::Text("f".into()),
                            Value::Int(user_table_oid(table_idx)),
                            Value::Int(0),
                            Value::Text((constraint.column + 1).to_string()),
                            Value::Bool(constraint.validated),
                        ]);
                    }
                }
            }
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_sequence" => {
            let cols = [
                ("seqrelid", DataType::Int8),
                ("seqtypid", DataType::Int8),
                ("seqstart", DataType::Int8),
                ("seqincrement", DataType::Int8),
                ("seqmax", DataType::Int8),
                ("seqmin", DataType::Int8),
                ("seqcache", DataType::Int8),
                ("seqcycle", DataType::Bool),
            ];
            let rows = db
                .sequences()
                .into_iter()
                .enumerate()
                .map(|(idx, sequence)| {
                    vec![
                        Value::Int(user_sequence_oid(idx)),
                        Value::Int(DataType::Int8.oid() as i64),
                        Value::Int(sequence.start),
                        Value::Int(sequence.increment),
                        Value::Int(i64::MAX),
                        Value::Int(1),
                        Value::Int(1),
                        Value::Bool(false),
                    ]
                })
                .collect();
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_attrdef" => {
            let cols = [
                ("oid", DataType::Int8),
                ("adrelid", DataType::Int8),
                ("adnum", DataType::Int2),
                ("adbin", DataType::Text),
            ];
            let mut rows = Vec::new();
            for (table_idx, table_name) in db.table_names().into_iter().enumerate() {
                if let Some(table) = db.table(&table_name) {
                    for (column_idx, column) in table.columns.iter().enumerate() {
                        if let Some(default) = column.default.as_ref().or(column.generated.as_ref())
                        {
                            rows.push(vec![
                                Value::Int(user_attrdef_oid(table_idx, column_idx)),
                                Value::Int(user_table_oid(table_idx)),
                                Value::Int(column_idx as i64 + 1),
                                Value::Text(expr_to_sql(default)),
                            ]);
                        }
                    }
                }
            }
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_description" => {
            let cols = [
                ("objoid", DataType::Int8),
                ("classoid", DataType::Int8),
                ("objsubid", DataType::Int4),
                ("description", DataType::Text),
            ];
            let mut rows = Vec::new();
            for (object, description) in db.comments() {
                match object {
                    CommentObject::Relation { name } => {
                        if let Some(objoid) = relation_oid_by_name(db, &name) {
                            rows.push(vec![
                                Value::Int(objoid),
                                Value::Int(PG_CLASS_OID),
                                Value::Int(0),
                                Value::Text(description),
                            ]);
                        }
                    }
                    CommentObject::Column { table, column } => {
                        if let (Some(objoid), Some(objsubid)) = (
                            relation_oid_by_name(db, &table),
                            column_number_by_name(db, &table, &column),
                        ) {
                            rows.push(vec![
                                Value::Int(objoid),
                                Value::Int(PG_CLASS_OID),
                                Value::Int(objsubid),
                                Value::Text(description),
                            ]);
                        }
                    }
                }
            }
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_seclabel" => {
            let cols = [
                ("objoid", DataType::Int8),
                ("classoid", DataType::Int8),
                ("objsubid", DataType::Int4),
                ("provider", DataType::Text),
                ("label", DataType::Text),
            ];
            let mut rows = Vec::new();
            for (provider, object, label) in db.security_labels() {
                match object {
                    CommentObject::Relation { name } => {
                        if let Some(objoid) = relation_oid_by_name(db, &name) {
                            rows.push(vec![
                                Value::Int(objoid),
                                Value::Int(PG_CLASS_OID),
                                Value::Int(0),
                                Value::Text(provider),
                                Value::Text(label),
                            ]);
                        }
                    }
                    CommentObject::Column { table, column } => {
                        if let (Some(objoid), Some(objsubid)) = (
                            relation_oid_by_name(db, &table),
                            column_number_by_name(db, &table, &column),
                        ) {
                            rows.push(vec![
                                Value::Int(objoid),
                                Value::Int(PG_CLASS_OID),
                                Value::Int(objsubid),
                                Value::Text(provider),
                                Value::Text(label),
                            ]);
                        }
                    }
                }
            }
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_depend" => {
            let cols = [
                ("classid", DataType::Int8),
                ("objid", DataType::Int8),
                ("objsubid", DataType::Int4),
                ("refclassid", DataType::Int8),
                ("refobjid", DataType::Int8),
                ("refobjsubid", DataType::Int4),
                ("deptype", DataType::Text),
            ];
            Ok(qualify_virtual(qualifier, &cols, Vec::new()))
        }
        "pg_roles" => {
            let cols = [
                ("oid", DataType::Int8),
                ("rolname", DataType::Text),
                ("rolsuper", DataType::Bool),
                ("rolinherit", DataType::Bool),
                ("rolcreaterole", DataType::Bool),
                ("rolcreatedb", DataType::Bool),
                ("rolcanlogin", DataType::Bool),
                ("rolreplication", DataType::Bool),
                ("rolconnlimit", DataType::Int4),
                ("rolpassword", DataType::Text),
                ("rolvaliduntil", DataType::TimestampTz),
                ("rolbypassrls", DataType::Bool),
            ];
            let rows = db
                .roles()
                .into_iter()
                .map(|role| {
                    vec![
                        Value::Int(role.oid),
                        Value::Text(role.name),
                        Value::Bool(role.superuser),
                        Value::Bool(role.inherit),
                        Value::Bool(role.create_role),
                        Value::Bool(role.create_db),
                        Value::Bool(role.login),
                        Value::Bool(role.replication),
                        Value::Int(role.connection_limit),
                        role.password.map(Value::Text).unwrap_or(Value::Null),
                        role.valid_until.map(Value::Text).unwrap_or(Value::Null),
                        Value::Bool(role.bypass_rls),
                    ]
                })
                .collect();
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_auth_members" => {
            let cols = [
                ("oid", DataType::Int8),
                ("roleid", DataType::Int8),
                ("member", DataType::Int8),
                ("grantor", DataType::Int8),
                ("admin_option", DataType::Bool),
            ];
            let rows = db
                .role_memberships()
                .into_iter()
                .enumerate()
                .map(|(i, (member_oid, group_oid, _, _))| {
                    vec![
                        Value::Int(16400 + i as i64),
                        // `roleid` is the group; `member` is the member role.
                        Value::Int(group_oid),
                        Value::Int(member_oid),
                        Value::Int(10),
                        Value::Bool(false),
                    ]
                })
                .collect();
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_user" => {
            let cols = [
                ("usename", DataType::Text),
                ("usesysid", DataType::Int8),
                ("usecreatedb", DataType::Bool),
                ("usesuper", DataType::Bool),
                ("userepl", DataType::Bool),
                ("usebypassrls", DataType::Bool),
                ("passwd", DataType::Text),
                ("valuntil", DataType::TimestampTz),
                ("useconfig", DataType::Text),
            ];
            let rows = db
                .roles()
                .into_iter()
                .filter(|role| role.login)
                .map(|role| {
                    vec![
                        Value::Text(role.name),
                        Value::Int(role.oid),
                        Value::Bool(role.create_db),
                        Value::Bool(role.superuser),
                        Value::Bool(role.replication),
                        Value::Bool(role.bypass_rls),
                        role.password.map(Value::Text).unwrap_or(Value::Null),
                        role.valid_until.map(Value::Text).unwrap_or(Value::Null),
                        Value::Null,
                    ]
                })
                .collect();
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_database" => {
            let cols = [
                ("oid", DataType::Int8),
                ("datname", DataType::Text),
                ("datdba", DataType::Int8),
                ("encoding", DataType::Int4),
                ("datistemplate", DataType::Bool),
                ("datallowconn", DataType::Bool),
                ("datconnlimit", DataType::Int4),
                ("datcollate", DataType::Text),
                ("datctype", DataType::Text),
            ];
            let rows = db
                .databases()
                .into_iter()
                .map(|database| {
                    vec![
                        Value::Int(database.oid),
                        Value::Text(database.name),
                        Value::Int(database.owner),
                        Value::Int(database.encoding),
                        Value::Bool(database.is_template),
                        Value::Bool(database.allow_connections),
                        Value::Int(database.connection_limit),
                        Value::Text(database.collate),
                        Value::Text(database.ctype),
                    ]
                })
                .collect();
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_settings" => {
            let cols = [
                ("name", DataType::Text),
                ("setting", DataType::Text),
                ("unit", DataType::Text),
                ("category", DataType::Text),
                ("short_desc", DataType::Text),
                ("context", DataType::Text),
                ("vartype", DataType::Text),
                ("source", DataType::Text),
                ("boot_val", DataType::Text),
                ("reset_val", DataType::Text),
                ("pending_restart", DataType::Bool),
            ];
            let mut rows = vec![
                pg_setting_row(
                    "server_version",
                    db.system_setting("server_version")
                        .map_or("16.0", String::as_str),
                    "Preset Options",
                    "Shows the server version.",
                    "string",
                ),
                pg_setting_row(
                    "server_encoding",
                    db.system_setting("server_encoding")
                        .map_or("UTF8", String::as_str),
                    "Client Connection Defaults / Locale and Formatting",
                    "Sets the server encoding.",
                    "string",
                ),
                pg_setting_row(
                    "client_encoding",
                    db.system_setting("client_encoding")
                        .map_or("UTF8", String::as_str),
                    "Client Connection Defaults / Locale and Formatting",
                    "Sets the client's character set encoding.",
                    "string",
                ),
                pg_setting_row(
                    "standard_conforming_strings",
                    db.system_setting("standard_conforming_strings")
                        .map_or("on", String::as_str),
                    "Version and Platform Compatibility",
                    "Causes '...' strings to treat backslashes literally.",
                    "bool",
                ),
                pg_setting_row(
                    "search_path",
                    db.system_setting("search_path")
                        .map_or_else(|| db.search_path(), Clone::clone)
                        .as_str(),
                    "Client Connection Defaults / Statement Behavior",
                    "Sets the schema search order.",
                    "string",
                ),
            ];
            for (name, value) in db.system_settings() {
                if rows.iter().any(|row| row[0] == Value::Text(name.clone())) {
                    continue;
                }
                rows.push(pg_setting_row(
                    &name,
                    &value,
                    "Customized Options",
                    "Custom setting set with ALTER SYSTEM.",
                    "string",
                ));
            }
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_proc" => {
            let cols = [
                ("oid", DataType::Int8),
                ("proname", DataType::Text),
                ("pronamespace", DataType::Int8),
                ("proowner", DataType::Int8),
                ("prolang", DataType::Int8),
                ("prokind", DataType::Text),
                ("proisstrict", DataType::Bool),
                ("proretset", DataType::Bool),
                ("prorettype", DataType::Int8),
                ("proargtypes", DataType::Text),
            ];
            let rows = vec![
                pg_proc_row(2000, "count", "a", DataType::Int8, ""),
                pg_proc_row(2001, "sum", "a", DataType::Int8, "20"),
                pg_proc_row(2002, "avg", "a", DataType::Float8, "20"),
                pg_proc_row(2003, "min", "a", DataType::Text, "25"),
                pg_proc_row(2004, "max", "a", DataType::Text, "25"),
                pg_proc_row(2100, "upper", "f", DataType::Text, "25"),
                pg_proc_row(2101, "lower", "f", DataType::Text, "25"),
                pg_proc_row(2102, "length", "f", DataType::Int8, "25"),
                pg_proc_row(2103, "substring", "f", DataType::Text, "25 20 20"),
                pg_proc_row(2104, "replace", "f", DataType::Text, "25 25 25"),
                pg_proc_row(2105, "coalesce", "f", DataType::Text, ""),
                pg_proc_row(2106, "nullif", "f", DataType::Text, ""),
                pg_proc_row(2110, "array_length", "f", DataType::Int8, "25 20"),
                pg_proc_row(2111, "cardinality", "f", DataType::Int8, "25"),
                pg_proc_row(2112, "array_position", "f", DataType::Int8, "25 25"),
                pg_proc_row(2113, "array_append", "f", DataType::Text, "25 25"),
                pg_proc_row(2114, "array_prepend", "f", DataType::Text, "25 25"),
                pg_proc_row(2115, "array_cat", "f", DataType::Text, "25 25"),
                pg_proc_row(2120, "json_typeof", "f", DataType::Text, "114"),
                pg_proc_row(2121, "jsonb_typeof", "f", DataType::Text, "3802"),
                pg_proc_row(2122, "json_array_length", "f", DataType::Int8, "114"),
                pg_proc_row(2123, "jsonb_array_length", "f", DataType::Int8, "3802"),
                pg_proc_row(2124, "json_extract_path_text", "f", DataType::Text, ""),
                pg_proc_row(2125, "jsonb_extract_path_text", "f", DataType::Text, ""),
                pg_proc_row(2126, "jsonb_path_query", "f", DataType::Jsonb, "3802 25"),
                pg_proc_row(2127, "jsonb_path_exists", "f", DataType::Bool, "3802 25"),
                pg_proc_row(2130, "to_tsvector", "f", DataType::TsVector, "25"),
                pg_proc_row(2131, "plainto_tsquery", "f", DataType::TsQuery, "25"),
                pg_proc_row(2132, "to_tsquery", "f", DataType::TsQuery, "25"),
                pg_proc_row(2133, "ts_rank", "f", DataType::Float4, "3614 3615"),
                pg_proc_row(2200, "now", "f", DataType::TimestampTz, ""),
                pg_proc_row(2201, "current_date", "f", DataType::Date, ""),
                pg_proc_row(2202, "date_part", "f", DataType::Float8, "25 1114"),
                pg_proc_row(2203, "date_trunc", "f", DataType::Timestamp, "25 1114"),
                pg_proc_row_set(2204, "generate_series", "f", DataType::Int8, "20 20", true),
                pg_proc_row(2300, "pg_get_userbyid", "f", DataType::Text, "20"),
                pg_proc_row(2301, "pg_table_is_visible", "f", DataType::Bool, "20"),
                pg_proc_row(2302, "pg_type_is_visible", "f", DataType::Bool, "20"),
                pg_proc_row(2303, "pg_get_expr", "f", DataType::Text, "25 20"),
                pg_proc_row(2304, "pg_get_constraintdef", "f", DataType::Text, "20"),
                pg_proc_row(2305, "pg_get_indexdef", "f", DataType::Text, "20"),
                pg_proc_row(2306, "format_type", "f", DataType::Text, "20 23"),
                pg_proc_row(2307, "pg_encoding_to_char", "f", DataType::Text, "23"),
                pg_proc_row(2310, "pg_advisory_lock", "f", DataType::Text, "20"),
                pg_proc_row(2311, "pg_try_advisory_lock", "f", DataType::Bool, "20"),
                pg_proc_row(2312, "pg_advisory_unlock", "f", DataType::Bool, "20"),
                pg_proc_row(2313, "pg_advisory_unlock_all", "f", DataType::Text, ""),
            ];
            let mut rows = rows;
            // User-defined functions and aggregates appear after the built-ins,
            // with synthetic OIDs starting at 16384 (the first user OID).
            let mut oid = 16384i64;
            for f in db.all_functions() {
                let argtypes = f
                    .arg_types
                    .iter()
                    .map(|t| t.oid().to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
                let ret = f.return_type.unwrap_or(DataType::Text);
                rows.push(pg_proc_row(oid, &f.name, "f", ret, &argtypes));
                oid += 1;
            }
            for a in db.all_aggregates() {
                rows.push(pg_proc_row(oid, &a.name, "a", DataType::Text, ""));
                oid += 1;
            }
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_operator" => {
            let cols = [
                ("oid", DataType::Int8),
                ("oprname", DataType::Text),
                ("oprnamespace", DataType::Int8),
                ("oprowner", DataType::Int8),
                ("oprkind", DataType::Text),
                ("oprcanmerge", DataType::Bool),
                ("oprcanhash", DataType::Bool),
                ("oprleft", DataType::Int8),
                ("oprright", DataType::Int8),
                ("oprresult", DataType::Int8),
            ];
            let rows = vec![
                pg_operator_row(3000, "=", DataType::Int8, DataType::Int8, DataType::Bool),
                pg_operator_row(3001, "<>", DataType::Int8, DataType::Int8, DataType::Bool),
                pg_operator_row(3002, "<", DataType::Int8, DataType::Int8, DataType::Bool),
                pg_operator_row(3003, ">", DataType::Int8, DataType::Int8, DataType::Bool),
                pg_operator_row(3004, "<=", DataType::Int8, DataType::Int8, DataType::Bool),
                pg_operator_row(3005, ">=", DataType::Int8, DataType::Int8, DataType::Bool),
                pg_operator_row(3010, "+", DataType::Int8, DataType::Int8, DataType::Int8),
                pg_operator_row(3011, "-", DataType::Int8, DataType::Int8, DataType::Int8),
                pg_operator_row(3012, "*", DataType::Int8, DataType::Int8, DataType::Int8),
                pg_operator_row(3013, "/", DataType::Int8, DataType::Int8, DataType::Int8),
                pg_operator_row(3020, "||", DataType::Text, DataType::Text, DataType::Text),
                pg_operator_row(3021, "~~", DataType::Text, DataType::Text, DataType::Bool),
                pg_operator_row(3022, "!~~", DataType::Text, DataType::Text, DataType::Bool),
                pg_operator_row(3023, "~", DataType::Text, DataType::Text, DataType::Bool),
                pg_operator_row(3024, "!~", DataType::Text, DataType::Text, DataType::Bool),
                pg_operator_row(3030, "<<", DataType::Inet, DataType::Inet, DataType::Bool),
                pg_operator_row(3031, "<<=", DataType::Inet, DataType::Inet, DataType::Bool),
                pg_operator_row(3032, ">>", DataType::Inet, DataType::Inet, DataType::Bool),
                pg_operator_row(3033, ">>=", DataType::Inet, DataType::Inet, DataType::Bool),
                pg_operator_row(3034, "&&", DataType::Inet, DataType::Inet, DataType::Bool),
                pg_operator_row(
                    3040,
                    "@@",
                    DataType::TsVector,
                    DataType::TsQuery,
                    DataType::Bool,
                ),
            ];
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_locks" => {
            let cols = [
                ("locktype", DataType::Text),
                ("database", DataType::Int8),
                ("classid", DataType::Int8),
                ("objid", DataType::Int8),
                ("objsubid", DataType::Int4),
                ("virtualtransaction", DataType::Text),
                ("pid", DataType::Int4),
                ("mode", DataType::Text),
                ("granted", DataType::Bool),
            ];
            let rows = db
                .advisory_locks()
                .into_iter()
                .map(|lock| {
                    vec![
                        Value::Text("advisory".into()),
                        Value::Int(5),
                        Value::Int(lock.classid),
                        Value::Int(lock.objid),
                        Value::Int(0),
                        Value::Text("1/1".into()),
                        Value::Int(1),
                        Value::Text("ExclusiveLock".into()),
                        Value::Bool(true),
                    ]
                })
                .collect();
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        "pg_extension" => {
            let cols = [
                ("oid", DataType::Int8),
                ("extname", DataType::Text),
                ("extowner", DataType::Int8),
                ("extnamespace", DataType::Int8),
                ("extrelocatable", DataType::Bool),
                ("extversion", DataType::Text),
            ];
            let rows = db
                .extensions()
                .into_iter()
                .enumerate()
                .map(|(i, ext)| {
                    vec![
                        Value::Int(13563 + i as i64),
                        Value::Text(ext.name),
                        Value::Int(10),
                        Value::Int(11),
                        Value::Bool(false),
                        Value::Text(ext.version),
                    ]
                })
                .collect();
            Ok(qualify_virtual(qualifier, &cols, rows))
        }
        other => Err(format!("pg_catalog.{other} is not supported")),
    }
}

fn pg_proc_row(oid: i64, name: &str, kind: &str, ret: DataType, argtypes: &str) -> Vec<Value> {
    pg_proc_row_set(oid, name, kind, ret, argtypes, false)
}

fn pg_proc_row_set(
    oid: i64,
    name: &str,
    kind: &str,
    ret: DataType,
    argtypes: &str,
    retset: bool,
) -> Vec<Value> {
    vec![
        Value::Int(oid),
        Value::Text(name.into()),
        Value::Int(11),
        Value::Int(10),
        Value::Int(12),
        Value::Text(kind.into()),
        Value::Bool(false),
        Value::Bool(retset),
        Value::Int(ret.oid() as i64),
        Value::Text(argtypes.into()),
    ]
}

fn pg_operator_row(
    oid: i64,
    name: &str,
    left: DataType,
    right: DataType,
    result: DataType,
) -> Vec<Value> {
    vec![
        Value::Int(oid),
        Value::Text(name.into()),
        Value::Int(11),
        Value::Int(10),
        Value::Text("b".into()),
        Value::Bool(matches!(name, "=" | "<" | ">" | "<=" | ">=")),
        Value::Bool(name == "="),
        Value::Int(left.oid() as i64),
        Value::Int(right.oid() as i64),
        Value::Int(result.oid() as i64),
    ]
}

fn pg_setting_row(
    name: &str,
    value: &str,
    category: &str,
    desc: &str,
    vartype: &str,
) -> Vec<Value> {
    vec![
        Value::Text(name.into()),
        Value::Text(value.into()),
        Value::Null,
        Value::Text(category.into()),
        Value::Text(desc.into()),
        Value::Text("internal".into()),
        Value::Text(vartype.into()),
        Value::Text("default".into()),
        Value::Text(value.into()),
        Value::Text(value.into()),
        Value::Bool(false),
    ]
}

fn type_category(dt: DataType) -> &'static str {
    match dt {
        DataType::Bool => "B",
        DataType::Int2
        | DataType::Int4
        | DataType::Int8
        | DataType::Float4
        | DataType::Float8
        | DataType::Numeric
        | DataType::Money => "N",
        DataType::Date
        | DataType::Time
        | DataType::TimeTz
        | DataType::Interval
        | DataType::Timestamp
        | DataType::TimestampTz => "D",
        DataType::Inet | DataType::Cidr | DataType::Macaddr | DataType::Macaddr8 => "I",
        DataType::Json
        | DataType::Jsonb
        | DataType::Xml
        | DataType::TsVector
        | DataType::TsQuery => "U",
        DataType::Bytea => "U",
        DataType::Text | DataType::Uuid => "S",
    }
}

/// Build the (qualified names, types) for a virtual table's column spec.
fn qualify_virtual(
    qualifier: &str,
    cols: &[(&str, DataType)],
    rows: Vec<Vec<Value>>,
) -> (Vec<String>, Vec<DataType>, Vec<Vec<Value>>) {
    let names = cols
        .iter()
        .map(|(n, _)| format!("{qualifier}.{n}"))
        .collect();
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
    let has_exact_qualifier = qualifier
        .map(|q| {
            col_names
                .iter()
                .any(|c| c.rsplit_once('.').is_some_and(|(cq, _)| cq == q))
        })
        .unwrap_or(false);
    let has_bare_match = qualifier.is_none()
        && col_names
            .iter()
            .any(|c| c.rsplit_once('.').is_none() && c == name);
    for (i, c) in col_names.iter().enumerate() {
        let (cq, cn) = match c.rsplit_once('.') {
            Some((q, n)) => (Some(q), n),
            None => (None, c.as_str()),
        };
        let matches = match qualifier {
            // Qualified ref: require the qualifier to match, but tolerate
            // bare-stored names (single-table queries) by matching on name.
            Some(q) if has_exact_qualifier => cq == Some(q) && cn == name,
            Some(_) => cq.is_none() && cn == name,
            // Unqualified ref: match on the bare name.
            None if has_bare_match => cq.is_none() && cn == name,
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

/// Build a storage [`Column`] from a parsed [`ColumnDef`], resolving any
/// user-defined type name. A domain column adopts the domain's base type but
/// keeps `type_name` set so its constraints are enforced; enum/composite/range
/// columns stay text-backed with `type_name` recording the declared type.
fn column_from_def(db: &Database, cd: ColumnDef) -> Result<Column, String> {
    let mut data_type = cd.data_type;
    if let Some(name) = &cd.type_name {
        if let Some(domain) = db.domain(name) {
            data_type = domain.base;
        } else if db.user_type(name).is_none() {
            // Not a known user type: it degraded to text at parse time
            // (unknown built-in / extension type). Leave it as-is and don't
            // record a type name so no spurious enforcement happens.
            return Ok(Column {
                name: cd.name,
                data_type,
                type_name: None,
                not_null: cd.not_null,
                primary_key: cd.primary_key,
                default: cd.default,
                serial: cd.serial,
                identity: cd.identity,
                identity_always: cd.identity_always,
                generated: cd.generated,
            });
        }
    }
    Ok(Column {
        name: cd.name,
        data_type,
        type_name: cd.type_name,
        not_null: cd.not_null,
        primary_key: cd.primary_key,
        default: cd.default,
        serial: cd.serial,
        identity: cd.identity,
        identity_always: cd.identity_always,
        generated: cd.generated,
    })
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
        .map(|cd| column_from_def(db, cd))
        .collect::<Result<_, _>>()?;
    // Auto-create a unique index for each PRIMARY KEY column so point lookups
    // on it are fast out of the box (mirrors PostgreSQL's implicit pkey index).
    let pk_indexes: Vec<(usize, String)> = columns
        .iter()
        .enumerate()
        .filter(|(_, col)| col.primary_key)
        .map(|(i, col)| (i, format!("{}_{}_pkey", c.name, col.name)))
        .collect();
    let mut table = Table::new_with_persistence(c.name.clone(), columns, c.persistence);
    for (col_idx, name) in pk_indexes {
        table.create_index(name, col_idx, true);
    }
    for constraint in c.constraints {
        match constraint {
            TableConstraint::Unique {
                name,
                columns,
                primary_key,
            } => {
                let column_indices = constraint_column_indices(&table, &columns)?;
                if column_indices.len() == 1 {
                    table.create_index(name, column_indices[0], true);
                    if primary_key {
                        table.set_primary_key(column_indices[0], true);
                    }
                } else {
                    table.add_unique_constraint(UniqueConstraint {
                        name,
                        columns: column_indices,
                        primary_key,
                    });
                }
            }
            TableConstraint::Check {
                name,
                expr,
                validated,
            } => table.add_check_constraint(CheckConstraint {
                name,
                expr,
                validated,
            }),
            TableConstraint::ForeignKey {
                name,
                column,
                ref_table,
                ref_column,
                validated,
            } => {
                let column_idx = table
                    .column_index(&column)
                    .ok_or_else(|| format!("column \"{column}\" does not exist"))?;
                validate_foreign_key_reference(db, &ref_table, &ref_column)?;
                table.add_foreign_key_constraint(ForeignKeyConstraint {
                    name,
                    column: column_idx,
                    ref_table,
                    ref_column,
                    validated,
                });
            }
        }
    }
    db.create_table(table)?;
    Ok(ExecResult::Command("CREATE TABLE".into()))
}

fn constraint_column_indices(table: &Table, columns: &[String]) -> Result<Vec<usize>, String> {
    columns
        .iter()
        .map(|column| {
            table
                .column_index(column)
                .ok_or_else(|| format!("column \"{column}\" does not exist"))
        })
        .collect()
}

fn exec_create_extension(db: &mut Database, c: CreateExtension) -> Result<ExecResult, String> {
    db.create_extension(c.name, c.version, c.if_not_exists)?;
    Ok(ExecResult::Command("CREATE EXTENSION".into()))
}

fn exec_create_role(db: &mut Database, c: CreateRole) -> Result<ExecResult, String> {
    db.create_role(c.name, c.login, c.options)?;
    Ok(ExecResult::Command("CREATE ROLE".into()))
}

fn exec_create_sequence(db: &mut Database, c: CreateSequence) -> Result<ExecResult, String> {
    db.create_sequence(c.name, c.if_not_exists, c.start, c.increment)?;
    Ok(ExecResult::Command("CREATE SEQUENCE".into()))
}

fn exec_create_schema(db: &mut Database, c: CreateSchema) -> Result<ExecResult, String> {
    db.create_schema(c.name, c.if_not_exists)?;
    Ok(ExecResult::Command("CREATE SCHEMA".into()))
}

fn exec_create_database(db: &mut Database, c: CreateDatabase) -> Result<ExecResult, String> {
    db.create_database(c.name)?;
    Ok(ExecResult::Command("CREATE DATABASE".into()))
}

fn exec_create_tablespace(db: &mut Database, c: CreateTablespace) -> Result<ExecResult, String> {
    db.create_tablespace(c.name, c.location)?;
    Ok(ExecResult::Command("CREATE TABLESPACE".into()))
}

fn exec_create_collation(db: &mut Database, c: CreateCollation) -> Result<ExecResult, String> {
    db.create_collation(c.name, c.if_not_exists, c.locale)?;
    Ok(ExecResult::Command("CREATE COLLATION".into()))
}

fn exec_create_type(db: &mut Database, c: CreateType) -> Result<ExecResult, String> {
    let ty = match c.kind {
        CreateTypeKind::Enum { labels } => crate::storage::UserType::Enum { labels },
        CreateTypeKind::Composite { attributes } => {
            crate::storage::UserType::Composite { attributes }
        }
        CreateTypeKind::Range { subtype } => crate::storage::UserType::Range { subtype },
    };
    db.create_user_type(c.name, ty)?;
    Ok(ExecResult::Command("CREATE TYPE".into()))
}

fn exec_create_domain(db: &mut Database, c: CreateDomain) -> Result<ExecResult, String> {
    // Canonicalize the `VALUE` keyword in the CHECK to lowercase `value` so the
    // (case-sensitive) column resolver can bind it to the inserted value.
    let check = c.check.map(|mut e| {
        canonicalize_domain_value(&mut e);
        e
    });
    db.create_domain(crate::storage::Domain {
        name: c.name,
        base: c.base,
        not_null: c.not_null,
        check,
    })?;
    Ok(ExecResult::Command("CREATE DOMAIN".into()))
}

/// Rewrite `VALUE` column references (in any case) within a domain CHECK to the
/// canonical lowercase `value`, matching how `enforce_user_types` binds the
/// inserted value.
fn canonicalize_domain_value(expr: &mut Expr) {
    match expr {
        Expr::Column(name) if name.eq_ignore_ascii_case("value") => *name = "value".to_string(),
        Expr::Unary { expr, .. } => canonicalize_domain_value(expr),
        Expr::Binary { left, right, .. } => {
            canonicalize_domain_value(left);
            canonicalize_domain_value(right);
        }
        Expr::IsNull { expr, .. } => canonicalize_domain_value(expr),
        Expr::IsDistinctFrom { left, right, .. } => {
            canonicalize_domain_value(left);
            canonicalize_domain_value(right);
        }
        Expr::Like { expr, pattern, .. } => {
            canonicalize_domain_value(expr);
            canonicalize_domain_value(pattern);
        }
        Expr::InList { expr, list, .. } => {
            canonicalize_domain_value(expr);
            list.iter_mut().for_each(canonicalize_domain_value);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            canonicalize_domain_value(expr);
            canonicalize_domain_value(low);
            canonicalize_domain_value(high);
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(o) = operand {
                canonicalize_domain_value(o);
            }
            for (cond, res) in whens {
                canonicalize_domain_value(cond);
                canonicalize_domain_value(res);
            }
            if let Some(e) = else_expr {
                canonicalize_domain_value(e);
            }
        }
        Expr::Cast { expr, .. } => canonicalize_domain_value(expr),
        Expr::Function { args, .. } => args.iter_mut().for_each(canonicalize_domain_value),
        Expr::QuantifiedCompare { left, list, .. } => {
            canonicalize_domain_value(left);
            list.iter_mut().for_each(canonicalize_domain_value);
        }
        Expr::Row(items) | Expr::Array(items) => {
            items.iter_mut().for_each(canonicalize_domain_value);
        }
        _ => {}
    }
}

fn exec_drop_type(db: &mut Database, d: DropType) -> Result<ExecResult, String> {
    db.drop_user_type(&d.name, d.if_exists)?;
    Ok(ExecResult::Command("DROP TYPE".into()))
}

fn exec_drop_domain(db: &mut Database, d: DropDomain) -> Result<ExecResult, String> {
    db.drop_domain(&d.name, d.if_exists)?;
    Ok(ExecResult::Command("DROP DOMAIN".into()))
}

fn exec_create_function(db: &mut Database, c: CreateFunction) -> Result<ExecResult, String> {
    let arg_names = c.args.iter().map(|a| a.name.clone()).collect();
    let arg_types = c.args.iter().map(|a| a.data_type).collect();
    let arg_type_names = c.args.iter().map(|a| a.type_name.clone()).collect();
    db.create_function(
        SqlFunction {
            name: c.name,
            arg_names,
            arg_types,
            arg_type_names,
            return_type: c.return_type,
            return_type_name: c.return_type_name,
            body: c.body,
            language: c.language,
        },
        c.or_replace,
    )?;
    refresh_scalar_udfs(db);
    Ok(ExecResult::Command("CREATE FUNCTION".into()))
}

fn exec_drop_function(db: &mut Database, d: DropFunction) -> Result<ExecResult, String> {
    db.drop_function(&d.name, d.arg_types.as_deref(), d.if_exists)?;
    refresh_scalar_udfs(db);
    Ok(ExecResult::Command("DROP FUNCTION".into()))
}

fn exec_create_trigger(db: &mut Database, c: CreateTrigger) -> Result<ExecResult, String> {
    if db.table(&c.table).is_none() {
        return Err(format!("relation \"{}\" does not exist", c.table));
    }
    let events = c
        .events
        .iter()
        .map(|e| match e {
            TriggerEvent::Insert => "insert".to_string(),
            TriggerEvent::Update => "update".to_string(),
            TriggerEvent::Delete => "delete".to_string(),
        })
        .collect();
    db.create_trigger(Trigger {
        name: c.name,
        before: c.timing == TriggerTiming::Before,
        events,
        table: c.table,
        for_each_row: c.for_each_row,
        function: c.function,
    })?;
    Ok(ExecResult::Command("CREATE TRIGGER".into()))
}

fn exec_drop_trigger(db: &mut Database, d: DropTrigger) -> Result<ExecResult, String> {
    db.drop_trigger(&d.name, &d.table, d.if_exists)?;
    Ok(ExecResult::Command("DROP TRIGGER".into()))
}

fn exec_create_rule(db: &mut Database, c: CreateRule) -> Result<ExecResult, String> {
    let event = match c.event {
        TriggerEvent::Insert => "insert",
        TriggerEvent::Update => "update",
        TriggerEvent::Delete => "delete",
    }
    .to_string();
    db.create_rule(
        Rule {
            name: c.name,
            event,
            table: c.table,
            definition: c.definition,
        },
        c.or_replace,
    )?;
    Ok(ExecResult::Command("CREATE RULE".into()))
}

fn exec_drop_rule(db: &mut Database, d: DropRule) -> Result<ExecResult, String> {
    db.drop_rule(&d.name, &d.table, d.if_exists)?;
    Ok(ExecResult::Command("DROP RULE".into()))
}

fn exec_create_aggregate(db: &mut Database, c: CreateAggregate) -> Result<ExecResult, String> {
    db.create_aggregate(
        Aggregate {
            name: c.name,
            arg_types: c.arg_types,
            options: c.options,
        },
        c.or_replace,
    )?;
    Ok(ExecResult::Command("CREATE AGGREGATE".into()))
}

fn exec_drop_aggregate(db: &mut Database, d: DropAggregate) -> Result<ExecResult, String> {
    db.drop_aggregate(&d.name, d.if_exists)?;
    Ok(ExecResult::Command("DROP AGGREGATE".into()))
}

/// Fire row-level triggers for `table` matching `event` (`"insert"`/`"update"`/
/// `"delete"`) and `before`-ness, once per affected row.
///
/// NEW/OLD support: there is no plpgsql, so trigger functions cannot read
/// `NEW`/`OLD`. A trigger function is a previously-created SQL function whose
/// body is executed (re-parsed and run against the database) once per affected
/// row. This is sufficient for audit-style side effects (e.g. the function body
/// is `INSERT INTO audit ...`). The function arguments are not bound to row
/// values. A trigger whose function is missing or is a non-statement scalar
/// body is silently treated as a no-op so DML is never blocked by it.
fn fire_row_triggers(
    db: &mut Database,
    table: &str,
    event: &str,
    before: bool,
    affected: usize,
) -> Result<(), String> {
    if affected == 0 {
        return Ok(());
    }
    let triggers = db.triggers_for(table, event, before);
    for trig in triggers {
        // Resolve the trigger function's body (must be a SQL function).
        let body = match db.functions(&trig.function) {
            Some(overloads) => overloads
                .iter()
                .find(|f| f.language == "sql")
                .map(|f| f.body.clone()),
            None => None,
        };
        let Some(body) = body else {
            continue;
        };
        // Re-parse the body and run each statement once per affected row.
        let trimmed = body.trim().trim_end_matches(';').trim();
        let Ok(stmts) = crate::sql::Parser::parse_sql(trimmed) else {
            continue;
        };
        for _ in 0..affected {
            for stmt in &stmts {
                // Only statement-shaped bodies have observable effects; a bare
                // `SELECT <expr>` (scalar function) is a harmless no-op here.
                if matches!(stmt, Statement::Select(_) | Statement::Empty) {
                    continue;
                }
                execute(db, stmt.clone())?;
            }
        }
    }
    Ok(())
}

fn exec_create_view(db: &mut Database, c: CreateView) -> Result<ExecResult, String> {
    let fields = select_fields(db, &c.select)?
        .into_iter()
        .map(|field| (field.name, field.data_type))
        .collect();
    db.create_view(
        View {
            name: c.name,
            select: *c.select,
            fields,
        },
        c.or_replace,
    )?;
    Ok(ExecResult::Command("CREATE VIEW".into()))
}

fn exec_create_materialized_view(
    db: &mut Database,
    c: CreateMaterializedView,
) -> Result<ExecResult, String> {
    let (fields, rows) = materialize_select(db, &c.select)?;
    db.create_materialized_view(
        MaterializedView {
            name: c.name,
            select: *c.select,
            fields,
            rows,
        },
        c.if_not_exists,
    )?;
    Ok(ExecResult::Command("CREATE MATERIALIZED VIEW".into()))
}

fn materialize_select(
    db: &mut Database,
    select: &Select,
) -> Result<(Vec<(String, DataType)>, Vec<Vec<Value>>), String> {
    let fields = select_fields(db, select)?
        .into_iter()
        .map(|field| (field.name, field.data_type))
        .collect();
    let ExecResult::Rows { rows, .. } = exec_select(db, select.clone())? else {
        return Err("materialized view query did not produce rows".into());
    };
    Ok((fields, rows))
}

fn exec_create_index(db: &mut Database, c: CreateIndex) -> Result<ExecResult, String> {
    let table = db
        .table_mut(&c.table)
        .ok_or_else(|| format!("relation \"{}\" does not exist", c.table))?;

    // Resolve each key into either a column position or a stored expression.
    let mut columns: Vec<usize> = Vec::new();
    let mut expr: Option<Expr> = None;
    for key in &c.keys {
        match key {
            IndexKeyExpr::Column(name) => {
                let col = table
                    .column_index(name)
                    .ok_or_else(|| format!("column \"{name}\" does not exist"))?;
                columns.push(col);
            }
            // We support a single expression key (the common `((lower(name)))`
            // form). More than one expression, or mixing an expression with
            // column keys, is rejected for now.
            IndexKeyExpr::Expr(e) => {
                if expr.is_some() {
                    return Err("multi-key expression indexes are not supported".to_string());
                }
                expr = Some(e.clone());
            }
        }
    }
    if expr.is_some() && !columns.is_empty() {
        return Err("mixing expression and column index keys is not supported".to_string());
    }
    let is_expr_index = expr.is_some();

    // Resolve INCLUDE columns.
    let mut include: Vec<usize> = Vec::new();
    for name in &c.include {
        let col = table
            .column_index(name)
            .ok_or_else(|| format!("column \"{name}\" does not exist"))?;
        include.push(col);
    }

    // Generate a deterministic name when none is given, matching PostgreSQL's
    // `<table>_<key>_idx` convention so replay is stable.
    let key_label = if is_expr_index {
        "expr".to_string()
    } else {
        c.keys
            .iter()
            .map(|k| match k {
                IndexKeyExpr::Column(name) => name.clone(),
                IndexKeyExpr::Expr(_) => "expr".to_string(),
            })
            .collect::<Vec<_>>()
            .join("_")
    };
    let name = c
        .name
        .unwrap_or_else(|| format!("{}_{}_idx", c.table, key_label));
    if table.has_index_named(&name) {
        if c.if_not_exists {
            return Ok(ExecResult::Command("CREATE INDEX".into()));
        }
        return Err(format!("relation \"{name}\" already exists"));
    }
    // Unique-violation check only applies to a plain single-column index with
    // no partial predicate.
    if c.unique
        && c.predicate.is_none()
        && columns.len() == 1
        && table.column_has_duplicate_values(columns[0])
    {
        return Err(format!(
            "could not create unique index \"{name}\": key contains duplicate values"
        ));
    }
    let method = match c.method {
        crate::sql::ast::IndexMethod::Hash => crate::index::IndexMethod::Hash,
        crate::sql::ast::IndexMethod::Btree => crate::index::IndexMethod::Btree,
    };
    table.create_index_full(name, columns, expr, c.predicate, include, c.unique, method);
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
    if db.table(&d.name).is_none() {
        if d.if_exists {
            return Ok(ExecResult::Command("DROP TABLE".into()));
        }
        return Err(format!("table \"{}\" does not exist", d.name));
    }
    ensure_table_not_referenced(db, &d.name)?;
    db.drop_table(&d.name);
    Ok(ExecResult::Command("DROP TABLE".into()))
}

fn exec_drop_extension(db: &mut Database, d: DropExtension) -> Result<ExecResult, String> {
    db.drop_extension(&d.name, d.if_exists)?;
    Ok(ExecResult::Command("DROP EXTENSION".into()))
}

fn exec_drop_role(db: &mut Database, d: DropRole) -> Result<ExecResult, String> {
    db.drop_role(&d.name, d.if_exists)?;
    Ok(ExecResult::Command("DROP ROLE".into()))
}

fn exec_drop_sequence(db: &mut Database, d: DropSequence) -> Result<ExecResult, String> {
    db.drop_sequence(&d.name, d.if_exists)?;
    Ok(ExecResult::Command("DROP SEQUENCE".into()))
}

fn exec_drop_schema(db: &mut Database, d: DropSchema) -> Result<ExecResult, String> {
    db.drop_schema(&d.name, d.if_exists)?;
    Ok(ExecResult::Command("DROP SCHEMA".into()))
}

fn exec_drop_database(db: &mut Database, d: DropDatabase) -> Result<ExecResult, String> {
    db.drop_database(&d.name, d.if_exists)?;
    Ok(ExecResult::Command("DROP DATABASE".into()))
}

fn exec_drop_tablespace(db: &mut Database, d: DropTablespace) -> Result<ExecResult, String> {
    db.drop_tablespace(&d.name, d.if_exists)?;
    Ok(ExecResult::Command("DROP TABLESPACE".into()))
}

fn exec_drop_collation(db: &mut Database, d: DropCollation) -> Result<ExecResult, String> {
    db.drop_collation(&d.name, d.if_exists)?;
    Ok(ExecResult::Command("DROP COLLATION".into()))
}

fn exec_alter_database(db: &mut Database, a: AlterDatabase) -> Result<ExecResult, String> {
    match a.action {
        AlterDatabaseAction::Rename { to } => db.alter_database_rename(&a.name, to)?,
        AlterDatabaseAction::SetConnectionLimit { limit } => {
            db.alter_database_connection_limit(&a.name, limit)?
        }
    }
    Ok(ExecResult::Command("ALTER DATABASE".into()))
}

fn exec_drop_view(db: &mut Database, d: DropView) -> Result<ExecResult, String> {
    let existed = db.drop_view(&d.name);
    if !existed && !d.if_exists {
        return Err(format!("view \"{}\" does not exist", d.name));
    }
    Ok(ExecResult::Command("DROP VIEW".into()))
}

fn exec_drop_materialized_view(
    db: &mut Database,
    d: DropMaterializedView,
) -> Result<ExecResult, String> {
    let existed = db.drop_materialized_view(&d.name);
    if !existed && !d.if_exists {
        return Err(format!("materialized view \"{}\" does not exist", d.name));
    }
    Ok(ExecResult::Command("DROP MATERIALIZED VIEW".into()))
}

fn exec_refresh_materialized_view(
    db: &mut Database,
    r: RefreshMaterializedView,
) -> Result<ExecResult, String> {
    let select = db
        .materialized_view(&r.name)
        .ok_or_else(|| format!("materialized view \"{}\" does not exist", r.name))?
        .select
        .clone();
    let (_, rows) = materialize_select(db, &select)?;
    db.replace_materialized_view_rows(&r.name, rows)?;
    Ok(ExecResult::Command("REFRESH MATERIALIZED VIEW".into()))
}

fn exec_alter_table(db: &mut Database, alter: AlterTable) -> Result<ExecResult, String> {
    match alter.action {
        AlterAction::RenameTable { to } => {
            db.rename_table(&alter.table, &to)?;
            return Ok(ExecResult::Command("ALTER TABLE".into()));
        }
        AlterAction::AddColumn {
            column,
            if_not_exists,
        } => {
            // Resolve user-defined types (domains adopt their base type).
            let col = column_from_def(db, column.clone())?;
            // Evaluate a constant default once; serial fills per row.
            let default_val = match &column.default {
                Some(e) => coerce(eval_expr(e, &[], &[])?, col.data_type)?,
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
                if column.not_null
                    && column.default.is_none()
                    && !column.serial
                    && !column.identity
                    && column.generated.is_none()
                    && !table.rows.is_empty()
                {
                    return Err(format!("column \"{}\" contains null values", column.name));
                }
            }
            if column.serial || column.identity {
                // Pre-compute one sequence value per existing row, then fill.
                let key = format!("{}.{}", alter.table, column.name);
                let n = db.table(&alter.table).map(|t| t.rows.len()).unwrap_or(0);
                let fills: Vec<Value> =
                    (0..n).map(|_| Value::Int(db.next_sequence(&key))).collect();
                let table = db.table_mut(&alter.table).unwrap();
                table.add_column(col, &|pos| fills[pos].clone());
            } else if column.generated.is_some() {
                let table = db.table_mut(&alter.table).unwrap();
                let col_names = table.column_names();
                let expr = column.generated.clone().expect("checked above");
                let generated_values: Result<Vec<_>, _> = table
                    .rows
                    .iter()
                    .map(|row| {
                        eval_expr(&expr, &col_names, row).and_then(|v| coerce(v, column.data_type))
                    })
                    .collect();
                let generated_values = generated_values?;
                table.add_column(col, &|pos| generated_values[pos].clone());
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
                None => Err(format!(
                    "column \"{name}\" of relation \"{}\" does not exist",
                    alter.table
                )),
            }
        }
        AlterAction::AddConstraint { constraint } => match constraint {
            TableConstraint::Unique {
                name,
                columns,
                primary_key,
            } => {
                let table = db
                    .table_mut(&alter.table)
                    .ok_or_else(|| format!("relation \"{}\" does not exist", alter.table))?;
                let column_indices = constraint_column_indices(table, &columns)?;
                if table.has_constraint_named(&name) {
                    return Err(format!("constraint \"{name}\" already exists"));
                }
                if primary_key {
                    for (&column_idx, column) in column_indices.iter().zip(&columns) {
                        if table.rows.iter().any(|row| row[column_idx].is_null()) {
                            return Err(format!("column \"{column}\" contains null values"));
                        }
                    }
                }
                let duplicate = if column_indices.len() == 1 {
                    table.column_has_duplicate_values(column_indices[0])
                } else {
                    table.columns_have_duplicate_values(&column_indices)
                };
                if duplicate {
                    return Err(format!(
                        "could not create unique constraint \"{name}\": key contains duplicate values"
                    ));
                }
                if column_indices.len() == 1 {
                    table.create_index(name, column_indices[0], true);
                    if primary_key {
                        table.set_primary_key(column_indices[0], true);
                    }
                } else {
                    table.add_unique_constraint(UniqueConstraint {
                        name,
                        columns: column_indices,
                        primary_key,
                    });
                }
                Ok(ExecResult::Command("ALTER TABLE".into()))
            }
            TableConstraint::Check {
                name,
                expr,
                validated,
            } => {
                let table = db
                    .table_mut(&alter.table)
                    .ok_or_else(|| format!("relation \"{}\" does not exist", alter.table))?;
                if table.has_constraint_named(&name) {
                    return Err(format!("constraint \"{name}\" already exists"));
                }
                if validated {
                    validate_check_constraint(table, &name, &expr)?;
                }
                table.add_check_constraint(CheckConstraint {
                    name,
                    expr,
                    validated,
                });
                Ok(ExecResult::Command("ALTER TABLE".into()))
            }
            TableConstraint::ForeignKey {
                name,
                column,
                ref_table,
                ref_column,
                validated,
            } => {
                {
                    let table = db
                        .table(&alter.table)
                        .ok_or_else(|| format!("relation \"{}\" does not exist", alter.table))?;
                    if table.has_constraint_named(&name) {
                        return Err(format!("constraint \"{name}\" already exists"));
                    }
                    let column_idx = table
                        .column_index(&column)
                        .ok_or_else(|| format!("column \"{column}\" does not exist"))?;
                    validate_foreign_key_reference(db, &ref_table, &ref_column)?;
                    if validated {
                        validate_foreign_key_existing_rows(
                            db,
                            &alter.table,
                            column_idx,
                            &ref_table,
                            &ref_column,
                            &name,
                        )?;
                    }
                }
                let column_idx = db
                    .table(&alter.table)
                    .and_then(|table| table.column_index(&column))
                    .expect("column checked above");
                let table = db.table_mut(&alter.table).expect("table checked above");
                table.add_foreign_key_constraint(ForeignKeyConstraint {
                    name,
                    column: column_idx,
                    ref_table,
                    ref_column,
                    validated,
                });
                Ok(ExecResult::Command("ALTER TABLE".into()))
            }
        },
        AlterAction::DropConstraint { name, if_exists } => {
            let table = db
                .table_mut(&alter.table)
                .ok_or_else(|| format!("relation \"{}\" does not exist", alter.table))?;
            if table.drop_check_constraint(&name) {
                return Ok(ExecResult::Command("ALTER TABLE".into()));
            }
            if table.drop_foreign_key_constraint(&name) {
                return Ok(ExecResult::Command("ALTER TABLE".into()));
            }
            if table.drop_unique_constraint(&name) {
                return Ok(ExecResult::Command("ALTER TABLE".into()));
            }
            match table.remove_index(&name) {
                Some(index) => {
                    if let Some(col) = index.leading_column() {
                        if index.unique && table.columns[col].primary_key {
                            table.set_primary_key(col, false);
                        }
                    }
                    Ok(ExecResult::Command("ALTER TABLE".into()))
                }
                None if if_exists => Ok(ExecResult::Command("ALTER TABLE".into())),
                None => Err(format!("constraint \"{name}\" does not exist")),
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
                None => Err(format!(
                    "column \"{from}\" of relation \"{}\" does not exist",
                    alter.table
                )),
            }
        }
    }
}

fn exec_alter_role(db: &mut Database, alter: AlterRole) -> Result<ExecResult, String> {
    db.alter_role(&alter.name, alter.options)?;
    Ok(ExecResult::Command("ALTER ROLE".into()))
}

fn exec_alter_sequence(db: &mut Database, alter: AlterSequence) -> Result<ExecResult, String> {
    db.alter_sequence(&alter.name, alter.restart, alter.increment)?;
    Ok(ExecResult::Command("ALTER SEQUENCE".into()))
}

fn validate_check_constraint(table: &Table, name: &str, expr: &Expr) -> Result<(), String> {
    let col_names = table.column_names();
    for row in &table.rows {
        if !eval_expr(expr, &col_names, row)?.is_true() {
            return Err(format!(
                "check constraint \"{name}\" of relation \"{}\" is violated by some row",
                table.name
            ));
        }
    }
    Ok(())
}

fn check_row_constraints(table: &Table, row: &[Value]) -> Result<(), String> {
    let col_names = table.column_names();
    for constraint in table.check_constraints() {
        if !eval_expr(&constraint.expr, &col_names, row)?.is_true() {
            return Err(format!(
                "new row for relation \"{}\" violates check constraint \"{}\"",
                table.name, constraint.name
            ));
        }
    }
    Ok(())
}

/// Enforce user-defined-type constraints on a finished row: enum-label
/// membership and domain NOT NULL / CHECK(VALUE ...) predicates. Built-in
/// columns (`type_name == None`) are skipped.
fn enforce_user_types(db: &Database, columns: &[Column], row: &[Value]) -> Result<(), String> {
    for (i, col) in columns.iter().enumerate() {
        let Some(type_name) = &col.type_name else {
            continue;
        };
        let value = &row[i];
        if let Some(crate::storage::UserType::Enum { labels }) = db.user_type(type_name) {
            if let Some(text) = value.to_text() {
                if !labels.iter().any(|label| label == &text) {
                    return Err(format!(
                        "invalid input value for enum {type_name}: \"{text}\""
                    ));
                }
            }
            continue;
        }
        if let Some(domain) = db.domain(type_name) {
            if value.is_null() {
                if domain.not_null {
                    return Err(format!(
                        "domain {} does not allow null values",
                        domain.name
                    ));
                }
                continue;
            }
            if let Some(check) = &domain.check {
                // `VALUE` in the CHECK refers to the inserted value; bind it as
                // a single-column row so the existing expression evaluator can
                // resolve the `value` column reference.
                let value_names = ["value".to_string()];
                let value_row = [value.clone()];
                if !eval_expr(check, &value_names, &value_row)?.is_true() {
                    return Err(format!(
                        "value for domain {} violates check constraint",
                        domain.name
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_foreign_key_reference(
    db: &Database,
    ref_table: &str,
    ref_column: &str,
) -> Result<(), String> {
    let table = db
        .table(ref_table)
        .ok_or_else(|| format!("relation \"{ref_table}\" does not exist"))?;
    let column = table
        .column_index(ref_column)
        .ok_or_else(|| format!("column \"{ref_column}\" does not exist"))?;
    let has_unique = table
        .indexes()
        .iter()
        .any(|idx| idx.unique && idx.columns == [column]);
    if !has_unique {
        return Err(format!(
            "there is no unique constraint matching given keys for referenced table \"{ref_table}\""
        ));
    }
    Ok(())
}

fn validate_foreign_key_existing_rows(
    db: &Database,
    table_name: &str,
    column: usize,
    ref_table: &str,
    ref_column: &str,
    constraint: &str,
) -> Result<(), String> {
    let table = db.table(table_name).expect("table checked by caller");
    for row in &table.rows {
        check_foreign_key_value(
            db,
            &row[column],
            ref_table,
            ref_column,
            table_name,
            constraint,
        )?;
    }
    Ok(())
}

fn check_foreign_key_value(
    db: &Database,
    value: &Value,
    ref_table: &str,
    ref_column: &str,
    table_name: &str,
    constraint: &str,
) -> Result<(), String> {
    if value.is_null() {
        return Ok(());
    }
    let referenced = db.table(ref_table).expect("referenced table checked");
    let ref_idx = referenced
        .column_index(ref_column)
        .expect("referenced column checked");
    let found = referenced
        .rows
        .iter()
        .any(|row| compare_values(&row[ref_idx], value) == Some(Ordering::Equal));
    if found {
        Ok(())
    } else {
        Err(format!(
            "insert or update on table \"{table_name}\" violates foreign key constraint \"{constraint}\""
        ))
    }
}

fn check_foreign_key_constraints(
    db: &Database,
    table_name: &str,
    row: &[Value],
) -> Result<(), String> {
    let table = db.table(table_name).expect("table checked by caller");
    for constraint in table.foreign_key_constraints() {
        check_foreign_key_value(
            db,
            &row[constraint.column],
            &constraint.ref_table,
            &constraint.ref_column,
            table_name,
            &constraint.name,
        )?;
    }
    Ok(())
}

fn check_parent_key_not_referenced(
    db: &Database,
    parent_table: &str,
    row: &[Value],
) -> Result<(), String> {
    let Some(parent) = db.table(parent_table) else {
        return Ok(());
    };
    for child_name in db.table_names() {
        let child = db.table(&child_name).expect("name came from table_names");
        for constraint in child.foreign_key_constraints() {
            if constraint.ref_table != parent_table {
                continue;
            }
            let ref_idx = parent
                .column_index(&constraint.ref_column)
                .expect("referenced column validated when constraint was created");
            let parent_value = &row[ref_idx];
            if parent_value.is_null() {
                continue;
            }
            let referenced = child.rows.iter().any(|child_row| {
                compare_values(&child_row[constraint.column], parent_value) == Some(Ordering::Equal)
            });
            if referenced {
                return Err(format!(
                    "update or delete on table \"{parent_table}\" violates foreign key constraint \"{}\" on table \"{child_name}\"",
                    constraint.name
                ));
            }
        }
    }
    Ok(())
}

fn ensure_table_not_referenced(db: &Database, table_name: &str) -> Result<(), String> {
    for child_name in db.table_names() {
        let child = db.table(&child_name).expect("name came from table_names");
        if let Some(constraint) = child
            .foreign_key_constraints()
            .iter()
            .find(|constraint| constraint.ref_table == table_name)
        {
            return Err(format!(
                "cannot drop table \"{table_name}\" because other objects depend on it: constraint \"{}\" on table \"{child_name}\"",
                constraint.name
            ));
        }
    }
    Ok(())
}

fn exec_insert(db: &mut Database, ins: Insert) -> Result<ExecResult, String> {
    // Resolve schema first (immutable borrow), then mutate.
    let table = db
        .table(&ins.table)
        .ok_or_else(|| format!("relation \"{}\" does not exist", ins.table))?;
    let columns = table.columns.clone();
    if let Some(on_conflict) = &ins.on_conflict {
        for name in on_conflict_target(on_conflict) {
            if !columns.iter().any(|c| &c.name == name) {
                return Err(format!(
                    "column \"{name}\" of relation \"{}\" does not exist",
                    ins.table
                ));
            }
        }
        if let OnConflict::DoUpdate { assignments, .. } = on_conflict {
            for (name, _) in assignments {
                let Some(column) = columns.iter().find(|c| &c.name == name) else {
                    return Err(format!(
                        "column \"{name}\" of relation \"{}\" does not exist",
                        ins.table
                    ));
                };
                if column.generated.is_some() {
                    return Err(format!(
                        "column \"{name}\" can only be updated to DEFAULT because it is a generated column"
                    ));
                }
            }
        }
    }

    // Map each VALUES position to a target column index.
    let target_indices: Vec<usize> = if ins.default_values {
        Vec::new()
    } else {
        match &ins.columns {
            Some(names) => {
                let mut idx = Vec::with_capacity(names.len());
                for n in names {
                    let i = columns.iter().position(|c| &c.name == n).ok_or_else(|| {
                        format!(
                            "column \"{n}\" of relation \"{}\" does not exist",
                            ins.table
                        )
                    })?;
                    idx.push(i);
                }
                idx
            }
            None => (0..columns.len()).collect(),
        }
    };

    let selected_rows = if let Some(select) = ins.select {
        match exec_select(db, *select)? {
            ExecResult::Rows { rows, .. } => Some(rows),
            ExecResult::Empty | ExecResult::Command(_) => {
                return Err("INSERT query did not produce rows".into());
            }
        }
    } else {
        None
    };

    let mut new_rows = Vec::new();
    if let Some(input_rows) = selected_rows {
        new_rows.reserve(input_rows.len());
        for values in input_rows {
            if values.len() != target_indices.len() {
                return Err(format!(
                    "INSERT has {} expressions but {} target columns",
                    values.len(),
                    target_indices.len()
                ));
            }
            let mut row = vec![Value::Null; columns.len()];
            for (val, &col_idx) in values.into_iter().zip(&target_indices) {
                row[col_idx] = coerce(val, columns[col_idx].data_type)?;
            }
            finish_insert_row(
                db,
                &ins.table,
                &columns,
                &target_indices,
                ins.overriding_system_value,
                &mut row,
            )?;
            new_rows.push(row);
        }
    } else {
        let input_rows: Vec<Vec<Expr>> = if ins.default_values {
            vec![Vec::new()]
        } else {
            ins.rows.clone()
        };
        new_rows.reserve(input_rows.len());
        for tuple in &input_rows {
            if tuple.len() != target_indices.len() {
                return Err(format!(
                    "INSERT has {} expressions but {} target columns",
                    tuple.len(),
                    target_indices.len()
                ));
            }
            let mut row = vec![Value::Null; columns.len()];
            for (expr, &col_idx) in tuple.iter().zip(&target_indices) {
                let val = eval_expr(expr, &[], &[])?;
                row[col_idx] = coerce(val, columns[col_idx].data_type)?;
            }
            finish_insert_row(
                db,
                &ins.table,
                &columns,
                &target_indices,
                ins.overriding_system_value,
                &mut row,
            )?;
            new_rows.push(row);
        }
    }

    {
        let table = db.table(&ins.table).expect("table existed above");
        for row in &new_rows {
            check_row_constraints(table, row)?;
        }
    }
    for row in &new_rows {
        enforce_user_types(db, &columns, row)?;
    }
    for row in &new_rows {
        check_foreign_key_constraints(db, &ins.table, row)?;
    }

    if matches!(ins.on_conflict, Some(OnConflict::DoNothing { .. })) {
        let mut accepted: Vec<Vec<Value>> = Vec::with_capacity(new_rows.len());
        let table = db.table(&ins.table).expect("table existed above");
        for row in &new_rows {
            if table.unique_violation(row, None).is_some() {
                continue;
            }
            let mut conflicts_prior_accepted = false;
            for columns in table.unique_key_columns() {
                if accepted
                    .iter()
                    .any(|existing| same_row_unique_key(existing, row, &columns))
                {
                    conflicts_prior_accepted = true;
                    break;
                }
            }
            if !conflicts_prior_accepted {
                accepted.push(row.clone());
            }
        }
        new_rows = accepted;
    } else if let Some(OnConflict::DoUpdate {
        target,
        assignments,
        filter,
    }) = &ins.on_conflict
    {
        let mut insert_rows = Vec::new();
        let mut update_rows: Vec<(usize, Vec<Value>)> = Vec::new();
        let mut touched = std::collections::HashSet::new();
        {
            let table = db.table(&ins.table).expect("table existed above");
            for row in &new_rows {
                if let Some(conflict_pos) = conflict_position_for_row(table, row, target, &columns)?
                {
                    if !touched.insert(conflict_pos) {
                        return Err(
                            "ON CONFLICT DO UPDATE command cannot affect row a second time".into(),
                        );
                    }
                    let existing = &table.rows[conflict_pos];
                    let eval_names = on_conflict_eval_names(&columns);
                    let mut eval_row = existing.clone();
                    eval_row.extend(row.clone());
                    if let Some(predicate) = filter {
                        if !eval_expr(predicate, &eval_names, &eval_row)?.is_true() {
                            continue;
                        }
                    }
                    let mut updated = existing.clone();
                    for (name, expr) in assignments {
                        let idx = columns
                            .iter()
                            .position(|c| &c.name == name)
                            .expect("assignment target checked above");
                        let value = eval_expr(expr, &eval_names, &eval_row)?;
                        updated[idx] = coerce(value, columns[idx].data_type)?;
                    }
                    apply_generated_columns(&columns, &mut updated)?;
                    update_rows.push((conflict_pos, updated));
                } else {
                    insert_rows.push(row.clone());
                }
            }
        }
        {
            let table = db.table(&ins.table).expect("table existed above");
            for (pos, row) in &update_rows {
                check_row_constraints(table, row)?;
                if let Some(name) = table.unique_violation(row, Some(*pos)) {
                    return Err(format!(
                        "duplicate key value violates unique constraint \"{name}\""
                    ));
                }
            }
            for row in &insert_rows {
                if let Some(name) = table.unique_violation(row, None) {
                    return Err(format!(
                        "duplicate key value violates unique constraint \"{name}\""
                    ));
                }
            }
            for columns in table.unique_key_columns() {
                if rows_have_duplicate_unique_key(
                    insert_rows
                        .iter()
                        .chain(update_rows.iter().map(|(_, row)| row)),
                    &columns,
                ) {
                    return Err("duplicate key value violates unique constraint".into());
                }
            }
        }
        for (_, row) in &update_rows {
            enforce_user_types(db, &columns, row)?;
        }
        for (_, row) in &update_rows {
            check_foreign_key_constraints(db, &ins.table, row)?;
        }
        for row in &insert_rows {
            check_foreign_key_constraints(db, &ins.table, row)?;
        }

        let mut affected_rows = Vec::with_capacity(update_rows.len() + insert_rows.len());
        affected_rows.extend(update_rows.iter().map(|(_, row)| row.clone()));
        affected_rows.extend(insert_rows.iter().cloned());
        let tag = format!("INSERT 0 {}", affected_rows.len());
        let result = returning_result(&ins.returning, &columns, &affected_rows, tag)?;
        let n_update = update_rows.len();
        let n_insert = insert_rows.len();
        let table = db.table_mut(&ins.table).expect("table existed above");
        for (pos, row) in update_rows {
            table.update_row(pos, row);
        }
        for row in insert_rows {
            table.push_row(row);
        }
        fire_row_triggers(db, &ins.table, "insert", false, n_insert)?;
        fire_row_triggers(db, &ins.table, "update", false, n_update)?;
        return Ok(result);
    } else {
        // Enforce unique constraints atomically: check all new rows against
        // existing data and against each other before inserting any.
        {
            let table = db.table(&ins.table).expect("table existed above");
            for row in &new_rows {
                if let Some(name) = table.unique_violation(row, None) {
                    return Err(format!(
                        "duplicate key value violates unique constraint \"{name}\""
                    ));
                }
            }
            for columns in table.unique_key_columns() {
                if rows_have_duplicate_unique_key(new_rows.iter(), &columns) {
                    return Err("duplicate key value violates unique constraint".into());
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
    fire_row_triggers(db, &ins.table, "insert", false, n)?;
    Ok(result)
}

fn on_conflict_target(on_conflict: &OnConflict) -> &[String] {
    match on_conflict {
        OnConflict::DoNothing { target } | OnConflict::DoUpdate { target, .. } => target,
    }
}

fn conflict_position_for_row(
    table: &Table,
    row: &[Value],
    target: &[String],
    columns: &[Column],
) -> Result<Option<usize>, String> {
    if target.is_empty() {
        for columns in table.unique_key_columns() {
            if let Some(pos) = conflict_position_for_key(table, row, &columns)? {
                return Ok(Some(pos));
            }
        }
        return Ok(None);
    }
    let target_indices: Vec<usize> = {
        let mut indices = Vec::with_capacity(target.len());
        for name in target {
            indices.push(
                columns
                    .iter()
                    .position(|column| &column.name == name)
                    .ok_or_else(|| format!("column \"{name}\" does not exist"))?,
            );
        }
        indices
    };
    conflict_position_for_key(table, row, &target_indices)
}

fn conflict_position_for_key(
    table: &Table,
    row: &[Value],
    target_indices: &[usize],
) -> Result<Option<usize>, String> {
    if target_indices.iter().any(|&idx| row[idx].is_null()) {
        return Ok(None);
    }
    for (pos, existing) in table.rows.iter().enumerate() {
        if same_row_unique_key(existing, row, target_indices) {
            return Ok(Some(pos));
        }
    }
    Ok(None)
}

fn rows_have_duplicate_unique_key<'a>(
    rows: impl Iterator<Item = &'a Vec<Value>>,
    columns: &[usize],
) -> bool {
    let mut seen = HashSet::new();
    for row in rows {
        if columns.iter().any(|&column| row[column].is_null()) {
            continue;
        }
        let key: Vec<String> = columns
            .iter()
            .map(|&column| row[column].to_text().unwrap_or_default())
            .collect();
        if !seen.insert(key) {
            return true;
        }
    }
    false
}

fn same_row_unique_key(left: &[Value], right: &[Value], columns: &[usize]) -> bool {
    !columns
        .iter()
        .any(|&column| left[column].is_null() || right[column].is_null())
        && columns
            .iter()
            .all(|&column| left[column].to_text() == right[column].to_text())
}

fn on_conflict_eval_names(columns: &[Column]) -> Vec<String> {
    let mut names: Vec<String> = columns.iter().map(|column| column.name.clone()).collect();
    names.extend(
        columns
            .iter()
            .map(|column| format!("excluded.{}", column.name)),
    );
    names
}

fn finish_insert_row(
    db: &mut Database,
    table_name: &str,
    columns: &[Column],
    target_indices: &[usize],
    overriding_system_value: bool,
    row: &mut [Value],
) -> Result<(), String> {
    for (i, col) in columns.iter().enumerate() {
        if col.generated.is_some() && target_indices.contains(&i) && !row[i].is_null() {
            return Err(format!(
                "cannot insert a non-DEFAULT value into column \"{}\" because it is a generated column",
                col.name
            ));
        }
    }
    for (i, col) in columns.iter().enumerate() {
        if col.identity
            && col.identity_always
            && target_indices.contains(&i)
            && !row[i].is_null()
            && !overriding_system_value
        {
            return Err(format!(
                "cannot insert a non-DEFAULT value into column \"{}\" because it is an identity column defined as GENERATED ALWAYS",
                col.name
            ));
        }
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
        if !col.serial && !col.identity {
            continue;
        }
        let key = format!("{}.{}", table_name, col.name);
        match row[i] {
            Value::Int(v) => db.observe_sequence(&key, v),
            Value::Null => row[i] = Value::Int(db.next_sequence(&key)),
            _ => {}
        }
    }
    apply_generated_columns(columns, row)?;
    // Enforce NOT NULL.
    for (i, col) in columns.iter().enumerate() {
        if col.not_null && row[i].is_null() {
            return Err(format!(
                "null value in column \"{}\" violates not-null constraint",
                col.name
            ));
        }
    }
    Ok(())
}

fn apply_generated_columns(columns: &[Column], row: &mut [Value]) -> Result<(), String> {
    let col_names: Vec<String> = columns.iter().map(|col| col.name.clone()).collect();
    for (i, col) in columns.iter().enumerate() {
        let Some(expr) = &col.generated else {
            continue;
        };
        let value = eval_expr(expr, &col_names, row)?;
        row[i] = coerce(value, col.data_type)?;
    }
    Ok(())
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
                    fields.push(FieldDescription {
                        name: bare_name(name),
                        data_type: col_types[i],
                    });
                }
            }
            SelectItem::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| default_column_name(expr));
                fields.push(FieldDescription {
                    name,
                    data_type: infer_expr_type(expr, col_names, col_types),
                });
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

/// Apply `f` to each immediate child expression of `expr` (one level only).
///
/// Subquery nodes (`ScalarSubquery`/`Exists`/`InSubquery`) carry nested
/// `Select`s rather than plain child expressions and are deliberately treated
/// as leaves here — callers that care about subqueries match them explicitly
/// before falling through to a generic walk.
fn visit_child_exprs(
    expr: &Expr,
    f: &mut dyn FnMut(&Expr) -> Result<(), String>,
) -> Result<(), String> {
    match expr {
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Cast { expr, .. } => f(expr)?,
        Expr::Binary { left, right, .. } | Expr::IsDistinctFrom { left, right, .. } => {
            f(left)?;
            f(right)?;
        }
        Expr::QuantifiedCompare { left, list, .. } => {
            f(left)?;
            for e in list {
                f(e)?;
            }
        }
        Expr::Row(items) | Expr::Array(items) => {
            for e in items {
                f(e)?;
            }
        }
        Expr::Like { expr, pattern, .. } => {
            f(expr)?;
            f(pattern)?;
        }
        Expr::InList { expr, list, .. } => {
            f(expr)?;
            for e in list {
                f(e)?;
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            f(expr)?;
            f(low)?;
            f(high)?;
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(o) = operand {
                f(o)?;
            }
            for (c, r) in whens {
                f(c)?;
                f(r)?;
            }
            if let Some(e) = else_expr {
                f(e)?;
            }
        }
        Expr::Function { args, filter, .. } => {
            for a in args {
                f(a)?;
            }
            if let Some(filter) = filter {
                f(filter)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Mutable counterpart to [`visit_child_exprs`].
fn map_child_exprs(
    expr: &mut Expr,
    f: &mut dyn FnMut(&mut Expr) -> Result<(), String>,
) -> Result<(), String> {
    match expr {
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Cast { expr, .. } => f(expr)?,
        Expr::Binary { left, right, .. } | Expr::IsDistinctFrom { left, right, .. } => {
            f(left)?;
            f(right)?;
        }
        Expr::QuantifiedCompare { left, list, .. } => {
            f(left)?;
            for e in list {
                f(e)?;
            }
        }
        Expr::Row(items) | Expr::Array(items) => {
            for e in items {
                f(e)?;
            }
        }
        Expr::Like { expr, pattern, .. } => {
            f(expr)?;
            f(pattern)?;
        }
        Expr::InList { expr, list, .. } => {
            f(expr)?;
            for e in list {
                f(e)?;
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            f(expr)?;
            f(low)?;
            f(high)?;
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(o) = operand {
                f(o)?;
            }
            for (c, r) in whens {
                f(c)?;
                f(r)?;
            }
            if let Some(e) = else_expr {
                f(e)?;
            }
        }
        Expr::Function { args, filter, .. } => {
            for a in args {
                f(a)?;
            }
            if let Some(filter) = filter {
                f(filter)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Apply `f` to each top-level expression directly owned by `sel`'s clauses.
/// Nested `Select`s reached through set operations and CTEs introduce their own
/// scopes and are intentionally not descended into here.
fn visit_select_exprs(
    sel: &Select,
    f: &mut dyn FnMut(&Expr) -> Result<(), String>,
) -> Result<(), String> {
    for item in &sel.projection {
        if let SelectItem::Expr { expr, .. } = item {
            f(expr)?;
        }
    }
    if let Some(fc) = &sel.from {
        for a in &fc.base.args {
            f(a)?;
        }
        for j in &fc.joins {
            for a in &j.table.args {
                f(a)?;
            }
            if let Some(on) = &j.on {
                f(on)?;
            }
        }
    }
    if let Some(filter) = &sel.filter {
        f(filter)?;
    }
    if let Some(having) = &sel.having {
        f(having)?;
    }
    for g in &sel.group_by {
        f(g)?;
    }
    for e in &sel.distinct_on {
        f(e)?;
    }
    for ob in &sel.order_by {
        f(&ob.expr)?;
    }
    if let Some(l) = &sel.limit {
        f(l)?;
    }
    if let Some(o) = &sel.offset {
        f(o)?;
    }
    Ok(())
}

/// Mutable counterpart to [`visit_select_exprs`].
fn visit_select_exprs_mut(
    sel: &mut Select,
    f: &mut dyn FnMut(&mut Expr) -> Result<(), String>,
) -> Result<(), String> {
    for item in &mut sel.projection {
        if let SelectItem::Expr { expr, .. } = item {
            f(expr)?;
        }
    }
    if let Some(fc) = &mut sel.from {
        for a in &mut fc.base.args {
            f(a)?;
        }
        for j in &mut fc.joins {
            for a in &mut j.table.args {
                f(a)?;
            }
            if let Some(on) = &mut j.on {
                f(on)?;
            }
        }
    }
    if let Some(filter) = &mut sel.filter {
        f(filter)?;
    }
    if let Some(having) = &mut sel.having {
        f(having)?;
    }
    for g in &mut sel.group_by {
        f(g)?;
    }
    for e in &mut sel.distinct_on {
        f(e)?;
    }
    for ob in &mut sel.order_by {
        f(&mut ob.expr)?;
    }
    if let Some(l) = &mut sel.limit {
        f(l)?;
    }
    if let Some(o) = &mut sel.offset {
        f(o)?;
    }
    Ok(())
}

/// Resolve every uncorrelated subquery within a SELECT's clauses.
///
/// `outer_cols` are the columns visible to this SELECT (its own FROM schema).
/// A subquery that references one of these outer columns is *correlated* and is
/// left in place: it cannot be reduced to a single literal, because its value
/// depends on the current outer row. Correlated subqueries are evaluated later,
/// per outer row, by [`resolve_correlated`].
fn resolve_subqueries_in_select(
    db: &mut Database,
    sel: &mut Select,
    outer_cols: &[String],
    ctes: &CteMap,
) -> Result<(), String> {
    for item in &mut sel.projection {
        if let SelectItem::Expr { expr, .. } = item {
            resolve_subqueries(db, expr, outer_cols, ctes)?;
        }
    }
    if let Some(f) = &mut sel.filter {
        resolve_subqueries(db, f, outer_cols, ctes)?;
    }
    if let Some(h) = &mut sel.having {
        resolve_subqueries(db, h, outer_cols, ctes)?;
    }
    for g in &mut sel.group_by {
        resolve_subqueries(db, g, outer_cols, ctes)?;
    }
    for e in &mut sel.distinct_on {
        resolve_subqueries(db, e, outer_cols, ctes)?;
    }
    for ob in &mut sel.order_by {
        resolve_subqueries(db, &mut ob.expr, outer_cols, ctes)?;
    }
    if let Some(l) = &mut sel.limit {
        resolve_subqueries(db, l, outer_cols, ctes)?;
    }
    if let Some(o) = &mut sel.offset {
        resolve_subqueries(db, o, outer_cols, ctes)?;
    }
    for set_op in &mut sel.set_ops {
        resolve_subqueries_in_select(db, &mut set_op.select, outer_cols, ctes)?;
    }
    Ok(())
}

/// Execute uncorrelated subqueries in `expr` once and replace them with the
/// resulting literal (scalar), value-list (`IN`), or boolean (`EXISTS`).
///
/// A subquery that references a column from `outer_cols` (and not one of its
/// own columns) is correlated: it is left in place so it can be re-evaluated
/// per outer row by [`resolve_correlated`]. When `outer_cols` is empty every
/// subquery is treated as uncorrelated, which is the behaviour callers that
/// have no outer scope (e.g. `UPDATE`/`DELETE`) rely on.
fn resolve_subqueries(
    db: &mut Database,
    expr: &mut Expr,
    outer_cols: &[String],
    ctes: &CteMap,
) -> Result<(), String> {
    match expr {
        Expr::ScalarSubquery(sel) => {
            if !select_correlated_to(db, sel, outer_cols, ctes)? {
                let v = exec_scalar_subquery(db, sel, ctes)?;
                *expr = value_to_literal(v);
            }
        }
        Expr::Exists(sel) => {
            if !select_correlated_to(db, sel, outer_cols, ctes)? {
                let has_rows = subquery_row_count(db, sel, ctes)? > 0;
                *expr = Expr::Bool(has_rows);
            }
        }
        Expr::InSubquery {
            expr: inner,
            subquery,
            negated,
        } => {
            resolve_subqueries(db, inner, outer_cols, ctes)?;
            if !select_correlated_to(db, subquery, outer_cols, ctes)? {
                let values = subquery_single_column(db, subquery, ctes)?;
                let list = values.into_iter().map(value_to_literal).collect();
                *expr = Expr::InList {
                    expr: inner.clone(),
                    list,
                    negated: *negated,
                };
            }
        }
        Expr::Unary { expr, .. } => resolve_subqueries(db, expr, outer_cols, ctes)?,
        Expr::Binary { left, right, .. } => {
            resolve_subqueries(db, left, outer_cols, ctes)?;
            resolve_subqueries(db, right, outer_cols, ctes)?;
        }
        Expr::QuantifiedCompare { left, list, .. } => {
            resolve_subqueries(db, left, outer_cols, ctes)?;
            for e in list {
                resolve_subqueries(db, e, outer_cols, ctes)?;
            }
        }
        Expr::Row(items) | Expr::Array(items) => {
            for item in items {
                resolve_subqueries(db, item, outer_cols, ctes)?;
            }
        }
        Expr::IsNull { expr, .. } | Expr::Cast { expr, .. } => {
            resolve_subqueries(db, expr, outer_cols, ctes)?
        }
        Expr::IsDistinctFrom { left, right, .. } => {
            resolve_subqueries(db, left, outer_cols, ctes)?;
            resolve_subqueries(db, right, outer_cols, ctes)?;
        }
        Expr::Like { expr, pattern, .. } => {
            resolve_subqueries(db, expr, outer_cols, ctes)?;
            resolve_subqueries(db, pattern, outer_cols, ctes)?;
        }
        Expr::InList { expr, list, .. } => {
            resolve_subqueries(db, expr, outer_cols, ctes)?;
            for e in list {
                resolve_subqueries(db, e, outer_cols, ctes)?;
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            resolve_subqueries(db, expr, outer_cols, ctes)?;
            resolve_subqueries(db, low, outer_cols, ctes)?;
            resolve_subqueries(db, high, outer_cols, ctes)?;
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(o) = operand {
                resolve_subqueries(db, o, outer_cols, ctes)?;
            }
            for (c, r) in whens {
                resolve_subqueries(db, c, outer_cols, ctes)?;
                resolve_subqueries(db, r, outer_cols, ctes)?;
            }
            if let Some(e) = else_expr {
                resolve_subqueries(db, e, outer_cols, ctes)?;
            }
        }
        Expr::Function { args, filter, .. } => {
            for a in args {
                resolve_subqueries(db, a, outer_cols, ctes)?;
            }
            if let Some(filter) = filter {
                resolve_subqueries(db, filter, outer_cols, ctes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Run a subquery expected to yield a single value (zero rows → NULL).
fn exec_scalar_subquery(db: &mut Database, sel: &Select, ctes: &CteMap) -> Result<Value, String> {
    match exec_select_with_ctes(db, sel.clone(), ctes)? {
        ExecResult::Rows {
            fields, mut rows, ..
        } => {
            if fields.len() != 1 {
                return Err("subquery must return only one column".into());
            }
            if rows.len() > 1 {
                return Err(
                    "more than one row returned by a subquery used as an expression".into(),
                );
            }
            Ok(rows.pop().map(|mut r| r.remove(0)).unwrap_or(Value::Null))
        }
        _ => Err("subquery did not return a result set".into()),
    }
}

/// Number of rows a subquery yields (for `EXISTS`).
fn subquery_row_count(db: &mut Database, sel: &Select, ctes: &CteMap) -> Result<usize, String> {
    match exec_select_with_ctes(db, sel.clone(), ctes)? {
        ExecResult::Rows { rows, .. } => Ok(rows.len()),
        _ => Ok(0),
    }
}

/// Collect a single-column subquery's values (for `IN (SELECT ...)`).
fn subquery_single_column(
    db: &mut Database,
    sel: &Select,
    ctes: &CteMap,
) -> Result<Vec<Value>, String> {
    match exec_select_with_ctes(db, sel.clone(), ctes)? {
        ExecResult::Rows { fields, rows, .. } => {
            if fields.len() != 1 {
                return Err("subquery must return only one column".into());
            }
            Ok(rows.into_iter().map(|mut r| r.remove(0)).collect())
        }
        _ => Err("subquery did not return a result set".into()),
    }
}

/// The column names visible inside a subquery's own `FROM` clause. A subquery
/// with no `FROM` exposes no columns of its own.
fn select_own_columns(
    db: &mut Database,
    sel: &Select,
    ctes: &CteMap,
) -> Result<Vec<String>, String> {
    match &sel.from {
        Some(fc) => Ok(build_source_with_ctes(db, fc, None, ctes)?.0),
        None => Ok(Vec::new()),
    }
}

/// Whether `sel` references any column from `outer_cols` that it cannot satisfy
/// from its own (or a more deeply nested) scope — i.e. whether it is correlated
/// with the enclosing query.
fn select_correlated_to(
    db: &mut Database,
    sel: &Select,
    outer_cols: &[String],
    ctes: &CteMap,
) -> Result<bool, String> {
    if outer_cols.is_empty() {
        return Ok(false);
    }
    let own = select_own_columns(db, sel, ctes)?;
    select_correlated_inner(db, sel, &own, outer_cols, ctes)
}

/// As [`select_correlated_to`], but `visible` already includes `sel`'s own
/// columns plus every enclosing inner scope down to (but excluding) the
/// `outer_cols` scope we are testing correlation against.
fn select_correlated_inner(
    db: &mut Database,
    sel: &Select,
    visible: &[String],
    outer_cols: &[String],
    ctes: &CteMap,
) -> Result<bool, String> {
    let mut found = false;
    visit_select_exprs(sel, &mut |e| {
        if !found {
            found = expr_correlated(db, e, visible, outer_cols, ctes)?;
        }
        Ok(())
    })?;
    Ok(found)
}

/// Whether `expr` (or a subquery within it) references an `outer_cols` column
/// not satisfied by `visible`.
fn expr_correlated(
    db: &mut Database,
    expr: &Expr,
    visible: &[String],
    outer_cols: &[String],
    ctes: &CteMap,
) -> Result<bool, String> {
    match expr {
        Expr::Column(name) => Ok(resolve_column(visible, None, name).is_err()
            && resolve_column(outer_cols, None, name).is_ok()),
        Expr::QualifiedColumn { qualifier, name } => {
            Ok(resolve_column(visible, Some(qualifier), name).is_err()
                && resolve_column(outer_cols, Some(qualifier), name).is_ok())
        }
        Expr::ScalarSubquery(sel) | Expr::Exists(sel) => {
            let mut inner = visible.to_vec();
            inner.extend(select_own_columns(db, sel, ctes)?);
            select_correlated_inner(db, sel, &inner, outer_cols, ctes)
        }
        Expr::InSubquery { expr, subquery, .. } => {
            if expr_correlated(db, expr, visible, outer_cols, ctes)? {
                return Ok(true);
            }
            let mut inner = visible.to_vec();
            inner.extend(select_own_columns(db, subquery, ctes)?);
            select_correlated_inner(db, subquery, &inner, outer_cols, ctes)
        }
        _ => {
            let mut found = false;
            visit_child_exprs(expr, &mut |child| {
                if !found {
                    found = expr_correlated(db, child, visible, outer_cols, ctes)?;
                }
                Ok(())
            })?;
            Ok(found)
        }
    }
}

/// Whether `expr` still contains an unresolved subquery. After
/// [`resolve_subqueries_in_select`] only correlated subqueries remain.
fn expr_contains_subquery(expr: &Expr) -> bool {
    match expr {
        Expr::ScalarSubquery(_) | Expr::Exists(_) | Expr::InSubquery { .. } => true,
        _ => {
            let mut found = false;
            // The closure is infallible here.
            let _ = visit_child_exprs(expr, &mut |child| {
                found = found || expr_contains_subquery(child);
                Ok(())
            });
            found
        }
    }
}

/// Whether any clause of `sel` contains a subquery.
fn select_has_subquery(sel: &Select) -> bool {
    let mut found = false;
    let _ = visit_select_exprs(sel, &mut |e| {
        found = found || expr_contains_subquery(e);
        Ok(())
    });
    found
}

/// Produce a copy of `expr` in which every correlated subquery has been
/// specialised to the given outer row and executed, so that the result
/// contains only literals/value-lists that the pure [`eval_expr`] can handle.
fn resolve_correlated(
    db: &mut Database,
    expr: &Expr,
    outer_cols: &[String],
    outer_row: &[Value],
    ctes: &CteMap,
) -> Result<Expr, String> {
    match expr {
        Expr::ScalarSubquery(sel) => {
            let specialised = specialize_select(db, sel, outer_cols, outer_row, ctes)?;
            Ok(value_to_literal(exec_scalar_subquery(db, &specialised, ctes)?))
        }
        Expr::Exists(sel) => {
            let specialised = specialize_select(db, sel, outer_cols, outer_row, ctes)?;
            Ok(Expr::Bool(subquery_row_count(db, &specialised, ctes)? > 0))
        }
        Expr::InSubquery {
            expr: inner,
            subquery,
            negated,
        } => {
            let inner = resolve_correlated(db, inner, outer_cols, outer_row, ctes)?;
            let specialised = specialize_select(db, subquery, outer_cols, outer_row, ctes)?;
            let values = subquery_single_column(db, &specialised, ctes)?;
            Ok(Expr::InList {
                expr: Box::new(inner),
                list: values.into_iter().map(value_to_literal).collect(),
                negated: *negated,
            })
        }
        _ => {
            let mut out = expr.clone();
            map_child_exprs(&mut out, &mut |child| {
                *child = resolve_correlated(db, child, outer_cols, outer_row, ctes)?;
                Ok(())
            })?;
            Ok(out)
        }
    }
}

/// Specialise a correlated subquery to a single outer row by replacing every
/// reference to an `outer_cols` column (that the subquery does not itself
/// provide) with the corresponding literal from `outer_row`. References to the
/// subquery's own columns, and correlations to scopes deeper still, are left
/// untouched and resolved when the specialised subquery executes.
fn specialize_select(
    db: &mut Database,
    sel: &Select,
    outer_cols: &[String],
    outer_row: &[Value],
    ctes: &CteMap,
) -> Result<Select, String> {
    let own = select_own_columns(db, sel, ctes)?;
    let mut out = sel.clone();
    specialize_select_inner(db, &mut out, &own, outer_cols, outer_row, ctes)?;
    Ok(out)
}

/// As [`specialize_select`], but `shadow` already includes this select's own
/// columns plus any enclosing inner scopes (which take precedence over the
/// outer scope being substituted).
fn specialize_select_inner(
    db: &mut Database,
    sel: &mut Select,
    shadow: &[String],
    outer_cols: &[String],
    outer_row: &[Value],
    ctes: &CteMap,
) -> Result<(), String> {
    visit_select_exprs_mut(sel, &mut |e| {
        specialize_expr(db, e, shadow, outer_cols, outer_row, ctes)
    })
}

fn specialize_expr(
    db: &mut Database,
    expr: &mut Expr,
    shadow: &[String],
    outer_cols: &[String],
    outer_row: &[Value],
    ctes: &CteMap,
) -> Result<(), String> {
    match expr {
        Expr::Column(name) => {
            if resolve_column(shadow, None, name).is_err() {
                if let Ok(idx) = resolve_column(outer_cols, None, name) {
                    *expr = value_to_literal(outer_row[idx].clone());
                }
            }
        }
        Expr::QualifiedColumn { qualifier, name } => {
            if resolve_column(shadow, Some(qualifier), name).is_err() {
                if let Ok(idx) = resolve_column(outer_cols, Some(qualifier), name) {
                    *expr = value_to_literal(outer_row[idx].clone());
                }
            }
        }
        Expr::ScalarSubquery(sel) | Expr::Exists(sel) => {
            let mut inner = shadow.to_vec();
            inner.extend(select_own_columns(db, sel, ctes)?);
            specialize_select_inner(db, sel, &inner, outer_cols, outer_row, ctes)?;
        }
        Expr::InSubquery { expr, subquery, .. } => {
            specialize_expr(db, expr, shadow, outer_cols, outer_row, ctes)?;
            let mut inner = shadow.to_vec();
            inner.extend(select_own_columns(db, subquery, ctes)?);
            specialize_select_inner(db, subquery, &inner, outer_cols, outer_row, ctes)?;
        }
        _ => {
            map_child_exprs(expr, &mut |child| {
                specialize_expr(db, child, shadow, outer_cols, outer_row, ctes)
            })?;
        }
    }
    Ok(())
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

fn row_value_to_text(values: &[Value]) -> String {
    let parts: Vec<String> = values
        .iter()
        .map(|value| match value {
            Value::Null => String::new(),
            other => {
                let text = other.to_text().unwrap_or_default();
                if text.contains([',', '(', ')', '"', '\\']) {
                    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
                } else {
                    text
                }
            }
        })
        .collect();
    format!("({})", parts.join(","))
}

fn array_value_to_text(values: &[Value]) -> String {
    let parts: Vec<String> = values
        .iter()
        .map(|value| match value {
            Value::Null => "NULL".to_string(),
            other => {
                let text = other.to_text().unwrap_or_default();
                if text.contains([',', '{', '}', '"', '\\']) || text.is_empty() {
                    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
                } else {
                    text
                }
            }
        })
        .collect();
    format!("{{{}}}", parts.join(","))
}

fn parse_array_text(text: &str) -> Option<Vec<Option<String>>> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'{') || bytes.last() != Some(&b'}') {
        return None;
    }
    if text.len() == 2 {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut escape = false;
    for ch in text[1..text.len() - 1].chars() {
        if escape {
            cur.push(ch);
            escape = false;
            continue;
        }
        if quoted {
            match ch {
                '\\' => escape = true,
                '"' => quoted = false,
                _ => cur.push(ch),
            }
            continue;
        }
        match ch {
            '"' => {
                quoted = true;
                was_quoted = true;
            }
            ',' => {
                out.push(array_element(&cur, was_quoted));
                cur.clear();
                was_quoted = false;
            }
            _ => cur.push(ch),
        }
    }
    out.push(array_element(&cur, was_quoted));
    Some(out)
}

fn array_element(raw: &str, was_quoted: bool) -> Option<String> {
    if !was_quoted && raw.eq_ignore_ascii_case("NULL") {
        None
    } else {
        Some(raw.to_string())
    }
}

fn array_text_from_elements(values: &[Option<String>]) -> String {
    let parts: Vec<String> = values
        .iter()
        .map(|value| match value {
            None => "NULL".to_string(),
            Some(text) if text.contains([',', '{', '}', '"', '\\']) || text.is_empty() => {
                format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
            }
            Some(text) => text.clone(),
        })
        .collect();
    format!("{{{}}}", parts.join(","))
}

fn exec_select(db: &mut Database, sel: Select) -> Result<ExecResult, String> {
    let ctes = CteMap::new();
    exec_select_with_ctes(db, sel, &ctes)
}

fn exec_select_with_ctes(
    db: &mut Database,
    mut sel: Select,
    inherited_ctes: &CteMap,
) -> Result<ExecResult, String> {
    let ctes = materialize_ctes(db, &sel.ctes, inherited_ctes)?;
    sel.ctes.clear();
    // Execute uncorrelated subqueries first, splicing their results in as
    // literals/value-lists so the row-evaluation and index-planning paths
    // never see them. Correlated subqueries (those referencing this SELECT's
    // own columns) are left in place and re-evaluated per outer row below.
    let outer_cols = if select_has_subquery(&sel) {
        match &sel.from {
            Some(fc) => build_source_with_ctes(db, fc, None, &ctes)?.0,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    resolve_subqueries_in_select(db, &mut sel, &outer_cols, &ctes)?;

    if !sel.set_ops.is_empty() {
        return exec_set_select(db, sel);
    }

    if sel.from.is_none() && is_single_generate_series_projection(&sel.projection) {
        return select_generate_series(&sel);
    }
    if sel.from.is_none() && is_single_sequence_projection(&sel.projection) {
        return select_sequence_function(db, &sel);
    }
    if sel.from.is_none() && is_advisory_lock_projection(&sel.projection) {
        return select_advisory_lock_functions(db, &sel);
    }

    // Resolve the source: the (possibly joined) FROM rows with qualified
    // column names, or a single synthetic empty row for `SELECT <exprs>`.
    let (col_names, col_types, source_rows) = match &sel.from {
        Some(fc) => build_source_with_ctes(db, fc, sel.filter.as_ref(), &ctes)?,
        None => (Vec::new(), Vec::new(), vec![Vec::new()]),
    };

    // Apply WHERE. A correlated subquery in the predicate needs database access
    // per row, so it takes a serial path; otherwise the scan/filter is split
    // across a bounded number of scoped workers, joined in chunk order.
    let rows = match sel.filter.as_ref() {
        Some(pred) if expr_contains_subquery(pred) => {
            let mut kept = Vec::new();
            for row in source_rows {
                let resolved = resolve_correlated(db, pred, &col_names, &row, &ctes)?;
                if eval_expr(&resolved, &col_names, &row)?.is_true() {
                    kept.push(row);
                }
            }
            kept
        }
        _ => filter_select_rows(source_rows, &col_names, sel.filter.as_ref())?,
    };

    // Grouped/aggregate path: triggered by GROUP BY, an aggregate in the
    // projection, or an aggregate in HAVING.
    let has_agg = sel.projection.iter().any(|item| match item {
        SelectItem::Expr { expr, .. } => contains_aggregate(expr),
        SelectItem::Wildcard => false,
    }) || sel.having.as_ref().is_some_and(contains_aggregate);

    if !sel.group_by.is_empty() || !sel.grouping_sets.is_empty() || has_agg {
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
                    fields.push(FieldDescription {
                        name: bare_name(name),
                        data_type: col_types[i],
                    });
                }
            }
            SelectItem::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| default_column_name(expr));
                let dt = infer_expr_type(expr, &col_names, &col_types);
                producers.push(Producer::Expr(expr.clone()));
                fields.push(FieldDescription {
                    name,
                    data_type: dt,
                });
            }
        }
    }

    // Window functions: precompute each window function's value per input row
    // (partitioned, ordered, framed) so the projection can splice them in.
    let mut window_fns: Vec<Expr> = Vec::new();
    for p in &producers {
        if let Producer::Expr(e) = p {
            collect_window_fns(e, &mut window_fns);
        }
    }
    let window_vals: Vec<Vec<Value>> = window_fns
        .iter()
        .map(|w| compute_window_values(w, &col_names, &rows))
        .collect::<Result<_, _>>()?;

    // Project each input row, keeping input + output side by side so ORDER BY
    // can reference either input columns or output aliases.
    let mut combined: Vec<(Vec<Value>, Vec<Value>)> = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let mut out = Vec::with_capacity(producers.len());
        for p in &producers {
            match p {
                Producer::Col(c) => out.push(row[*c].clone()),
                // Replace window functions with their precomputed per-row value.
                Producer::Expr(e) if contains_window_function(e) => {
                    let resolved = substitute_window_fns(e, &window_fns, &window_vals, i);
                    out.push(eval_expr(&resolved, &col_names, row)?);
                }
                // A correlated subquery in the projection is specialised to this
                // row and executed before the (pure) expression is evaluated.
                Producer::Expr(e) if expr_contains_subquery(e) => {
                    let resolved = resolve_correlated(db, e, &col_names, row, &ctes)?;
                    out.push(eval_expr(&resolved, &col_names, row)?);
                }
                Producer::Expr(e) => out.push(eval_expr(e, &col_names, row)?),
            }
        }
        combined.push((row.clone(), out));
    }

    // DISTINCT: drop later duplicates of the projected row (order-preserving).
    if sel.distinct && sel.distinct_on.is_empty() {
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

    // ORDER BY. DISTINCT ON depends on this ordering because PostgreSQL keeps
    // the first row for each DISTINCT ON key.
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
                let ord =
                    compare_values(&sort_keys[a][i], &sort_keys[b][i]).unwrap_or(Ordering::Equal);
                let ord = if item.asc { ord } else { ord.reverse() };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });
        combined = idx
            .into_iter()
            .map(|i| std::mem::take(&mut combined[i]))
            .collect();
    }

    if !sel.distinct_on.is_empty() {
        let mut distinct_keys = Vec::with_capacity(combined.len());
        for (input, output) in &combined {
            let mut key = Vec::with_capacity(sel.distinct_on.len());
            for expr in &sel.distinct_on {
                let value = if let Some(i) = output_column_index(expr, &fields) {
                    output[i].clone()
                } else {
                    eval_expr(expr, &col_names, input)?
                };
                key.push(value);
            }
            distinct_keys.push(key);
        }
        let mut seen: Vec<Vec<Value>> = Vec::new();
        let mut filtered = Vec::new();
        for (pair, key) in combined.into_iter().zip(distinct_keys) {
            if seen.iter().any(|s| s == &key) {
                continue;
            } else {
                seen.push(key);
                filtered.push(pair);
            }
        }
        combined = filtered;
    }

    // OFFSET / LIMIT.
    let offset = eval_count(&sel.offset, &col_names)?.unwrap_or(0);
    let limit = eval_count(&sel.limit, &col_names)?;
    let start = offset.min(combined.len());
    let end = match limit {
        Some(l) => (start + l).min(combined.len()),
        None => combined.len(),
    };
    let out_rows: Vec<Vec<Value>> = combined[start..end]
        .iter()
        .map(|(_, o)| o.clone())
        .collect();
    let tag = format!("SELECT {}", out_rows.len());
    Ok(ExecResult::Rows {
        fields,
        rows: out_rows,
        tag,
    })
}

fn filter_select_rows(
    source_rows: Vec<Vec<Value>>,
    col_names: &[String],
    filter: Option<&Expr>,
) -> Result<Vec<Vec<Value>>, String> {
    let worker_count = parallel_select_worker_count(source_rows.len());
    if worker_count <= 1 {
        return filter_select_rows_serial(&source_rows, col_names, filter);
    }

    let chunk_len = source_rows.len().div_ceil(worker_count);
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in source_rows.chunks(chunk_len) {
            handles.push(scope.spawn(move || filter_select_rows_serial(chunk, col_names, filter)));
        }

        let mut rows = Vec::with_capacity(source_rows.len());
        for handle in handles {
            match handle.join() {
                Ok(Ok(mut chunk_rows)) => rows.append(&mut chunk_rows),
                Ok(Err(err)) => return Err(err),
                Err(_) => return Err("parallel SELECT worker panicked".into()),
            }
        }
        Ok(rows)
    })
}

fn filter_select_rows_serial(
    source_rows: &[Vec<Value>],
    col_names: &[String],
    filter: Option<&Expr>,
) -> Result<Vec<Vec<Value>>, String> {
    let mut rows = Vec::new();
    for row in source_rows {
        let keep = match filter {
            Some(pred) => eval_expr(pred, col_names, row)?.is_true(),
            None => true,
        };
        if keep {
            rows.push(row.clone());
        }
    }
    Ok(rows)
}

fn parallel_select_worker_count(row_count: usize) -> usize {
    if row_count < PARALLEL_SELECT_MIN_ROWS {
        return 1;
    }
    let available = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    available
        .min(PARALLEL_SELECT_MAX_WORKERS)
        .min(row_count)
        .max(1)
}

fn exec_set_select(db: &mut Database, mut sel: Select) -> Result<ExecResult, String> {
    let order_by = std::mem::take(&mut sel.order_by);
    let limit = sel.limit.take();
    let offset = sel.offset.take();
    let set_ops = std::mem::take(&mut sel.set_ops);

    let ExecResult::Rows {
        fields, mut rows, ..
    } = exec_select(db, sel)?
    else {
        return Err("set operation branch did not produce rows".into());
    };

    for set_op in set_ops {
        let ExecResult::Rows {
            fields: right_fields,
            rows: right_rows,
            ..
        } = exec_select(db, *set_op.select)?
        else {
            return Err("set operation branch did not produce rows".into());
        };
        if right_fields.len() != fields.len() {
            return Err("each set operation query must have the same number of columns".into());
        }
        rows = apply_set_operation(rows, right_rows, set_op.op, set_op.all);
    }

    if !order_by.is_empty() {
        sort_set_rows(&mut rows, &fields, &order_by)?;
    }

    let col_names: Vec<String> = fields.iter().map(|f| f.name.clone()).collect();
    let offset = eval_count(&offset, &col_names)?.unwrap_or(0);
    let limit = eval_count(&limit, &col_names)?;
    let start = offset.min(rows.len());
    let end = match limit {
        Some(limit) => (start + limit).min(rows.len()),
        None => rows.len(),
    };
    let rows = rows[start..end].to_vec();
    let tag = format!("SELECT {}", rows.len());
    Ok(ExecResult::Rows { fields, rows, tag })
}

fn apply_set_operation(
    left: Vec<Vec<Value>>,
    right: Vec<Vec<Value>>,
    op: SetOperator,
    all: bool,
) -> Vec<Vec<Value>> {
    match (op, all) {
        (SetOperator::Union, true) => left.into_iter().chain(right).collect(),
        (SetOperator::Union, false) => distinct_rows(left.into_iter().chain(right).collect()),
        (SetOperator::Intersect, _) => {
            let right_distinct = distinct_rows(right);
            distinct_rows(left)
                .into_iter()
                .filter(|row| right_distinct.iter().any(|r| r == row))
                .collect()
        }
        (SetOperator::Except, _) => {
            let right_distinct = distinct_rows(right);
            distinct_rows(left)
                .into_iter()
                .filter(|row| !right_distinct.iter().any(|r| r == row))
                .collect()
        }
    }
}

fn distinct_rows(rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for row in rows {
        if !out.iter().any(|seen| seen == &row) {
            out.push(row);
        }
    }
    out
}

fn sort_set_rows(
    rows: &mut [Vec<Value>],
    fields: &[FieldDescription],
    order_by: &[OrderByItem],
) -> Result<(), String> {
    let col_names: Vec<String> = fields.iter().map(|f| f.name.clone()).collect();
    let mut sort_keys = Vec::with_capacity(rows.len());
    for row in rows.iter() {
        let mut key = Vec::with_capacity(order_by.len());
        for ob in order_by {
            let value = if let Some(i) = positional_index(&ob.expr, row.len())? {
                row[i].clone()
            } else if let Some(i) = output_column_index(&ob.expr, fields) {
                row[i].clone()
            } else {
                eval_expr(&ob.expr, &col_names, row)?
            };
            key.push(value);
        }
        sort_keys.push(key);
    }
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    idx.sort_by(|&a, &b| {
        for (i, item) in order_by.iter().enumerate() {
            let ord = compare_values(&sort_keys[a][i], &sort_keys[b][i]).unwrap_or(Ordering::Equal);
            let ord = if item.asc { ord } else { ord.reverse() };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    });
    let sorted: Vec<Vec<Value>> = idx.into_iter().map(|i| rows[i].clone()).collect();
    rows.clone_from_slice(&sorted);
    Ok(())
}

fn is_single_generate_series_projection(items: &[SelectItem]) -> bool {
    matches!(
        items,
        [SelectItem::Expr {
            expr: Expr::Function { name, .. },
            ..
        }] if name.eq_ignore_ascii_case("generate_series")
    )
}

fn is_single_sequence_projection(items: &[SelectItem]) -> bool {
    matches!(
        items,
        [SelectItem::Expr {
            expr:
                Expr::Function {
                    name,
                    args,
                    star: false,
                    ..
                },
            ..
        }] if matches!(name.to_ascii_lowercase().as_str(), "nextval" | "currval" | "setval")
            && !args.is_empty()
    )
}

fn is_advisory_lock_projection(items: &[SelectItem]) -> bool {
    !items.is_empty()
        && items.iter().all(|item| {
            matches!(
                item,
                SelectItem::Expr {
                    expr:
                        Expr::Function {
                            name,
                            star: false,
                            ..
                        },
                    ..
                } if matches!(
                    name.to_ascii_lowercase().as_str(),
                    "pg_advisory_lock"
                        | "pg_try_advisory_lock"
                        | "pg_advisory_unlock"
                        | "pg_advisory_unlock_all"
                )
            )
        })
}

fn select_advisory_lock_functions(db: &mut Database, sel: &Select) -> Result<ExecResult, String> {
    let mut fields = Vec::with_capacity(sel.projection.len());
    let mut row = Vec::with_capacity(sel.projection.len());
    for item in &sel.projection {
        let SelectItem::Expr { expr, alias } = item else {
            unreachable!()
        };
        let Expr::Function { name, args, .. } = expr else {
            unreachable!()
        };
        let lname = name.to_ascii_lowercase();
        let value = match lname.as_str() {
            "pg_advisory_lock" => {
                let (classid, objid) = advisory_lock_key(args)?;
                db.advisory_lock(classid, objid);
                Value::Null
            }
            "pg_try_advisory_lock" => {
                let (classid, objid) = advisory_lock_key(args)?;
                Value::Bool(db.try_advisory_lock(classid, objid))
            }
            "pg_advisory_unlock" => {
                let (classid, objid) = advisory_lock_key(args)?;
                Value::Bool(db.advisory_unlock(classid, objid))
            }
            "pg_advisory_unlock_all" => {
                if !args.is_empty() {
                    return Err("pg_advisory_unlock_all() expects no arguments".into());
                }
                db.advisory_unlock_all();
                Value::Null
            }
            _ => unreachable!(),
        };
        let data_type = if matches!(value, Value::Bool(_)) {
            DataType::Bool
        } else {
            DataType::Text
        };
        fields.push(FieldDescription {
            name: alias.clone().unwrap_or_else(|| lname.clone()),
            data_type,
        });
        row.push(value);
    }
    Ok(ExecResult::Rows {
        fields,
        rows: vec![row],
        tag: "SELECT 1".into(),
    })
}

fn advisory_lock_key(args: &[Expr]) -> Result<(i64, i64), String> {
    match args {
        [key] => Ok((0, eval_int_arg(key)?)),
        [classid, objid] => Ok((eval_int_arg(classid)?, eval_int_arg(objid)?)),
        _ => Err("advisory lock functions expect one or two integer arguments".into()),
    }
}

fn select_sequence_function(db: &mut Database, sel: &Select) -> Result<ExecResult, String> {
    let SelectItem::Expr { expr, alias } = &sel.projection[0] else {
        unreachable!()
    };
    let Expr::Function { name, args, .. } = expr else {
        unreachable!()
    };
    let sequence_name = eval_expr(&args[0], &[], &[])?
        .to_text()
        .ok_or_else(|| format!("{}() sequence name must not be null", name))?;
    let value = match name.to_ascii_lowercase().as_str() {
        "nextval" => db.next_sequence_value(&sequence_name)?,
        "currval" => db.current_sequence_value(&sequence_name)?,
        "setval" => {
            let value = match args
                .get(1)
                .map(|expr| eval_expr(expr, &[], &[]))
                .transpose()?
            {
                Some(Value::Int(value)) => value,
                Some(other) => {
                    return Err(format!(
                        "setval() value must be integer, got {}",
                        other.inferred_type().sql_name()
                    ));
                }
                None => return Err("setval() expects a value".into()),
            };
            let called = match args
                .get(2)
                .map(|expr| eval_expr(expr, &[], &[]))
                .transpose()?
            {
                Some(Value::Bool(called)) => called,
                Some(Value::Null) | None => true,
                Some(other) => {
                    return Err(format!(
                        "setval() called flag must be boolean, got {}",
                        other.inferred_type().sql_name()
                    ));
                }
            };
            db.set_sequence_value(&sequence_name, value, called)?
        }
        _ => unreachable!(),
    };
    Ok(ExecResult::Rows {
        fields: vec![FieldDescription {
            name: alias.clone().unwrap_or_else(|| name.to_ascii_lowercase()),
            data_type: DataType::Int8,
        }],
        rows: vec![vec![Value::Int(value)]],
        tag: "SELECT 1".into(),
    })
}

fn select_generate_series(sel: &Select) -> Result<ExecResult, String> {
    let SelectItem::Expr {
        expr: Expr::Function { args, .. },
        alias,
    } = &sel.projection[0]
    else {
        unreachable!()
    };
    let values = eval_generate_series(args)?;
    let field_name = alias
        .clone()
        .unwrap_or_else(|| "generate_series".to_string());
    let rows: Vec<Vec<Value>> = values.into_iter().map(|v| vec![Value::Int(v)]).collect();
    let tag = format!("SELECT {}", rows.len());
    Ok(ExecResult::Rows {
        fields: vec![FieldDescription {
            name: field_name,
            data_type: DataType::Int8,
        }],
        rows,
        tag,
    })
}

fn eval_generate_series(args: &[Expr]) -> Result<Vec<i64>, String> {
    if !(2..=3).contains(&args.len()) {
        return Err("generate_series() expects 2 or 3 arguments".into());
    }
    let start = eval_int_arg(&args[0])?;
    let stop = eval_int_arg(&args[1])?;
    let step = if args.len() == 3 {
        eval_int_arg(&args[2])?
    } else {
        1
    };
    if step == 0 {
        return Err("step size cannot equal zero".into());
    }
    let mut out = Vec::new();
    let mut cur = start;
    if step > 0 {
        while cur <= stop {
            out.push(cur);
            cur = cur.saturating_add(step);
            if cur == i64::MAX && *out.last().unwrap() == i64::MAX {
                break;
            }
        }
    } else {
        while cur >= stop {
            out.push(cur);
            cur = cur.saturating_add(step);
            if cur == i64::MIN && *out.last().unwrap() == i64::MIN {
                break;
            }
        }
    }
    Ok(out)
}

fn eval_int_arg(expr: &Expr) -> Result<i64, String> {
    match eval_expr(expr, &[], &[])? {
        Value::Int(i) => Ok(i),
        Value::Float(f) => Ok(f.round() as i64),
        Value::Text(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("invalid input syntax for type integer: \"{s}\"")),
        Value::Bool(b) => Ok(b as i64),
        Value::Null => Err("generate_series() arguments must not be null".into()),
    }
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

    // The grouping sets to evaluate, in output order. The ordinary path is a
    // single set derived from `group_by`.
    let sets: Vec<Vec<Expr>> = if sel.grouping_sets.is_empty() {
        vec![sel.group_by.clone()]
    } else {
        sel.grouping_sets.clone()
    };

    // The union of all grouping columns across the sets: any of these that is
    // absent from the active set must read as NULL in that set's output rows.
    let mut all_group_exprs: Vec<Expr> = Vec::new();
    for set in &sets {
        for g in set {
            if !all_group_exprs.contains(g) {
                all_group_exprs.push(g.clone());
            }
        }
    }

    // One output row per surviving group, carrying ORDER BY sort keys. Sets are
    // emitted in order; within a set, groups keep first-seen order.
    let mut keyed: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    for set in &sets {
        grouped_set_rows(sel, set, &all_group_exprs, col_names, &fields, rows, &mut keyed)?;
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
    Ok(ExecResult::Rows {
        fields,
        rows: final_rows,
        tag,
    })
}

/// Evaluate a single grouping `set`: partition `rows` by the set's grouping
/// expressions (first-seen order), apply HAVING, and append `(sort_key, out)`
/// pairs to `keyed`. Projection columns that are grouping columns absent from
/// this set are emitted as NULL (per SQL grouping-set semantics).
#[allow(clippy::too_many_arguments)]
fn grouped_set_rows(
    sel: &Select,
    set: &[Expr],
    all_group_exprs: &[Expr],
    col_names: &[String],
    fields: &[FieldDescription],
    rows: &[Vec<Value>],
    keyed: &mut Vec<(Vec<Value>, Vec<Value>)>,
) -> Result<(), String> {
    // Grouping columns that are NULLed for this set: every grouping column used
    // anywhere that is not part of the active set.
    let inactive: Vec<&Expr> = all_group_exprs
        .iter()
        .filter(|g| !set.contains(g))
        .collect();

    // Partition rows into groups, preserving first-seen order.
    let groups: Vec<Vec<Vec<Value>>> = if set.is_empty() {
        if rows.is_empty() {
            // The grand-total / no-GROUP-BY set always yields one (empty) group
            // so that bare aggregates over an empty input still produce a row.
            vec![Vec::new()]
        } else {
            vec![rows.to_vec()]
        }
    } else {
        let mut keys: Vec<Vec<Value>> = Vec::new();
        let mut buckets: Vec<Vec<Vec<Value>>> = Vec::new();
        for row in rows {
            let mut key = Vec::with_capacity(set.len());
            for g in set {
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

    for group in &groups {
        if let Some(h) = &sel.having {
            let h = null_out_inactive(h, &inactive);
            if !eval_aggregate_expr(&h, col_names, group)?.is_true() {
                continue;
            }
        }
        let mut out = Vec::with_capacity(sel.projection.len());
        for item in &sel.projection {
            if let SelectItem::Expr { expr, .. } = item {
                let expr = null_out_inactive(expr, &inactive);
                out.push(eval_aggregate_expr(&expr, col_names, group)?);
            }
        }
        let mut sort_key = Vec::with_capacity(sel.order_by.len());
        for ob in &sel.order_by {
            // ORDER BY may use a position, an output alias, or an expression.
            let v = if let Some(i) = positional_index(&ob.expr, out.len())? {
                out[i].clone()
            } else if let Some(i) = output_column_index(&ob.expr, fields) {
                out[i].clone()
            } else {
                let e = null_out_inactive(&ob.expr, &inactive);
                eval_aggregate_expr(&e, col_names, group)?
            };
            sort_key.push(v);
        }
        keyed.push((sort_key, out));
    }
    Ok(())
}

/// Return a copy of `expr` with any subexpression equal to one of the
/// `inactive` grouping expressions replaced by `NULL`. Used to implement
/// grouping-set semantics, where a grouping column not part of the active set
/// reads as NULL. A no-op when `inactive` is empty (the ordinary GROUP BY path),
/// keeping that path byte-for-byte identical.
fn null_out_inactive(expr: &Expr, inactive: &[&Expr]) -> Expr {
    if inactive.is_empty() {
        return expr.clone();
    }
    if inactive.contains(&expr) {
        return Expr::Null;
    }
    match expr {
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(null_out_inactive(expr, inactive)),
        },
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(null_out_inactive(left, inactive)),
            right: Box::new(null_out_inactive(right, inactive)),
        },
        Expr::Cast { expr, target } => Expr::Cast {
            expr: Box::new(null_out_inactive(expr, inactive)),
            target: *target,
        },
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => Expr::Case {
            operand: operand
                .as_ref()
                .map(|o| Box::new(null_out_inactive(o, inactive))),
            whens: whens
                .iter()
                .map(|(c, r)| {
                    (
                        null_out_inactive(c, inactive),
                        null_out_inactive(r, inactive),
                    )
                })
                .collect(),
            else_expr: else_expr
                .as_ref()
                .map(|e| Box::new(null_out_inactive(e, inactive))),
        },
        // Aggregate arguments refer to the underlying rows, not the grouping
        // key, so they are left untouched; other expressions are returned as-is.
        _ => expr.clone(),
    }
}

/// Upper bound on recursive-CTE iterations, guarding against non-terminating
/// recursion (e.g. a missing termination predicate).
const MAX_RECURSIVE_ITERATIONS: usize = 100_000;

fn materialize_ctes(
    db: &mut Database,
    ctes: &[Cte],
    inherited_ctes: &CteMap,
) -> Result<CteMap, String> {
    let mut map = inherited_ctes.clone();
    for cte in ctes {
        // A data-modifying CTE body: run the statement and materialise its
        // RETURNING rows as the CTE relation.
        if let Some(stmt) = &cte.dml {
            let relation = materialize_dml_cte(db, cte, stmt)?;
            map.insert(cte.name.clone(), relation);
            continue;
        }
        // A `WITH RECURSIVE` CTE whose body is `base UNION [ALL] <term that
        // references the CTE>` is evaluated to a fixpoint; everything else is a
        // one-shot materialization.
        if cte.recursive
            && !cte.select.set_ops.is_empty()
            && recursive_term_self_references(cte)
        {
            let relation = materialize_recursive_cte(db, cte, &map)?;
            map.insert(cte.name.clone(), relation);
            continue;
        }
        let result = exec_select_with_ctes(db, (*cte.select).clone(), &map)?;
        let ExecResult::Rows { fields, rows, .. } = result else {
            return Err(format!("WITH query \"{}\" did not produce rows", cte.name));
        };
        let mut fields: Vec<(String, DataType)> = fields
            .into_iter()
            .map(|field| (field.name, field.data_type))
            .collect();
        apply_cte_column_aliases(cte, &mut fields)?;
        map.insert(cte.name.clone(), CteRelation { fields, rows });
    }
    Ok(map)
}

/// Execute a data-modifying CTE body and materialise its `RETURNING` rows as a
/// CTE relation. The statement must carry a `RETURNING` clause to feed an outer
/// query; without one the CTE is still run (for its effects) and yields no rows.
fn materialize_dml_cte(
    db: &mut Database,
    cte: &Cte,
    stmt: &Statement,
) -> Result<CteRelation, String> {
    match execute(db, (*stmt).clone())? {
        ExecResult::Rows { fields, rows, .. } => {
            let mut fields: Vec<(String, DataType)> = fields
                .into_iter()
                .map(|field| (field.name, field.data_type))
                .collect();
            apply_cte_column_aliases(cte, &mut fields)?;
            Ok(CteRelation { fields, rows })
        }
        // No RETURNING: the statement ran for its effect; the CTE is empty.
        _ => Ok(CteRelation {
            fields: Vec::new(),
            rows: Vec::new(),
        }),
    }
}

/// Whether the recursive term (the first set-op branch) of `cte` refers back to
/// the CTE's own name.
fn recursive_term_self_references(cte: &Cte) -> bool {
    cte.select
        .set_ops
        .iter()
        .any(|op| select_references_table(&op.select, &cte.name))
}

/// Whether `sel` reads from a relation named `name` (in its FROM base, joins,
/// or any of its set-op branches).
fn select_references_table(sel: &Select, name: &str) -> bool {
    if let Some(from) = &sel.from {
        if from.base.name == name || from.joins.iter().any(|j| j.table.name == name) {
            return true;
        }
    }
    sel.set_ops
        .iter()
        .any(|op| select_references_table(&op.select, name))
}

/// Evaluate a recursive CTE using the standard working-table algorithm: seed
/// with the base term, then repeatedly evaluate the recursive term (which sees
/// only the previous iteration's new rows) until no new rows are produced.
fn materialize_recursive_cte(
    db: &mut Database,
    cte: &Cte,
    outer: &CteMap,
) -> Result<CteRelation, String> {
    // Split `base UNION[ALL] recursive` into its two terms. UNION (not ALL)
    // deduplicates the accumulated result.
    let mut base_select = (*cte.select).clone();
    let set_ops = std::mem::take(&mut base_select.set_ops);
    let union_all = set_ops[0].all;
    let recursive_select = (*set_ops[0].select).clone();

    // Seed: evaluate the base term.
    let ExecResult::Rows { fields, rows, .. } =
        exec_select_with_ctes(db, base_select, outer)?
    else {
        return Err(format!("WITH query \"{}\" did not produce rows", cte.name));
    };
    let mut fields: Vec<(String, DataType)> = fields
        .into_iter()
        .map(|field| (field.name, field.data_type))
        .collect();
    apply_cte_column_aliases(cte, &mut fields)?;

    let mut result = if union_all { rows.clone() } else { distinct_rows(rows) };
    let mut working = result.clone();
    let mut iterations = 0;

    while !working.is_empty() {
        iterations += 1;
        if iterations > MAX_RECURSIVE_ITERATIONS {
            return Err(format!(
                "recursive query \"{}\" exceeded {MAX_RECURSIVE_ITERATIONS} iterations",
                cte.name
            ));
        }

        // The recursive term sees the working table as the CTE's contents.
        let mut iter_map = outer.clone();
        iter_map.insert(
            cte.name.clone(),
            CteRelation {
                fields: fields.clone(),
                rows: std::mem::take(&mut working),
            },
        );
        let ExecResult::Rows { rows: produced, .. } =
            exec_select_with_ctes(db, recursive_select.clone(), &iter_map)?
        else {
            return Err(format!("WITH query \"{}\" did not produce rows", cte.name));
        };

        if union_all {
            if produced.is_empty() {
                break;
            }
            result.extend(produced.iter().cloned());
            working = produced;
        } else {
            // Keep only genuinely new rows (not already in the result).
            let fresh: Vec<Vec<Value>> = produced
                .into_iter()
                .filter(|r| !result.contains(r))
                .collect();
            let fresh = distinct_rows(fresh);
            if fresh.is_empty() {
                break;
            }
            result.extend(fresh.iter().cloned());
            working = fresh;
        }
    }

    Ok(CteRelation { fields, rows: result })
}

fn exec_update(db: &mut Database, mut upd: Update) -> Result<ExecResult, String> {
    // Resolve any uncorrelated subqueries in SET expressions / WHERE first.
    let no_ctes = CteMap::new();
    for (_, e) in &mut upd.assignments {
        resolve_subqueries(db, e, &[], &no_ctes)?;
    }
    if let Some(f) = &mut upd.filter {
        resolve_subqueries(db, f, &[], &no_ctes)?;
    }
    let from_source = match &upd.from {
        Some(from) => Some(build_source(db, from, None)?),
        None => None,
    };
    let table = db
        .table(&upd.table)
        .ok_or_else(|| format!("relation \"{}\" does not exist", upd.table))?;
    let target_col_names = table.column_names();
    let columns = table.columns.clone();
    let col_names = dml_col_names(&upd.table, &target_col_names, from_source.as_ref());

    // Resolve assignment target indices up front.
    let mut targets = Vec::with_capacity(upd.assignments.len());
    for (name, expr) in &upd.assignments {
        let idx = columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| {
                format!(
                    "column \"{name}\" of relation \"{}\" does not exist",
                    upd.table
                )
            })?;
        if columns[idx].generated.is_some() {
            return Err(format!(
                "column \"{name}\" can only be updated to DEFAULT because it is a generated column"
            ));
        }
        targets.push((idx, expr.clone()));
    }

    // Pick the candidate row positions: an index when the filter allows it,
    // otherwise every row. The predicate is re-checked below regardless, so
    // the index can only narrow the set, never change the result.
    let candidates = if from_source.is_some() {
        (0..table.rows.len()).collect()
    } else {
        candidate_positions(table, &upd.filter, &target_col_names)?
    };

    let mut new_versions: Vec<(usize, Vec<Value>)> = Vec::new();
    let mut affected = Vec::new();
    for pos in candidates {
        let row = &table.rows[pos];
        let source_row = first_dml_source_row(row, from_source.as_ref(), &upd.filter, &col_names)?;
        let Some(source_row) = source_row else {
            continue;
        };
        let mut new_row = row.clone();
        for (idx, expr) in &targets {
            let eval_row = dml_eval_row(&new_row, source_row.as_ref());
            let val = eval_expr(expr, &col_names, &eval_row)?;
            new_row[*idx] = coerce(val, columns[*idx].data_type)?;
        }
        apply_generated_columns(&columns, &mut new_row)?;
        affected.push(new_row.clone());
        new_versions.push((pos, new_row));
    }

    for (_, new_row) in &new_versions {
        check_row_constraints(table, new_row)?;
    }
    for (_, new_row) in &new_versions {
        enforce_user_types(db, &columns, new_row)?;
    }
    for (_, new_row) in &new_versions {
        check_foreign_key_constraints(db, &upd.table, new_row)?;
    }
    for (pos, new_row) in &new_versions {
        for child_name in db.table_names() {
            let child = db.table(&child_name).expect("name came from table_names");
            for constraint in child.foreign_key_constraints() {
                if constraint.ref_table != upd.table {
                    continue;
                }
                let ref_idx = table
                    .column_index(&constraint.ref_column)
                    .expect("referenced column validated when constraint was created");
                if compare_values(&table.rows[*pos][ref_idx], &new_row[ref_idx])
                    != Some(Ordering::Equal)
                {
                    check_parent_key_not_referenced(db, &upd.table, &table.rows[*pos])?;
                }
            }
        }
    }

    // Enforce unique constraints before applying any change (atomic): each new
    // row must not collide with another row (excluding its own position) or
    // with another row updated in the same statement.
    for (pos, new_row) in &new_versions {
        if let Some(name) = table.unique_violation(new_row, Some(*pos)) {
            return Err(format!(
                "duplicate key value violates unique constraint \"{name}\""
            ));
        }
    }
    for columns in table.unique_key_columns() {
        if rows_have_duplicate_unique_key(new_versions.iter().map(|(_, row)| row), &columns) {
            return Err("duplicate key value violates unique constraint".into());
        }
    }

    let n = affected.len();
    let tag = format!("UPDATE {n}");
    let result = returning_result(&upd.returning, &columns, &affected, tag)?;
    let table = db.table_mut(&upd.table).expect("table existed above");
    // Apply through `update_row` so each touched index is repaired in place.
    for (pos, new_row) in new_versions {
        table.update_row(pos, new_row);
    }
    fire_row_triggers(db, &upd.table, "update", false, n)?;
    Ok(result)
}

fn dml_col_names(
    target_table: &str,
    target_cols: &[String],
    source: Option<&(Vec<String>, Vec<DataType>, Vec<Vec<Value>>)>,
) -> Vec<String> {
    let mut names = Vec::new();
    if source.is_some() {
        names.extend(
            target_cols
                .iter()
                .map(|name| format!("{target_table}.{name}")),
        );
    } else {
        names.extend(target_cols.iter().cloned());
    }
    if let Some((source_names, _, _)) = source {
        names.extend(source_names.iter().cloned());
    }
    names
}

fn first_dml_source_row(
    target_row: &[Value],
    source: Option<&(Vec<String>, Vec<DataType>, Vec<Vec<Value>>)>,
    filter: &Option<Expr>,
    col_names: &[String],
) -> Result<Option<Option<Vec<Value>>>, String> {
    match source {
        Some((_, _, source_rows)) => {
            for source_row in source_rows {
                let eval_row = dml_eval_row(target_row, Some(source_row));
                let matches = match filter {
                    Some(pred) => eval_expr(pred, col_names, &eval_row)?.is_true(),
                    None => true,
                };
                if matches {
                    return Ok(Some(Some(source_row.clone())));
                }
            }
            Ok(None)
        }
        None => {
            let matches = match filter {
                Some(pred) => eval_expr(pred, col_names, target_row)?.is_true(),
                None => true,
            };
            Ok(matches.then_some(None))
        }
    }
}

fn dml_eval_row(target_row: &[Value], source_row: Option<&Vec<Value>>) -> Vec<Value> {
    let mut row = target_row.to_vec();
    if let Some(source_row) = source_row {
        row.extend(source_row.iter().cloned());
    }
    row
}

fn exec_delete(db: &mut Database, mut del: Delete) -> Result<ExecResult, String> {
    if let Some(f) = &mut del.filter {
        resolve_subqueries(db, f, &[], &CteMap::new())?;
    }
    let using_source = match &del.using {
        Some(using) => Some(build_source(db, using, None)?),
        None => None,
    };
    let table = db
        .table(&del.table)
        .ok_or_else(|| format!("relation \"{}\" does not exist", del.table))?;
    let target_col_names = table.column_names();
    let columns = table.columns.clone();
    let col_names = dml_col_names(&del.table, &target_col_names, using_source.as_ref());

    let candidates = if using_source.is_some() {
        (0..table.rows.len()).collect()
    } else {
        candidate_positions(table, &del.filter, &target_col_names)?
    };
    // Build the matching positions in ascending row order so RETURNING and the
    // command tag match the full-scan path exactly.
    let mut matching = std::collections::BTreeSet::new();
    for pos in candidates {
        if first_dml_source_row(
            &table.rows[pos],
            using_source.as_ref(),
            &del.filter,
            &col_names,
        )?
        .is_some()
        {
            matching.insert(pos);
        }
    }
    let positions: Vec<usize> = matching.into_iter().collect();
    let affected: Vec<Vec<Value>> = positions.iter().map(|&p| table.rows[p].clone()).collect();
    for row in &affected {
        check_parent_key_not_referenced(db, &del.table, row)?;
    }

    let n = affected.len();
    let tag = format!("DELETE {n}");
    let result = returning_result(&del.returning, &columns, &affected, tag)?;
    let table = db.table_mut(&del.table).expect("table existed above");
    table.delete_rows(&positions);
    fire_row_triggers(db, &del.table, "delete", false, n)?;
    Ok(result)
}

/// Materialize a MERGE source into qualified column names plus row data.
/// Column names are qualified with the source's alias, mirroring how joins
/// expose `alias.col` so the ON predicate and action expressions can refer to
/// either side unambiguously.
fn build_merge_source(
    db: &mut Database,
    source: &MergeSource,
) -> Result<(Vec<String>, Vec<Vec<Value>>), String> {
    let qualifier = source.qualifier().to_string();
    match source {
        MergeSource::Table { name, alias } => {
            let tref = TableRef {
                schema: None,
                name: name.clone(),
                args: Vec::new(),
                alias: alias.clone(),
                subquery: None,
                lateral: false,
            };
            let (names, _, rows) = resolve_source_table(db, &tref, &CteMap::new())?;
            Ok((names, rows))
        }
        MergeSource::Subquery { select, .. } => {
            let fields = select_fields(db, select)?;
            let ExecResult::Rows { rows, .. } = exec_select(db, (**select).clone())? else {
                return Err("MERGE source subquery did not produce rows".into());
            };
            let names = fields
                .into_iter()
                .map(|f| format!("{qualifier}.{}", f.name))
                .collect();
            Ok((names, rows))
        }
        MergeSource::Values {
            rows,
            columns,
            ..
        } => {
            let width = rows.first().map(|r| r.len()).unwrap_or(0);
            let names: Vec<String> = (0..width)
                .map(|i| {
                    let base = columns
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| format!("column{}", i + 1));
                    format!("{qualifier}.{base}")
                })
                .collect();
            let mut out = Vec::with_capacity(rows.len());
            for tuple in rows {
                if tuple.len() != width {
                    return Err("VALUES lists must all be the same length".into());
                }
                let mut vals = Vec::with_capacity(width);
                for e in tuple {
                    vals.push(eval_expr(e, &[], &[])?);
                }
                out.push(vals);
            }
            Ok((names, out))
        }
    }
}

fn exec_merge(db: &mut Database, mut merge: Merge) -> Result<ExecResult, String> {
    let no_ctes = CteMap::new();
    // Resolve any uncorrelated subqueries in the ON condition and clause exprs.
    resolve_subqueries(db, &mut merge.on, &[], &no_ctes)?;
    for when in &mut merge.clauses {
        if let Some(c) = &mut when.condition {
            resolve_subqueries(db, c, &[], &no_ctes)?;
        }
        match &mut when.action {
            MergeAction::Update { assignments } => {
                for (_, e) in assignments {
                    resolve_subqueries(db, e, &[], &no_ctes)?;
                }
            }
            MergeAction::Insert { values, .. } => {
                for e in values {
                    resolve_subqueries(db, e, &[], &no_ctes)?;
                }
            }
            MergeAction::Delete | MergeAction::DoNothing => {}
        }
    }

    let (source_names, source_rows) = build_merge_source(db, &merge.source)?;

    let table = db
        .table(&merge.target)
        .ok_or_else(|| format!("relation \"{}\" does not exist", merge.target))?;
    let columns = table.columns.clone();
    let target_qualifier = merge.target_qualifier().to_string();
    // Combined namespace: target columns (qualified) ++ source columns. Target
    // columns are exposed both bare and qualified so unqualified references in
    // assignments/INSERT values resolve, while the ON clause can disambiguate.
    let target_names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
    let target_qualified: Vec<String> = columns
        .iter()
        .map(|c| format!("{target_qualifier}.{}", c.name))
        .collect();
    // Names used for ON / clause conditions: qualified target ++ source.
    let mut cond_names = target_qualified.clone();
    cond_names.extend(source_names.iter().cloned());
    // Names used for action expressions: bare target ++ qualified target ++
    // source, so both `col` and `alias.col` resolve.
    let mut action_names = target_names.clone();
    action_names.extend(target_qualified.iter().cloned());
    action_names.extend(source_names.iter().cloned());

    // Validate clause structure and resolve assignment/INSERT target columns.
    for when in &merge.clauses {
        match &when.action {
            MergeAction::Update { assignments } if when.matched => {
                for (name, _) in assignments {
                    let col = columns.iter().find(|c| &c.name == name).ok_or_else(|| {
                        format!(
                            "column \"{name}\" of relation \"{}\" does not exist",
                            merge.target
                        )
                    })?;
                    if col.generated.is_some() {
                        return Err(format!(
                            "column \"{name}\" can only be updated to DEFAULT because it is a generated column"
                        ));
                    }
                }
            }
            MergeAction::Update { .. } => {
                return Err("UPDATE is only allowed in WHEN MATCHED clauses".into());
            }
            MergeAction::Delete if !when.matched => {
                return Err("DELETE is only allowed in WHEN MATCHED clauses".into());
            }
            MergeAction::Insert { .. } if when.matched => {
                return Err("INSERT is only allowed in WHEN NOT MATCHED clauses".into());
            }
            MergeAction::Insert { columns: cols, .. } => {
                if let Some(cols) = cols {
                    for name in cols {
                        if !columns.iter().any(|c| &c.name == name) {
                            return Err(format!(
                                "column \"{name}\" of relation \"{}\" does not exist",
                                merge.target
                            ));
                        }
                    }
                }
            }
            MergeAction::Delete | MergeAction::DoNothing => {}
        }
    }

    let mut touched: HashSet<usize> = HashSet::new();
    let mut deletes: Vec<usize> = Vec::new();
    let mut updates: Vec<(usize, Vec<Value>)> = Vec::new();
    let mut inserts: Vec<Vec<Value>> = Vec::new();
    let mut affected = 0usize;

    for source_row in &source_rows {
        // Find target rows matching the ON condition (skipping any already
        // modified by an earlier source row: each target row is acted on once).
        let table = db.table(&merge.target).expect("target existed above");
        let mut matches: Vec<usize> = Vec::new();
        for pos in 0..table.rows.len() {
            if touched.contains(&pos) {
                continue;
            }
            let mut eval_row = table.rows[pos].clone();
            eval_row.extend(source_row.iter().cloned());
            if eval_expr(&merge.on, &cond_names, &eval_row)?.is_true() {
                matches.push(pos);
            }
        }

        if matches.is_empty() {
            // NOT MATCHED: pick the first applicable WHEN NOT MATCHED clause.
            let mut action_row: Vec<Value> = vec![Value::Null; columns.len()];
            action_row.extend(target_qualified.iter().map(|_| Value::Null));
            action_row.extend(source_row.iter().cloned());
            for when in &merge.clauses {
                if when.matched {
                    continue;
                }
                if let Some(cond) = &when.condition {
                    if !eval_expr(cond, &action_names, &action_row)?.is_true() {
                        continue;
                    }
                }
                match &when.action {
                    MergeAction::DoNothing => {}
                    MergeAction::Insert {
                        columns: cols,
                        values,
                        default_values,
                    } => {
                        let target_indices: Vec<usize> = if *default_values {
                            Vec::new()
                        } else {
                            match cols {
                                Some(names) => names
                                    .iter()
                                    .map(|n| {
                                        columns.iter().position(|c| &c.name == n).expect(
                                            "INSERT column existence validated above",
                                        )
                                    })
                                    .collect(),
                                None => (0..columns.len()).collect(),
                            }
                        };
                        if values.len() != target_indices.len() {
                            return Err(format!(
                                "MERGE INSERT has {} expressions but {} target columns",
                                values.len(),
                                target_indices.len()
                            ));
                        }
                        let mut row = vec![Value::Null; columns.len()];
                        for (expr, &col_idx) in values.iter().zip(&target_indices) {
                            let val = eval_expr(expr, &action_names, &action_row)?;
                            row[col_idx] = coerce(val, columns[col_idx].data_type)?;
                        }
                        finish_insert_row(
                            db,
                            &merge.target,
                            &columns,
                            &target_indices,
                            false,
                            &mut row,
                        )?;
                        inserts.push(row);
                        affected += 1;
                    }
                    _ => unreachable!("validated NOT MATCHED action above"),
                }
                break;
            }
        } else {
            // MATCHED: pick the first applicable WHEN MATCHED clause, then apply
            // it to every matched target row.
            for &pos in &matches {
                let existing = db
                    .table(&merge.target)
                    .expect("target existed above")
                    .rows[pos]
                    .clone();
                let mut action_row = existing.clone();
                action_row.extend(existing.iter().cloned());
                action_row.extend(source_row.iter().cloned());
                for when in &merge.clauses {
                    if !when.matched {
                        continue;
                    }
                    if let Some(cond) = &when.condition {
                        if !eval_expr(cond, &action_names, &action_row)?.is_true() {
                            continue;
                        }
                    }
                    match &when.action {
                        MergeAction::DoNothing => {
                            touched.insert(pos);
                        }
                        MergeAction::Delete => {
                            touched.insert(pos);
                            deletes.push(pos);
                            affected += 1;
                        }
                        MergeAction::Update { assignments } => {
                            let mut new_row = existing.clone();
                            for (name, expr) in assignments {
                                let idx = columns
                                    .iter()
                                    .position(|c| &c.name == name)
                                    .expect("assignment target validated above");
                                let val = eval_expr(expr, &action_names, &action_row)?;
                                new_row[idx] = coerce(val, columns[idx].data_type)?;
                            }
                            apply_generated_columns(&columns, &mut new_row)?;
                            touched.insert(pos);
                            updates.push((pos, new_row));
                            affected += 1;
                        }
                        MergeAction::Insert { .. } => {
                            unreachable!("validated MATCHED action above")
                        }
                    }
                    break;
                }
            }
        }
    }

    // Validate constraints for every new/updated row before mutating anything.
    {
        let table = db.table(&merge.target).expect("target existed above");
        for (_, row) in &updates {
            check_row_constraints(table, row)?;
        }
        for row in &inserts {
            check_row_constraints(table, row)?;
        }
    }
    for (_, row) in &updates {
        enforce_user_types(db, &columns, row)?;
    }
    for row in &inserts {
        enforce_user_types(db, &columns, row)?;
    }
    for (_, row) in &updates {
        check_foreign_key_constraints(db, &merge.target, row)?;
    }
    for row in &inserts {
        check_foreign_key_constraints(db, &merge.target, row)?;
    }
    {
        let table = db.table(&merge.target).expect("target existed above");
        for &pos in &deletes {
            check_parent_key_not_referenced(db, &merge.target, &table.rows[pos])?;
        }
        // Unique checks: each updated row against existing data (excluding its
        // own position) and inserts against the table; plus cross-checks among
        // the new rows produced by this statement.
        for (pos, row) in &updates {
            if let Some(name) = table.unique_violation(row, Some(*pos)) {
                return Err(format!(
                    "duplicate key value violates unique constraint \"{name}\""
                ));
            }
        }
        for row in &inserts {
            if let Some(name) = table.unique_violation(row, None) {
                return Err(format!(
                    "duplicate key value violates unique constraint \"{name}\""
                ));
            }
        }
        for key_cols in table.unique_key_columns() {
            if rows_have_duplicate_unique_key(
                inserts.iter().chain(updates.iter().map(|(_, r)| r)),
                &key_cols,
            ) {
                return Err("duplicate key value violates unique constraint".into());
            }
        }
    }

    // Apply: updates keep their position; deletes are applied last so positions
    // referenced by updates/deletes stay valid; inserts append.
    let table = db.table_mut(&merge.target).expect("target existed above");
    for (pos, row) in updates {
        table.update_row(pos, row);
    }
    if !deletes.is_empty() {
        deletes.sort_unstable();
        deletes.dedup();
        table.delete_rows(&deletes);
    }
    for row in inserts {
        table.push_row(row);
    }

    Ok(ExecResult::Command(format!("MERGE {affected}")))
}

fn exec_set(db: &mut Database, name: String, value: String) -> Result<ExecResult, String> {
    if name.eq_ignore_ascii_case("search_path") {
        db.set_search_path(&value);
    }
    Ok(ExecResult::Command("SET".into()))
}

fn exec_show(db: &Database, name: String) -> Result<ExecResult, String> {
    let value = match name.to_ascii_lowercase().as_str() {
        // Honor PGRS_SERVER_VERSION so compatibility mode is visible to SQL too.
        "server_version" => std::env::var("PGRS_SERVER_VERSION")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "16.0 (postgres-rs)".to_string()),
        "server_encoding" | "client_encoding" => "UTF8".to_string(),
        "transaction_isolation" => "read committed".to_string(),
        "search_path" => db.search_path(),
        "current_schema" => db.current_schema(),
        _ => String::new(),
    };
    Ok(ExecResult::Rows {
        fields: vec![FieldDescription {
            name,
            data_type: DataType::Text,
        }],
        rows: vec![vec![Value::Text(value)]],
        tag: "SHOW".to_string(),
    })
}

// --- expression evaluation ---------------------------------------------------

/// Evaluate a scalar expression against a row. `col_names`/`row` give the
/// current tuple's columns; both may be empty for constant expressions.
pub(crate) fn eval_expr(expr: &Expr, col_names: &[String], row: &[Value]) -> Result<Value, String> {
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
        Expr::QuantifiedCompare {
            left,
            op,
            quantifier,
            list,
        } => eval_quantified_compare(left, *op, *quantifier, list, col_names, row),
        Expr::Row(items) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                values.push(eval_expr(item, col_names, row)?);
            }
            Ok(Value::Text(row_value_to_text(&values)))
        }
        Expr::Array(items) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                values.push(eval_expr(item, col_names, row)?);
            }
            Ok(Value::Text(array_value_to_text(&values)))
        }
        Expr::IsNull { expr, negated } => {
            let v = eval_expr(expr, col_names, row)?;
            let is_null = v.is_null();
            Ok(Value::Bool(if *negated { !is_null } else { is_null }))
        }
        Expr::IsDistinctFrom {
            left,
            right,
            negated,
        } => {
            let l = eval_expr(left, col_names, row)?;
            let r = eval_expr(right, col_names, row)?;
            let distinct = match (l.is_null(), r.is_null()) {
                (true, true) => false,
                (true, false) | (false, true) => true,
                (false, false) => compare_values(&l, &r) != Some(Ordering::Equal),
            };
            Ok(Value::Bool(if *negated { !distinct } else { distinct }))
        }
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => {
            let v = eval_expr(expr, col_names, row)?;
            let p = eval_expr(pattern, col_names, row)?;
            if v.is_null() || p.is_null() {
                return Ok(Value::Null);
            }
            let (text, pat) = (
                v.to_text().unwrap_or_default(),
                p.to_text().unwrap_or_default(),
            );
            let m = like_match(&text, &pat, *case_insensitive);
            Ok(Value::Bool(if *negated { !m } else { m }))
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => eval_in_list(expr, list, *negated, col_names, row),
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let v = eval_expr(expr, col_names, row)?;
            let lo = eval_expr(low, col_names, row)?;
            let hi = eval_expr(high, col_names, row)?;
            if v.is_null() || lo.is_null() || hi.is_null() {
                return Ok(Value::Null);
            }
            let ge = compare_values(&v, &lo)
                .map(|o| o != Ordering::Less)
                .unwrap_or(false);
            let le = compare_values(&v, &hi)
                .map(|o| o != Ordering::Greater)
                .unwrap_or(false);
            let within = ge && le;
            Ok(Value::Bool(if *negated { !within } else { within }))
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            let operand_val = match operand {
                Some(o) => Some(eval_expr(o, col_names, row)?),
                None => None,
            };
            for (cond, result) in whens {
                let hit = match &operand_val {
                    // Simple CASE: compare operand to each WHEN value.
                    Some(o) => {
                        let c = eval_expr(cond, col_names, row)?;
                        !o.is_null()
                            && !c.is_null()
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
        Expr::Function {
            name, args, star, ..
        } => eval_scalar_function(name, args, *star, col_names, row),
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
            other => Err(format!(
                "cannot negate {}",
                other.inferred_type().sql_name()
            )),
        },
        UnaryOp::Not => Ok(Value::Bool(!v.is_true())),
    }
}

fn eval_binary(op: BinaryOp, l: Value, r: Value) -> Result<Value, String> {
    use BinaryOp::*;
    // NULL propagation for non-logical operators.
    if matches!(
        op,
        Add | Sub
            | Mul
            | Div
            | Mod
            | Concat
            | JsonGet
            | JsonGetText
            | ArrayContains
            | ArrayContainedBy
            | ArrayOverlap
            | NetworkContainedBy
            | NetworkContainedByEq
            | NetworkContains
            | NetworkContainsEq
            | TextSearchMatch
            | Eq
            | NotEq
            | Lt
            | LtEq
            | Gt
            | GtEq
            | RegexMatch { .. }
            | RegexNotMatch { .. }
    ) && (l.is_null() || r.is_null())
    {
        return Ok(Value::Null);
    }
    match op {
        Add | Sub | Mul | Div | Mod => arithmetic(op, l, r),
        JsonGet => json_extract(l, r, false),
        JsonGetText => json_extract(l, r, true),
        ArrayContains | ArrayContainedBy | ArrayOverlap => array_operator(op, l, r),
        NetworkContainedBy | NetworkContainedByEq | NetworkContains | NetworkContainsEq => {
            network_operator(op, l, r)
        }
        TextSearchMatch => Ok(Value::Bool(text_search_match(&l, &r))),
        RegexMatch { ci } => {
            let m = regex_match(
                &r.to_text().unwrap_or_default(),
                &l.to_text().unwrap_or_default(),
                ci,
            );
            Ok(Value::Bool(m))
        }
        RegexNotMatch { ci } => {
            let m = regex_match(
                &r.to_text().unwrap_or_default(),
                &l.to_text().unwrap_or_default(),
                ci,
            );
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

fn array_operator(op: BinaryOp, left: Value, right: Value) -> Result<Value, String> {
    if matches!(op, BinaryOp::ArrayOverlap) {
        if let (Some(left_net), Some(right_net)) = (
            left.to_text().and_then(|text| parse_ipv4_network(&text)),
            right.to_text().and_then(|text| parse_ipv4_network(&text)),
        ) {
            return Ok(Value::Bool(networks_overlap(left_net, right_net)));
        }
    }
    let Some(left_text) = left.to_text() else {
        return Ok(Value::Null);
    };
    let Some(right_text) = right.to_text() else {
        return Ok(Value::Null);
    };
    let left_values = parse_array_text(&left_text)
        .ok_or_else(|| "array operator requires array values".to_string())?;
    let right_values = parse_array_text(&right_text)
        .ok_or_else(|| "array operator requires array values".to_string())?;
    let contains = |haystack: &[Option<String>], needles: &[Option<String>]| {
        needles.iter().all(|needle| {
            haystack
                .iter()
                .any(|candidate| candidate.as_deref() == needle.as_deref())
        })
    };
    let overlaps = left_values.iter().any(|left| {
        right_values
            .iter()
            .any(|right| left.as_deref() == right.as_deref())
    });
    let result = match op {
        BinaryOp::ArrayContains => contains(&left_values, &right_values),
        BinaryOp::ArrayContainedBy => contains(&right_values, &left_values),
        BinaryOp::ArrayOverlap => overlaps,
        _ => unreachable!(),
    };
    Ok(Value::Bool(result))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ipv4Network {
    addr: u32,
    prefix: u8,
}

fn network_operator(op: BinaryOp, left: Value, right: Value) -> Result<Value, String> {
    let Some(left_text) = left.to_text() else {
        return Ok(Value::Null);
    };
    let Some(right_text) = right.to_text() else {
        return Ok(Value::Null);
    };
    let left = parse_ipv4_network(&left_text)
        .ok_or_else(|| "network operator requires inet/cidr values".to_string())?;
    let right = parse_ipv4_network(&right_text)
        .ok_or_else(|| "network operator requires inet/cidr values".to_string())?;
    let result = match op {
        BinaryOp::NetworkContainedBy => network_contains(right, left) && left != right,
        BinaryOp::NetworkContainedByEq => network_contains(right, left),
        BinaryOp::NetworkContains => network_contains(left, right) && left != right,
        BinaryOp::NetworkContainsEq => network_contains(left, right),
        _ => unreachable!(),
    };
    Ok(Value::Bool(result))
}

fn parse_ipv4_network(input: &str) -> Option<Ipv4Network> {
    let (addr, prefix) = match input.split_once('/') {
        Some((addr, prefix)) => (addr, prefix.parse::<u8>().ok()?),
        None => (input, 32),
    };
    if prefix > 32 {
        return None;
    }
    let mut octets = [0u8; 4];
    let mut parts = addr.split('.');
    for octet in &mut octets {
        *octet = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    let addr = u32::from_be_bytes(octets);
    Some(Ipv4Network { addr, prefix })
}

fn network_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

fn network_start(network: Ipv4Network) -> u32 {
    network.addr & network_mask(network.prefix)
}

fn network_end(network: Ipv4Network) -> u32 {
    network_start(network) | !network_mask(network.prefix)
}

fn network_contains(parent: Ipv4Network, child: Ipv4Network) -> bool {
    child.prefix >= parent.prefix
        && (child.addr & network_mask(parent.prefix)) == network_start(parent)
}

fn networks_overlap(left: Ipv4Network, right: Ipv4Network) -> bool {
    network_start(left) <= network_end(right) && network_start(right) <= network_end(left)
}

fn text_search_terms(input: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            terms.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        terms.push(current);
    }
    terms
}

fn to_tsvector_text(input: &str) -> String {
    let mut positions: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (idx, term) in text_search_terms(input).into_iter().enumerate() {
        positions.entry(term).or_default().push(idx + 1);
    }
    positions
        .into_iter()
        .map(|(term, positions)| {
            let positions = positions
                .into_iter()
                .map(|pos| pos.to_string())
                .collect::<Vec<_>>()
                .join(",");
            format!("'{term}':{positions}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn plainto_tsquery_text(input: &str) -> String {
    text_search_terms(input)
        .into_iter()
        .map(|term| format!("'{term}'"))
        .collect::<Vec<_>>()
        .join(" & ")
}

fn to_tsquery_text(input: &str) -> String {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch.to_ascii_lowercase());
        } else {
            if !current.is_empty() {
                out.push(format!("'{}'", std::mem::take(&mut current)));
            }
            match ch {
                '&' | '|' | '!' | '(' | ')' => out.push(ch.to_string()),
                _ => {}
            }
        }
    }
    if !current.is_empty() {
        out.push(format!("'{current}'"));
    }
    out.join(" ")
}

fn text_search_match(vector: &Value, query: &Value) -> bool {
    let vector_terms: HashSet<String> = vector
        .to_text()
        .map(|text| text_search_terms(&text).into_iter().collect())
        .unwrap_or_default();
    let query_terms = query
        .to_text()
        .map(|text| text_search_terms(&text))
        .unwrap_or_default();
    !query_terms.is_empty()
        && query_terms
            .into_iter()
            .all(|term| vector_terms.contains(&term))
}

fn ts_rank_text(vector: &Value, query: &Value) -> f64 {
    let vector_terms: HashSet<String> = vector
        .to_text()
        .map(|text| text_search_terms(&text).into_iter().collect())
        .unwrap_or_default();
    let query_terms = query
        .to_text()
        .map(|text| text_search_terms(&text))
        .unwrap_or_default();
    if query_terms.is_empty() {
        return 0.0;
    }
    let matches = query_terms
        .iter()
        .filter(|term| vector_terms.contains(*term))
        .count();
    matches as f64 / query_terms.len() as f64
}

fn json_extract(source: Value, key: Value, as_text: bool) -> Result<Value, String> {
    let Some(json) = source.to_text() else {
        return Ok(Value::Null);
    };
    let Some(raw) = json_lookup(json.trim(), &key)? else {
        return Ok(Value::Null);
    };
    if as_text {
        json_to_text(raw)
    } else {
        Ok(Value::Text(raw.trim().to_string()))
    }
}

fn json_path_text(source: &Value, path: &[Value]) -> Result<Value, String> {
    let Some(json) = source.to_text() else {
        return Ok(Value::Null);
    };
    let mut current = json.trim();
    for key in path {
        let Some(raw) = json_lookup(current, key)? else {
            return Ok(Value::Null);
        };
        current = raw.trim();
    }
    json_to_text(current)
}

/// One step of a (simplified) SQL/JSON path.
enum JsonPathStep {
    /// `.key`
    Key(String),
    /// `[n]`
    Index(usize),
    /// `.*` (all members of an object) or `[*]` (all array elements)
    Wildcard,
}

/// Evaluate a simplified SQL/JSON path expression against JSON text, returning
/// the JSON text of each matched item. Supports `$` (root), `.key`, `["key"]`,
/// `[n]` (array index), `.*` and `[*]` (wildcards). Path strings are the form
/// accepted by `jsonb_path_query`/`jsonb_path_exists`.
fn jsonpath_query(source: &Value, path: &str) -> Result<Vec<String>, String> {
    let Some(json) = source.to_text() else {
        return Ok(Vec::new());
    };
    let steps = parse_jsonpath(path)?;
    // Current set of matched JSON fragments; start at the root document.
    let mut current = vec![json.trim().to_string()];
    for step in &steps {
        let mut next = Vec::new();
        for frag in &current {
            match step {
                JsonPathStep::Key(k) => {
                    if let Some(v) = json_lookup(frag, &Value::Text(k.clone()))? {
                        next.push(v.trim().to_string());
                    }
                }
                JsonPathStep::Index(i) => {
                    if let Some(v) = json_lookup(frag, &Value::Int(*i as i64))? {
                        next.push(v.trim().to_string());
                    }
                }
                JsonPathStep::Wildcard => {
                    next.extend(json_all_members(frag)?);
                }
            }
        }
        current = next;
    }
    Ok(current)
}

/// Parse a SQL/JSON path string into steps. Requires a leading `$`.
fn parse_jsonpath(path: &str) -> Result<Vec<JsonPathStep>, String> {
    let path = path.trim();
    let bytes = path.as_bytes();
    if bytes.first() != Some(&b'$') {
        return Err(format!("invalid jsonpath (must start with $): {path}"));
    }
    let mut steps = Vec::new();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'.' => {
                i += 1;
                if bytes.get(i) == Some(&b'*') {
                    steps.push(JsonPathStep::Wildcard);
                    i += 1;
                    continue;
                }
                // Read a member name (bare or double-quoted).
                if bytes.get(i) == Some(&b'"') {
                    let start = i + 1;
                    let mut j = start;
                    while j < bytes.len() && bytes[j] != b'"' {
                        j += 1;
                    }
                    steps.push(JsonPathStep::Key(path[start..j].to_string()));
                    i = j + 1;
                } else {
                    let start = i;
                    while i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                        i += 1;
                    }
                    steps.push(JsonPathStep::Key(path[start..i].to_string()));
                }
            }
            b'[' => {
                i += 1;
                if bytes.get(i) == Some(&b'*') {
                    steps.push(JsonPathStep::Wildcard);
                    i += 1;
                } else if bytes.get(i) == Some(&b'"') {
                    let start = i + 1;
                    let mut j = start;
                    while j < bytes.len() && bytes[j] != b'"' {
                        j += 1;
                    }
                    steps.push(JsonPathStep::Key(path[start..j].to_string()));
                    i = j + 1;
                } else {
                    let start = i;
                    while i < bytes.len() && bytes[i] != b']' {
                        i += 1;
                    }
                    let idx: usize = path[start..i]
                        .trim()
                        .parse()
                        .map_err(|_| format!("invalid array index in jsonpath: {path}"))?;
                    steps.push(JsonPathStep::Index(idx));
                }
                if bytes.get(i) != Some(&b']') {
                    return Err(format!("unterminated [ in jsonpath: {path}"));
                }
                i += 1;
            }
            b' ' | b'\t' => i += 1,
            other => {
                return Err(format!(
                    "unsupported jsonpath token '{}' in: {path}",
                    other as char
                ))
            }
        }
    }
    Ok(steps)
}

/// All immediate members (object values or array elements) of a JSON fragment.
fn json_all_members(frag: &str) -> Result<Vec<String>, String> {
    let frag = frag.trim();
    let bytes = frag.as_bytes();
    let mut out = Vec::new();
    if bytes.first() == Some(&b'[') {
        let mut idx = 0;
        while let Some(v) = json_array_lookup(frag, idx)? {
            out.push(v.trim().to_string());
            idx += 1;
        }
    } else if bytes.first() == Some(&b'{') {
        // Walk object values via the existing key-scan machinery.
        let mut pos = 1;
        loop {
            pos = skip_json_ws(bytes, pos);
            if pos >= bytes.len() - 1 {
                break;
            }
            if bytes[pos] != b'"' {
                return Err("invalid json object".into());
            }
            let key_end = json_string_end(bytes, pos)?;
            pos = skip_json_ws(bytes, key_end + 1);
            if bytes.get(pos) != Some(&b':') {
                return Err("invalid json object".into());
            }
            pos = skip_json_ws(bytes, pos + 1);
            let value_end = json_value_end(bytes, pos)?;
            out.push(frag[pos..value_end].trim().to_string());
            pos = skip_json_ws(bytes, value_end);
            match bytes.get(pos) {
                Some(b',') => pos += 1,
                _ => break,
            }
        }
    }
    Ok(out)
}

fn json_typeof_text(source: &Value) -> Result<Value, String> {
    let Some(json) = source.to_text() else {
        return Ok(Value::Null);
    };
    let json = json.trim();
    if json.is_empty() {
        return Err("invalid json value".into());
    }
    let kind = match json.as_bytes()[0] {
        b'{' => "object",
        b'[' => "array",
        b'"' => "string",
        b't' | b'f' if json == "true" || json == "false" => "boolean",
        b'n' if json == "null" => "null",
        b'-' | b'0'..=b'9' => "number",
        _ => return Err("invalid json value".into()),
    };
    Ok(Value::Text(kind.into()))
}

fn json_array_length_text(source: &Value) -> Result<Value, String> {
    let Some(json) = source.to_text() else {
        return Ok(Value::Null);
    };
    let json = json.trim();
    let bytes = json.as_bytes();
    if bytes.first() != Some(&b'[') || bytes.last() != Some(&b']') {
        return Err("cannot get array length of a non-array".into());
    }
    let mut pos = 1;
    pos = skip_json_ws(bytes, pos);
    if bytes.get(pos) == Some(&b']') {
        return Ok(Value::Int(0));
    }
    let mut count = 0;
    loop {
        let value_end = json_value_end(bytes, pos)?;
        count += 1;
        pos = value_end;
        pos = skip_json_ws(bytes, pos);
        match bytes.get(pos) {
            Some(b',') => {
                pos += 1;
                pos = skip_json_ws(bytes, pos);
            }
            Some(b']') => return Ok(Value::Int(count)),
            _ => return Err("invalid json array".into()),
        }
    }
}

fn json_lookup<'a>(json: &'a str, key: &Value) -> Result<Option<&'a str>, String> {
    let json = json.trim();
    if json.starts_with('{') {
        let Some(key) = key.to_text() else {
            return Ok(None);
        };
        return json_object_lookup(json, key.trim());
    }
    if json.starts_with('[') {
        let index = match key {
            Value::Int(i) if *i >= 0 => *i as usize,
            Value::Text(s) => match s.trim().parse::<usize>() {
                Ok(i) => i,
                Err(_) => return Ok(None),
            },
            _ => return Ok(None),
        };
        return json_array_lookup(json, index);
    }
    Ok(None)
}

fn json_object_lookup<'a>(json: &'a str, wanted: &str) -> Result<Option<&'a str>, String> {
    let bytes = json.as_bytes();
    if bytes.last() != Some(&b'}') {
        return Err("invalid json object".into());
    }
    let mut pos = 1;
    loop {
        pos = skip_json_ws(bytes, pos);
        if pos >= bytes.len() - 1 {
            return Ok(None);
        }
        if bytes[pos] != b'"' {
            return Err("invalid json object key".into());
        }
        let key_end = json_string_end(bytes, pos)?;
        let key = json_unescape(&json[pos + 1..key_end])?;
        pos = skip_json_ws(bytes, key_end + 1);
        if bytes.get(pos) != Some(&b':') {
            return Err("invalid json object".into());
        }
        pos = skip_json_ws(bytes, pos + 1);
        let value_end = json_value_end(bytes, pos)?;
        if key == wanted {
            return Ok(Some(&json[pos..value_end]));
        }
        pos = skip_json_ws(bytes, value_end);
        match bytes.get(pos) {
            Some(b',') => pos += 1,
            Some(b'}') => return Ok(None),
            _ => return Err("invalid json object".into()),
        }
    }
}

fn json_array_lookup(json: &str, wanted: usize) -> Result<Option<&str>, String> {
    let bytes = json.as_bytes();
    if bytes.last() != Some(&b']') {
        return Err("invalid json array".into());
    }
    let mut pos = 1;
    let mut index = 0;
    loop {
        pos = skip_json_ws(bytes, pos);
        if pos >= bytes.len() - 1 {
            return Ok(None);
        }
        let value_end = json_value_end(bytes, pos)?;
        if index == wanted {
            return Ok(Some(&json[pos..value_end]));
        }
        index += 1;
        pos = skip_json_ws(bytes, value_end);
        match bytes.get(pos) {
            Some(b',') => pos += 1,
            Some(b']') => return Ok(None),
            _ => return Err("invalid json array".into()),
        }
    }
}

fn json_to_text(raw: &str) -> Result<Value, String> {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("null") {
        return Ok(Value::Null);
    }
    if raw.starts_with('"') && raw.ends_with('"') {
        return Ok(Value::Text(json_unescape(&raw[1..raw.len() - 1])?));
    }
    Ok(Value::Text(raw.to_string()))
}

fn json_value_end(bytes: &[u8], start: usize) -> Result<usize, String> {
    match bytes.get(start) {
        Some(b'"') => json_string_end(bytes, start).map(|end| end + 1),
        Some(b'{') => json_container_end(bytes, start, b'{', b'}'),
        Some(b'[') => json_container_end(bytes, start, b'[', b']'),
        Some(_) => {
            let mut pos = start;
            while pos < bytes.len() && !matches!(bytes[pos], b',' | b'}' | b']') {
                pos += 1;
            }
            Ok(pos)
        }
        None => Err("invalid json value".into()),
    }
}

fn json_container_end(bytes: &[u8], start: usize, open: u8, close: u8) -> Result<usize, String> {
    let mut depth = 0;
    let mut pos = start;
    while pos < bytes.len() {
        match bytes[pos] {
            b'"' => pos = json_string_end(bytes, pos)? + 1,
            c if c == open => {
                depth += 1;
                pos += 1;
            }
            c if c == close => {
                depth -= 1;
                pos += 1;
                if depth == 0 {
                    return Ok(pos);
                }
            }
            _ => pos += 1,
        }
    }
    Err("invalid json container".into())
}

fn json_string_end(bytes: &[u8], start: usize) -> Result<usize, String> {
    let mut pos = start + 1;
    while pos < bytes.len() {
        match bytes[pos] {
            b'\\' => pos += 2,
            b'"' => return Ok(pos),
            _ => pos += 1,
        }
    }
    Err("unterminated json string".into())
}

fn skip_json_ws(bytes: &[u8], mut pos: usize) -> usize {
    while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
        pos += 1;
    }
    pos
}

fn json_unescape(input: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('b') => out.push('\u{0008}'),
            Some('f') => out.push('\u{000c}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let mut hex = String::new();
                for _ in 0..4 {
                    hex.push(chars.next().ok_or("invalid json unicode escape")?);
                }
                let code =
                    u16::from_str_radix(&hex, 16).map_err(|_| "invalid json unicode escape")?;
                let ch = char::from_u32(code as u32).ok_or("invalid json unicode escape")?;
                out.push(ch);
            }
            Some(other) => return Err(format!("invalid json escape \\{other}")),
            None => return Err("unterminated json escape".into()),
        }
    }
    Ok(out)
}

fn arithmetic(op: BinaryOp, l: Value, r: Value) -> Result<Value, String> {
    use BinaryOp::*;
    if matches!(op, Add | Sub) {
        if let Some(value) = date_arithmetic(op, &l, &r) {
            return Ok(value);
        }
    }
    // If either is a float, compute in float.
    let both_int = matches!(l, Value::Int(_)) && matches!(r, Value::Int(_));
    if both_int {
        let (Value::Int(a), Value::Int(b)) = (&l, &r) else {
            unreachable!()
        };
        let (a, b) = (*a, *b);
        return match op {
            Add => a
                .checked_add(b)
                .map(Value::Int)
                .ok_or_else(|| "integer out of range".into()),
            Sub => a
                .checked_sub(b)
                .map(Value::Int)
                .ok_or_else(|| "integer out of range".into()),
            Mul => a
                .checked_mul(b)
                .map(Value::Int)
                .ok_or_else(|| "integer out of range".into()),
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

fn date_arithmetic(op: BinaryOp, left: &Value, right: &Value) -> Option<Value> {
    match (op, left, right) {
        (BinaryOp::Add, Value::Text(date), Value::Int(days))
        | (BinaryOp::Add, Value::Int(days), Value::Text(date)) => add_days_to_date(date, *days),
        (BinaryOp::Sub, Value::Text(date), Value::Int(days)) => add_days_to_date(date, -*days),
        _ => None,
    }
    .map(Value::Text)
}

fn add_days_to_date(value: &str, days: i64) -> Option<String> {
    let p = parse_iso_datetime(value)?;
    let day_number = days_from_civil(p.year, p.month, p.day).checked_add(days)?;
    let (year, month, day) = civil_from_days(day_number);
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

/// A parsed interval, broken into the three independent fields PostgreSQL keeps
/// (months, days, microseconds-as-seconds). Years fold into months, weeks into
/// days; time components accumulate into `seconds` (fractional allowed).
#[derive(Default, Clone, Copy)]
pub(crate) struct Interval {
    months: i64,
    days: i64,
    seconds: f64,
}

/// Parse an interval literal into canonical text, e.g.
/// `1 year 2 months 3 days` → `1 year 2 mons 3 days`,
/// `2 hours 30 minutes` → `02:30:00`, `1-2` (with `\u{1}year to month`
/// qualifier) → `1 year 2 mons`. Accepts the common unit-keyword form, an
/// `H:M:S` time component, and the `Y-M` packed form.
fn normalize_interval(input: &str) -> Result<String, String> {
    // A trailing field qualifier may have been folded in by the parser using a
    // U+0001 separator (e.g. "1-2\u{1}year to month").
    let (body, qualifier) = match input.split_once('\u{1}') {
        Some((b, q)) => (b.trim(), Some(q)),
        None => (input.trim(), None),
    };

    let mut iv = Interval::default();

    // `Y-M` packed form (optionally with a leading sign): "1-2".
    let packed = body
        .strip_prefix('-')
        .map(|rest| (true, rest))
        .unwrap_or((false, body));
    if qualifier == Some("year to month")
        || (packed.1.split_once('-').is_some_and(|(a, b)| {
            !a.is_empty()
                && a.bytes().all(|c| c.is_ascii_digit())
                && b.bytes().all(|c| c.is_ascii_digit())
        }) && !body.contains(' '))
    {
        if let Some((y, m)) = packed.1.split_once('-') {
            let sign = if packed.0 { -1 } else { 1 };
            let years: i64 = y.parse().map_err(|_| "invalid interval".to_string())?;
            let months: i64 = m.parse().map_err(|_| "invalid interval".to_string())?;
            iv.months = sign * (years * 12 + months);
            return Ok(format_interval(&iv));
        }
    }

    // Token stream: alternating <number> <unit>, plus optional H:M:S groups.
    let mut tokens = body.split_whitespace().peekable();
    while let Some(tok) = tokens.next() {
        // A bare time component `HH:MM:SS[.ffff]`.
        if tok.contains(':') {
            add_time_component(&mut iv, tok)?;
            continue;
        }
        // Otherwise a number followed by a unit word.
        let value: f64 = tok
            .parse()
            .map_err(|_| format!("invalid interval value: {tok}"))?;
        let unit = tokens
            .next()
            .ok_or_else(|| format!("interval value {tok} has no unit"))?;
        apply_interval_unit(&mut iv, value, unit)?;
    }

    Ok(format_interval(&iv))
}

fn add_time_component(iv: &mut Interval, tok: &str) -> Result<(), String> {
    let (neg, tok) = match tok.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, tok),
    };
    let mut parts = tok.split(':');
    let h: f64 = parts.next().unwrap_or("0").parse().map_err(|_| "invalid interval time".to_string())?;
    let m: f64 = parts.next().unwrap_or("0").parse().map_err(|_| "invalid interval time".to_string())?;
    let s: f64 = parts.next().unwrap_or("0").parse().map_err(|_| "invalid interval time".to_string())?;
    let secs = h * 3600.0 + m * 60.0 + s;
    iv.seconds += if neg { -secs } else { secs };
    Ok(())
}

fn apply_interval_unit(iv: &mut Interval, value: f64, unit: &str) -> Result<(), String> {
    let u = unit.to_ascii_lowercase();
    let u = u.trim_end_matches('s'); // accept singular/plural
    match u {
        "year" | "yr" | "y" => iv.months += (value * 12.0) as i64,
        "mon" | "month" | "mo" => iv.months += value as i64,
        "week" | "w" => iv.days += (value * 7.0) as i64,
        "day" | "d" => iv.days += value as i64,
        "hour" | "hr" | "h" => iv.seconds += value * 3600.0,
        "minute" | "min" | "m" => iv.seconds += value * 60.0,
        "second" | "sec" => iv.seconds += value,
        "millisecond" | "msec" | "ms" => iv.seconds += value / 1000.0,
        "microsecond" | "usec" | "us" => iv.seconds += value / 1_000_000.0,
        other => return Err(format!("unknown interval unit: {other}")),
    }
    Ok(())
}

/// Render an interval in PostgreSQL's canonical text form: the month/day parts
/// as `N year(s) N mon(s) N day(s)` and the time part as `[-]HH:MM:SS[.ffffff]`.
fn format_interval(iv: &Interval) -> String {
    let mut parts: Vec<String> = Vec::new();
    let years = iv.months / 12;
    let mons = iv.months % 12;
    if years != 0 {
        parts.push(format!("{years} year{}", if years.abs() == 1 { "" } else { "s" }));
    }
    if mons != 0 {
        parts.push(format!("{mons} mon{}", if mons.abs() == 1 { "" } else { "s" }));
    }
    if iv.days != 0 {
        parts.push(format!("{} day{}", iv.days, if iv.days.abs() == 1 { "" } else { "s" }));
    }
    if iv.seconds != 0.0 || parts.is_empty() {
        let neg = iv.seconds < 0.0;
        let total = iv.seconds.abs();
        let whole = total.trunc() as i64;
        let h = whole / 3600;
        let m = (whole % 3600) / 60;
        let s = whole % 60;
        let frac = total.fract();
        let sign = if neg { "-" } else { "" };
        if frac.abs() > 1e-9 {
            // Up to 6 fractional digits, trimmed of trailing zeros.
            let micros = (frac * 1_000_000.0).round() as i64;
            let mut f = format!("{micros:06}");
            while f.ends_with('0') {
                f.pop();
            }
            parts.push(format!("{sign}{h:02}:{m:02}:{s:02}.{f}"));
        } else {
            parts.push(format!("{sign}{h:02}:{m:02}:{s:02}"));
        }
    }
    parts.join(" ")
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719468;
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = days - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    (year, month, day)
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
                Err(_) => l
                    .to_text()
                    .and_then(|ls| ls.as_str().partial_cmp(s.as_str())),
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
                Err(_) => r
                    .to_text()
                    .and_then(|rs| s.as_str().partial_cmp(rs.as_str())),
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

fn eval_quantified_compare(
    left: &Expr,
    op: BinaryOp,
    quantifier: Quantifier,
    list: &[Expr],
    col_names: &[String],
    row: &[Value],
) -> Result<Value, String> {
    let lhs = eval_expr(left, col_names, row)?;
    let mut saw_null = lhs.is_null();
    match quantifier {
        Quantifier::Any | Quantifier::Some => {
            for item in list {
                let rhs = eval_expr(item, col_names, row)?;
                let cmp = eval_binary(op, lhs.clone(), rhs)?;
                match cmp {
                    Value::Bool(true) => return Ok(Value::Bool(true)),
                    Value::Null => saw_null = true,
                    _ => {}
                }
            }
            Ok(if saw_null {
                Value::Null
            } else {
                Value::Bool(false)
            })
        }
        Quantifier::All => {
            for item in list {
                let rhs = eval_expr(item, col_names, row)?;
                let cmp = eval_binary(op, lhs.clone(), rhs)?;
                match cmp {
                    Value::Bool(false) => return Ok(Value::Bool(false)),
                    Value::Null => saw_null = true,
                    _ => {}
                }
            }
            Ok(if saw_null {
                Value::Null
            } else {
                Value::Bool(true)
            })
        }
    }
}

/// Minimal POSIX-style regex matcher supporting `^`, `$`, `.`, and `*`
/// (enough for the catalog patterns PostgreSQL clients use, e.g. `^pg_toast`).
fn regex_match(pattern: &str, text: &str, case_insensitive: bool) -> bool {
    let fold = |s: &str| {
        if case_insensitive {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };
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
                Ok(Value::Int(
                    v.to_text().unwrap_or_default().chars().count() as i64
                ))
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
                        let take = if want_greatest {
                            ord == Ordering::Greater
                        } else {
                            ord == Ordering::Less
                        };
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
                Some(Value::Int(len)) => text
                    .iter()
                    .skip(start_idx)
                    .take((*len).max(0) as usize)
                    .collect(),
                _ => text.iter().skip(start_idx).collect(),
            };
            Ok(Value::Text(result))
        }
        "array_length" => {
            let array = arg(&vals, 0)?;
            let dim = arg(&vals, 1)?;
            if array.is_null() || dim.is_null() {
                return Ok(Value::Null);
            }
            if !matches!(dim, Value::Int(1)) {
                return Ok(Value::Null);
            }
            let text = array.to_text().unwrap_or_default();
            let values = parse_array_text(&text)
                .ok_or_else(|| "array_length() requires an array".to_string())?;
            if values.is_empty() {
                Ok(Value::Null)
            } else {
                Ok(Value::Int(values.len() as i64))
            }
        }
        "cardinality" => {
            let array = arg(&vals, 0)?;
            if array.is_null() {
                return Ok(Value::Null);
            }
            let text = array.to_text().unwrap_or_default();
            let values = parse_array_text(&text)
                .ok_or_else(|| "cardinality() requires an array".to_string())?;
            Ok(Value::Int(values.len() as i64))
        }
        "array_position" => {
            let array = arg(&vals, 0)?;
            let needle = arg(&vals, 1)?;
            if array.is_null() {
                return Ok(Value::Null);
            }
            let text = array.to_text().unwrap_or_default();
            let values = parse_array_text(&text)
                .ok_or_else(|| "array_position() requires an array".to_string())?;
            let needle = needle.to_text();
            for (idx, value) in values.iter().enumerate() {
                if value.as_deref() == needle.as_deref() {
                    return Ok(Value::Int(idx as i64 + 1));
                }
            }
            Ok(Value::Null)
        }
        "array_append" => {
            let array = arg(&vals, 0)?;
            let value = arg(&vals, 1)?;
            if array.is_null() {
                return Ok(Value::Null);
            }
            let text = array.to_text().unwrap_or_default();
            let mut values = parse_array_text(&text)
                .ok_or_else(|| "array_append() requires an array".to_string())?;
            values.push(value.to_text());
            Ok(Value::Text(array_text_from_elements(&values)))
        }
        "array_prepend" => {
            let value = arg(&vals, 0)?;
            let array = arg(&vals, 1)?;
            if array.is_null() {
                return Ok(Value::Null);
            }
            let text = array.to_text().unwrap_or_default();
            let mut values = parse_array_text(&text)
                .ok_or_else(|| "array_prepend() requires an array".to_string())?;
            values.insert(0, value.to_text());
            Ok(Value::Text(array_text_from_elements(&values)))
        }
        "array_cat" => {
            let left = arg(&vals, 0)?;
            let right = arg(&vals, 1)?;
            if left.is_null() || right.is_null() {
                return Ok(Value::Null);
            }
            let left_text = left.to_text().unwrap_or_default();
            let right_text = right.to_text().unwrap_or_default();
            let mut values = parse_array_text(&left_text)
                .ok_or_else(|| "array_cat() requires array arguments".to_string())?;
            let mut right_values = parse_array_text(&right_text)
                .ok_or_else(|| "array_cat() requires array arguments".to_string())?;
            values.append(&mut right_values);
            Ok(Value::Text(array_text_from_elements(&values)))
        }
        "json_typeof" | "jsonb_typeof" => json_typeof_text(arg(&vals, 0)?),
        "json_array_length" | "jsonb_array_length" => json_array_length_text(arg(&vals, 0)?),
        "json_extract_path_text" | "jsonb_extract_path_text" => {
            let source = arg(&vals, 0)?;
            json_path_text(source, &vals[1..])
        }
        "jsonb_path_query" | "json_path_query" => {
            let source = arg(&vals, 0)?;
            let path = arg(&vals, 1)?.to_text().unwrap_or_default();
            let matches = jsonpath_query(source, &path)?;
            // `jsonb_path_query` is set-returning in PostgreSQL; in scalar
            // context we return the first match (or NULL if none).
            Ok(matches.into_iter().next().map(Value::Text).unwrap_or(Value::Null))
        }
        "jsonb_path_exists" | "json_path_exists" => {
            let source = arg(&vals, 0)?;
            if source.is_null() {
                return Ok(Value::Null);
            }
            let path = arg(&vals, 1)?.to_text().unwrap_or_default();
            Ok(Value::Bool(!jsonpath_query(source, &path)?.is_empty()))
        }
        "to_tsvector" => {
            let source = arg(&vals, 0)?;
            if source.is_null() {
                Ok(Value::Null)
            } else {
                Ok(Value::Text(to_tsvector_text(
                    &source.to_text().unwrap_or_default(),
                )))
            }
        }
        "plainto_tsquery" => {
            let source = arg(&vals, 0)?;
            if source.is_null() {
                Ok(Value::Null)
            } else {
                Ok(Value::Text(plainto_tsquery_text(
                    &source.to_text().unwrap_or_default(),
                )))
            }
        }
        "to_tsquery" => {
            let source = arg(&vals, 0)?;
            if source.is_null() {
                Ok(Value::Null)
            } else {
                Ok(Value::Text(to_tsquery_text(
                    &source.to_text().unwrap_or_default(),
                )))
            }
        }
        "ts_rank" => {
            let vector = arg(&vals, 0)?;
            let query = arg(&vals, 1)?;
            if vector.is_null() || query.is_null() {
                Ok(Value::Null)
            } else {
                Ok(Value::Float(ts_rank_text(vector, query)))
            }
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
        "date_part" | "extract" => {
            let field = arg(&vals, 0)?
                .to_text()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let src = arg(&vals, 1)?;
            if src.is_null() {
                return Ok(Value::Null);
            }
            let p = parse_iso_datetime(&src.to_text().unwrap_or_default())
                .ok_or_else(|| format!("invalid timestamp for date_part: {src}"))?;
            let v = match field.as_str() {
                "year" => p.year,
                "month" => p.month,
                "day" => p.day,
                "hour" => p.hour,
                "minute" => p.minute,
                "second" => p.second,
                other => return Err(format!("unsupported date_part field: {other}")),
            };
            Ok(Value::Float(v as f64))
        }
        "date_trunc" => {
            let field = arg(&vals, 0)?
                .to_text()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let src = arg(&vals, 1)?;
            if src.is_null() {
                return Ok(Value::Null);
            }
            let p = parse_iso_datetime(&src.to_text().unwrap_or_default())
                .ok_or_else(|| format!("invalid timestamp for date_trunc: {src}"))?;
            let truncated = match field.as_str() {
                "year" => format!("{:04}-01-01 00:00:00", p.year),
                "month" => format!("{:04}-{:02}-01 00:00:00", p.year, p.month),
                "day" => format!("{:04}-{:02}-{:02} 00:00:00", p.year, p.month, p.day),
                "hour" => format!(
                    "{:04}-{:02}-{:02} {:02}:00:00",
                    p.year, p.month, p.day, p.hour
                ),
                "minute" => {
                    format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}:00",
                        p.year, p.month, p.day, p.hour, p.minute
                    )
                }
                "second" => format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    p.year, p.month, p.day, p.hour, p.minute, p.second
                ),
                other => return Err(format!("unsupported date_trunc field: {other}")),
            };
            Ok(Value::Text(truncated))
        }
        // --- Math functions ---
        "ceil" | "ceiling" | "floor" | "trunc" => {
            let v = arg(&vals, 0)?;
            if v.is_null() {
                return Ok(Value::Null);
            }
            let x = to_f64(v)?;
            let digits = match (lname.as_str(), vals.get(1)) {
                ("trunc", Some(Value::Int(d))) => *d,
                _ => 0,
            };
            let factor = 10f64.powi(digits as i32);
            let r = match lname.as_str() {
                "ceil" | "ceiling" => x.ceil(),
                "floor" => x.floor(),
                _ => (x * factor).trunc() / factor,
            };
            // Integer input with no fractional digits stays integral.
            if digits <= 0 && matches!(v, Value::Int(_)) {
                Ok(Value::Int(r as i64))
            } else {
                Ok(Value::Float(r))
            }
        }
        "sign" => {
            let v = arg(&vals, 0)?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(i) => Ok(Value::Int(i.signum())),
                _ => Ok(Value::Float(to_f64(v)?.signum())),
            }
        }
        "sqrt" => math_unary(&vals, |x| x.sqrt()),
        "cbrt" => math_unary(&vals, |x| x.cbrt()),
        "exp" => math_unary(&vals, |x| x.exp()),
        "ln" => math_unary(&vals, |x| x.ln()),
        "log10" => math_unary(&vals, |x| x.log10()),
        // PostgreSQL `log(x)` is base-10; `log(b, x)` is log base b.
        "log" => {
            if vals.len() >= 2 {
                math_binary(&vals, |b, x| x.log(b))
            } else {
                math_unary(&vals, |x| x.log10())
            }
        }
        "power" | "pow" => math_binary(&vals, |b, e| b.powf(e)),
        "pi" => Ok(Value::Float(std::f64::consts::PI)),
        "radians" => math_unary(&vals, |x| x.to_radians()),
        "degrees" => math_unary(&vals, |x| x.to_degrees()),
        "sin" => math_unary(&vals, |x| x.sin()),
        "cos" => math_unary(&vals, |x| x.cos()),
        "tan" => math_unary(&vals, |x| x.tan()),
        "cot" => math_unary(&vals, |x| 1.0 / x.tan()),
        "asin" => math_unary(&vals, |x| x.asin()),
        "acos" => math_unary(&vals, |x| x.acos()),
        "atan" => math_unary(&vals, |x| x.atan()),
        "atan2" => math_binary(&vals, |y, x| y.atan2(x)),
        "mod" => {
            let a = arg(&vals, 0)?;
            let b = arg(&vals, 1)?;
            if a.is_null() || b.is_null() {
                return Ok(Value::Null);
            }
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => {
                    if *y == 0 {
                        return Err("division by zero".into());
                    }
                    Ok(Value::Int(x % y))
                }
                _ => {
                    let y = to_f64(b)?;
                    if y == 0.0 {
                        return Err("division by zero".into());
                    }
                    Ok(Value::Float(to_f64(a)? % y))
                }
            }
        }
        "div" => {
            let a = arg(&vals, 0)?;
            let b = arg(&vals, 1)?;
            if a.is_null() || b.is_null() {
                return Ok(Value::Null);
            }
            let y = to_f64(b)?;
            if y == 0.0 {
                return Err("division by zero".into());
            }
            Ok(Value::Int((to_f64(a)? / y).trunc() as i64))
        }
        "gcd" | "lcm" => {
            let a = arg(&vals, 0)?;
            let b = arg(&vals, 1)?;
            if a.is_null() || b.is_null() {
                return Ok(Value::Null);
            }
            let (x, y) = match (a, b) {
                (Value::Int(x), Value::Int(y)) => (x.unsigned_abs(), y.unsigned_abs()),
                _ => return Err(format!("{lname}() requires integer arguments")),
            };
            let g = gcd_u64(x, y);
            if lname == "gcd" {
                Ok(Value::Int(g as i64))
            } else if g == 0 {
                Ok(Value::Int(0))
            } else {
                Ok(Value::Int((x / g * y) as i64))
            }
        }
        // --- String functions ---
        "lpad" | "rpad" => {
            let s = arg(&vals, 0)?;
            let len = arg(&vals, 1)?;
            if s.is_null() || len.is_null() {
                return Ok(Value::Null);
            }
            let target = match len {
                Value::Int(i) => (*i).max(0) as usize,
                other => to_f64(other)?.max(0.0) as usize,
            };
            let fill = match vals.get(2) {
                Some(v) if !v.is_null() => v.to_text().unwrap_or_default(),
                _ => " ".to_string(),
            };
            let chars: Vec<char> = s.to_text().unwrap_or_default().chars().collect();
            if chars.len() >= target {
                Ok(Value::Text(chars.into_iter().take(target).collect()))
            } else if fill.is_empty() {
                Ok(Value::Text(chars.into_iter().collect()))
            } else {
                let pad_needed = target - chars.len();
                let fill_chars: Vec<char> = fill.chars().collect();
                let pad: String = (0..pad_needed)
                    .map(|i| fill_chars[i % fill_chars.len()])
                    .collect();
                let body: String = chars.into_iter().collect();
                if lname == "lpad" {
                    Ok(Value::Text(format!("{pad}{body}")))
                } else {
                    Ok(Value::Text(format!("{body}{pad}")))
                }
            }
        }
        "left" | "right" => {
            let s = arg(&vals, 0)?;
            let n = arg(&vals, 1)?;
            if s.is_null() || n.is_null() {
                return Ok(Value::Null);
            }
            let chars: Vec<char> = s.to_text().unwrap_or_default().chars().collect();
            let len = chars.len() as i64;
            let n = match n {
                Value::Int(i) => *i,
                other => to_f64(other)? as i64,
            };
            // Negative n means "all but the last/first |n| characters".
            let result: String = if lname == "left" {
                let take = if n < 0 { (len + n).max(0) } else { n.min(len) };
                chars.into_iter().take(take as usize).collect()
            } else {
                let take = if n < 0 { (len + n).max(0) } else { n.min(len) };
                chars
                    .into_iter()
                    .skip((len - take) as usize)
                    .collect()
            };
            Ok(Value::Text(result))
        }
        "repeat" => {
            let s = arg(&vals, 0)?;
            let n = arg(&vals, 1)?;
            if s.is_null() || n.is_null() {
                return Ok(Value::Null);
            }
            let count = match n {
                Value::Int(i) => (*i).max(0) as usize,
                other => to_f64(other)?.max(0.0) as usize,
            };
            Ok(Value::Text(s.to_text().unwrap_or_default().repeat(count)))
        }
        "reverse" => str_fn(&vals, |s| s.chars().rev().collect()),
        "initcap" => str_fn(&vals, |s| {
            let mut out = String::with_capacity(s.len());
            let mut start_of_word = true;
            for ch in s.chars() {
                if ch.is_alphanumeric() {
                    if start_of_word {
                        out.extend(ch.to_uppercase());
                    } else {
                        out.extend(ch.to_lowercase());
                    }
                    start_of_word = false;
                } else {
                    out.push(ch);
                    start_of_word = true;
                }
            }
            out
        }),
        "ascii" => {
            let v = arg(&vals, 0)?;
            if v.is_null() {
                return Ok(Value::Null);
            }
            match v.to_text().unwrap_or_default().chars().next() {
                Some(c) => Ok(Value::Int(c as i64)),
                None => Ok(Value::Int(0)),
            }
        }
        "chr" => {
            let v = arg(&vals, 0)?;
            if v.is_null() {
                return Ok(Value::Null);
            }
            let code = match v {
                Value::Int(i) => *i,
                other => to_f64(other)? as i64,
            };
            let c = u32::try_from(code)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| format!("chr(): invalid character code {code}"))?;
            Ok(Value::Text(c.to_string()))
        }
        "strpos" => {
            let s = arg(&vals, 0)?;
            let sub = arg(&vals, 1)?;
            if s.is_null() || sub.is_null() {
                return Ok(Value::Null);
            }
            let haystack = s.to_text().unwrap_or_default();
            let needle = sub.to_text().unwrap_or_default();
            // 1-based character position, 0 when not found.
            match haystack.find(&needle) {
                Some(byte_idx) => Ok(Value::Int(haystack[..byte_idx].chars().count() as i64 + 1)),
                None => Ok(Value::Int(0)),
            }
        }
        "starts_with" => {
            let s = arg(&vals, 0)?;
            let prefix = arg(&vals, 1)?;
            if s.is_null() || prefix.is_null() {
                return Ok(Value::Null);
            }
            Ok(Value::Bool(
                s.to_text()
                    .unwrap_or_default()
                    .starts_with(&prefix.to_text().unwrap_or_default()),
            ))
        }
        "split_part" => {
            let s = arg(&vals, 0)?;
            let delim = arg(&vals, 1)?;
            let field = arg(&vals, 2)?;
            if s.is_null() || delim.is_null() || field.is_null() {
                return Ok(Value::Null);
            }
            let n = match field {
                Value::Int(i) => *i,
                other => to_f64(other)? as i64,
            };
            let text = s.to_text().unwrap_or_default();
            let delim = delim.to_text().unwrap_or_default();
            let parts: Vec<&str> = if delim.is_empty() {
                vec![text.as_str()]
            } else {
                text.split(delim.as_str()).collect()
            };
            // 1-based; negative counts from the end (PostgreSQL 14+).
            let idx = if n > 0 {
                (n - 1) as usize
            } else if n < 0 {
                let from_end = (-n) as usize;
                if from_end > parts.len() {
                    return Ok(Value::Text(String::new()));
                }
                parts.len() - from_end
            } else {
                return Err("field position must not be zero".into());
            };
            Ok(Value::Text(
                parts.get(idx).copied().unwrap_or("").to_string(),
            ))
        }
        "to_hex" => {
            let v = arg(&vals, 0)?;
            if v.is_null() {
                return Ok(Value::Null);
            }
            let n = match v {
                Value::Int(i) => *i,
                other => to_f64(other)? as i64,
            };
            Ok(Value::Text(format!("{:x}", n as u64)))
        }
        "concat_ws" => {
            let sep = arg(&vals, 0)?;
            if sep.is_null() {
                return Ok(Value::Null);
            }
            let sep = sep.to_text().unwrap_or_default();
            let parts: Vec<String> = vals[1..]
                .iter()
                .filter(|v| !v.is_null())
                .filter_map(|v| v.to_text())
                .collect();
            Ok(Value::Text(parts.join(&sep)))
        }
        "translate" => {
            let s = arg(&vals, 0)?;
            let from = arg(&vals, 1)?;
            let to = arg(&vals, 2)?;
            if s.is_null() || from.is_null() || to.is_null() {
                return Ok(Value::Null);
            }
            let from: Vec<char> = from.to_text().unwrap_or_default().chars().collect();
            let to: Vec<char> = to.to_text().unwrap_or_default().chars().collect();
            let out: String = s
                .to_text()
                .unwrap_or_default()
                .chars()
                .filter_map(|c| match from.iter().position(|f| *f == c) {
                    Some(i) => to.get(i).copied(),
                    None => Some(c),
                })
                .collect();
            Ok(Value::Text(out))
        }
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
        // Fall through to user-defined scalar SQL functions.
        other => match try_eval_scalar_udf(other, &vals) {
            Some(result) => result,
            None => Err(format!("function {other}() does not exist")),
        },
    }
}

fn gcd_u64(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

/// Evaluate a unary floating-point math function with NULL passthrough.
fn math_unary(vals: &[Value], f: impl Fn(f64) -> f64) -> Result<Value, String> {
    let v = arg(vals, 0)?;
    if v.is_null() {
        return Ok(Value::Null);
    }
    Ok(Value::Float(f(to_f64(v)?)))
}

/// Evaluate a binary floating-point math function with NULL passthrough.
fn math_binary(vals: &[Value], f: impl Fn(f64, f64) -> f64) -> Result<Value, String> {
    let a = arg(vals, 0)?;
    let b = arg(vals, 1)?;
    if a.is_null() || b.is_null() {
        return Ok(Value::Null);
    }
    Ok(Value::Float(f(to_f64(a)?, to_f64(b)?)))
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
    vals.get(i)
        .ok_or_else(|| "missing function argument".to_string())
}

/// Components of an ISO date or timestamp (date/time types are text-stored).
struct DateTimeParts {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
}

/// Parse an ISO date/timestamp like `2024-03-15` or `2024-03-15 10:30:00`
/// (a `T` separator and trailing fraction/timezone are tolerated).
fn parse_iso_datetime(s: &str) -> Option<DateTimeParts> {
    let s = s.trim();
    let (date, time) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut d = date.split('-');
    let year = d.next()?.parse().ok()?;
    let month = d.next().and_then(|x| x.parse().ok()).unwrap_or(1);
    let day = d.next().and_then(|x| x.parse().ok()).unwrap_or(1);

    let (mut hour, mut minute, mut second) = (0, 0, 0);
    if let Some(t) = time {
        // Drop any timezone offset or 'Z' suffix.
        let t = t.split(['+', 'Z']).next().unwrap_or(t);
        let mut parts = t.split(':');
        hour = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        minute = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        // Seconds may carry a fraction (e.g. 12.5).
        second = parts
            .next()
            .and_then(|x| x.split('.').next())
            .and_then(|x| x.parse().ok())
            .unwrap_or(0);
    }
    Some(DateTimeParts {
        year,
        month,
        day,
        hour,
        minute,
        second,
    })
}

// --- aggregates --------------------------------------------------------------

fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        // A window function (`f(...) OVER (...)`) is evaluated in the window
        // phase, not as a grouping aggregate, even if `f` is an aggregate name.
        Expr::Function { over: Some(_), .. } => false,
        Expr::Function { name, .. } if is_aggregate_name(name) => true,
        Expr::Function { args, filter, .. } => {
            args.iter().any(contains_aggregate) || filter.as_deref().is_some_and(contains_aggregate)
        }
        Expr::Row(items) | Expr::Array(items) => items.iter().any(contains_aggregate),
        Expr::Unary { expr, .. } => contains_aggregate(expr),
        Expr::Binary { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::QuantifiedCompare { left, list, .. } => {
            contains_aggregate(left) || list.iter().any(contains_aggregate)
        }
        Expr::IsNull { expr, .. } => contains_aggregate(expr),
        Expr::IsDistinctFrom { left, right, .. } => {
            contains_aggregate(left) || contains_aggregate(right)
        }
        Expr::Cast { expr, .. } => contains_aggregate(expr),
        Expr::Like { expr, pattern, .. } => contains_aggregate(expr) || contains_aggregate(pattern),
        Expr::InList { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        Expr::Between {
            expr, low, high, ..
        } => contains_aggregate(expr) || contains_aggregate(low) || contains_aggregate(high),
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            operand.as_deref().is_some_and(contains_aggregate)
                || whens
                    .iter()
                    .any(|(c, r)| contains_aggregate(c) || contains_aggregate(r))
                || else_expr.as_deref().is_some_and(contains_aggregate)
        }
        // A subquery's own aggregates don't make the outer expression an
        // aggregate; only the IN-test's left operand matters here.
        Expr::InSubquery { expr, .. } => contains_aggregate(expr),
        _ => false,
    }
}

/// Whether `expr` contains a window function (`f(...) OVER (...)`).
fn contains_window_function(expr: &Expr) -> bool {
    if let Expr::Function { over: Some(_), .. } = expr {
        return true;
    }
    let mut found = false;
    let _ = visit_child_exprs(expr, &mut |child| {
        found = found || contains_window_function(child);
        Ok(())
    });
    found
}

/// Collect each distinct window-function subexpression of `expr` (in first-seen
/// order) so its values can be precomputed once per input row.
fn collect_window_fns(expr: &Expr, out: &mut Vec<Expr>) {
    if let Expr::Function { over: Some(_), .. } = expr {
        if !out.contains(expr) {
            out.push(expr.clone());
        }
        return;
    }
    let _ = visit_child_exprs(expr, &mut |child| {
        collect_window_fns(child, out);
        Ok(())
    });
}

/// Replace each window-function node in `expr` with the precomputed literal for
/// row `row_idx`, leaving everything else intact.
fn substitute_window_fns(
    expr: &Expr,
    wfns: &[Expr],
    wvals: &[Vec<Value>],
    row_idx: usize,
) -> Expr {
    if let Expr::Function { over: Some(_), .. } = expr {
        if let Some(k) = wfns.iter().position(|w| w == expr) {
            return value_to_literal(wvals[k][row_idx].clone());
        }
    }
    let mut out = expr.clone();
    let _ = map_child_exprs(&mut out, &mut |child| {
        *child = substitute_window_fns(child, wfns, wvals, row_idx);
        Ok(())
    });
    out
}

/// Evaluate one window function across all input rows, returning its value per
/// original row index. Partitions by `PARTITION BY`, orders by `ORDER BY`, and
/// applies the SQL default frame (RANGE UNBOUNDED PRECEDING TO CURRENT ROW when
/// ordered, otherwise the whole partition).
fn compute_window_values(
    wfn: &Expr,
    col_names: &[String],
    rows: &[Vec<Value>],
) -> Result<Vec<Value>, String> {
    let Expr::Function {
        name,
        args,
        star,
        distinct,
        over: Some(spec),
        ..
    } = wfn
    else {
        return Err("not a window function".into());
    };
    let lname = name.to_ascii_lowercase();
    let n = rows.len();
    let mut result = vec![Value::Null; n];

    // Partition rows by the PARTITION BY key, preserving first-seen order.
    let mut partitions: Vec<Vec<usize>> = Vec::new();
    let mut part_keys: Vec<Vec<Value>> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let mut key = Vec::with_capacity(spec.partition_by.len());
        for e in &spec.partition_by {
            key.push(eval_expr(e, col_names, row)?);
        }
        match part_keys.iter().position(|k| *k == key) {
            Some(p) => partitions[p].push(i),
            None => {
                part_keys.push(key);
                partitions.push(vec![i]);
            }
        }
    }

    for part in &partitions {
        // Order the partition by ORDER BY (stable; defaults to input order).
        let mut ordered: Vec<usize> = part.clone();
        if !spec.order_by.is_empty() {
            // Precompute sort keys for each row in the partition.
            let mut keyed: Vec<(usize, Vec<Value>)> = Vec::with_capacity(part.len());
            for &idx in part {
                let mut k = Vec::with_capacity(spec.order_by.len());
                for ob in &spec.order_by {
                    k.push(eval_expr(&ob.expr, col_names, &rows[idx])?);
                }
                keyed.push((idx, k));
            }
            keyed.sort_by(|a, b| compare_order_keys(&a.1, &b.1, &spec.order_by));
            ordered = keyed.into_iter().map(|(i, _)| i).collect();
        }

        // Order keys (for peer detection and rank).
        let order_key = |idx: usize| -> Result<Vec<Value>, String> {
            let mut k = Vec::with_capacity(spec.order_by.len());
            for ob in &spec.order_by {
                k.push(eval_expr(&ob.expr, col_names, &rows[idx])?);
            }
            Ok(k)
        };

        let plen = ordered.len();
        for (pos, &orig) in ordered.iter().enumerate() {
            let value = match lname.as_str() {
                "row_number" => Value::Int(pos as i64 + 1),
                "rank" => {
                    // rank = position of the current row's first peer + 1, so
                    // tied rows share a rank and the next distinct key gaps.
                    let cur = order_key(orig)?;
                    let mut first_peer = pos;
                    while first_peer > 0 && order_key(ordered[first_peer - 1])? == cur {
                        first_peer -= 1;
                    }
                    Value::Int(first_peer as i64 + 1)
                }
                "dense_rank" => {
                    // Count distinct order keys up to and including current.
                    let mut distinct_keys = 0i64;
                    let mut last: Option<Vec<Value>> = None;
                    for &o in &ordered[..=pos] {
                        let k = order_key(o)?;
                        if last.as_ref() != Some(&k) {
                            distinct_keys += 1;
                            last = Some(k);
                        }
                    }
                    Value::Int(distinct_keys)
                }
                "ntile" => {
                    let buckets = args
                        .first()
                        .and_then(|a| eval_expr(a, col_names, &rows[orig]).ok())
                        .and_then(|v| match v {
                            Value::Int(i) => Some(i.max(1)),
                            _ => None,
                        })
                        .unwrap_or(1);
                    // Distribute plen rows across `buckets`, larger buckets first.
                    let base = plen as i64 / buckets;
                    let rem = plen as i64 % buckets;
                    let mut bucket = 1i64;
                    let mut consumed = 0i64;
                    let mut acc = 0i64;
                    for b in 0..buckets {
                        let size = base + if b < rem { 1 } else { 0 };
                        acc += size;
                        if (pos as i64) < acc {
                            bucket = b + 1;
                            break;
                        }
                        consumed = acc;
                    }
                    let _ = consumed;
                    Value::Int(bucket)
                }
                "lag" | "lead" => {
                    let offset = args
                        .get(1)
                        .and_then(|a| eval_expr(a, col_names, &rows[orig]).ok())
                        .and_then(|v| match v {
                            Value::Int(i) => Some(i),
                            _ => None,
                        })
                        .unwrap_or(1);
                    let target = if lname == "lag" {
                        pos as i64 - offset
                    } else {
                        pos as i64 + offset
                    };
                    if target >= 0 && (target as usize) < plen {
                        let src = ordered[target as usize];
                        match args.first() {
                            Some(a) => eval_expr(a, col_names, &rows[src])?,
                            None => Value::Null,
                        }
                    } else {
                        // Optional default argument.
                        match args.get(2) {
                            Some(a) => eval_expr(a, col_names, &rows[orig])?,
                            None => Value::Null,
                        }
                    }
                }
                "first_value" => match args.first() {
                    Some(a) => eval_expr(a, col_names, &rows[ordered[0]])?,
                    None => Value::Null,
                },
                "last_value" => {
                    let frame_end = frame_end_index(&ordered, pos, &spec.order_by, &order_key)?;
                    match args.first() {
                        Some(a) => eval_expr(a, col_names, &rows[ordered[frame_end]])?,
                        None => Value::Null,
                    }
                }
                "nth_value" => {
                    let nth = args
                        .get(1)
                        .and_then(|a| eval_expr(a, col_names, &rows[orig]).ok())
                        .and_then(|v| match v {
                            Value::Int(i) => Some(i),
                            _ => None,
                        })
                        .unwrap_or(1);
                    let frame_end = frame_end_index(&ordered, pos, &spec.order_by, &order_key)?;
                    let target = nth - 1;
                    if nth >= 1 && (target as usize) <= frame_end {
                        match args.first() {
                            Some(a) => eval_expr(a, col_names, &rows[ordered[target as usize]])?,
                            None => Value::Null,
                        }
                    } else {
                        Value::Null
                    }
                }
                _ if is_aggregate_name(&lname) => {
                    // Aggregate over the default frame: partition start .. current
                    // row's last peer (or whole partition when unordered).
                    let frame_end = frame_end_index(&ordered, pos, &spec.order_by, &order_key)?;
                    let frame_rows: Vec<Vec<Value>> = ordered[..=frame_end]
                        .iter()
                        .map(|&idx| rows[idx].clone())
                        .collect();
                    eval_aggregate(
                        &lname,
                        args,
                        *star,
                        *distinct,
                        None,
                        col_names,
                        &frame_rows,
                    )?
                }
                other => return Err(format!("window function {other}() is not supported")),
            };
            result[orig] = value;
        }
    }
    Ok(result)
}

/// Index (within the ordered partition) of the last row in the current row's
/// default frame: its last peer when ordered, else the partition end.
fn frame_end_index(
    ordered: &[usize],
    pos: usize,
    order_by: &[OrderByItem],
    order_key: &dyn Fn(usize) -> Result<Vec<Value>, String>,
) -> Result<usize, String> {
    if order_by.is_empty() {
        return Ok(ordered.len() - 1);
    }
    let cur = order_key(ordered[pos])?;
    let mut end = pos;
    while end + 1 < ordered.len() && order_key(ordered[end + 1])? == cur {
        end += 1;
    }
    Ok(end)
}

/// Compare two ORDER BY key tuples honoring each item's ASC/DESC direction.
fn compare_order_keys(a: &[Value], b: &[Value], order_by: &[OrderByItem]) -> Ordering {
    for (i, ob) in order_by.iter().enumerate() {
        let ord = compare_values(&a[i], &b[i]).unwrap_or(Ordering::Equal);
        let ord = if ob.asc { ord } else { ord.reverse() };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "string_agg"
            | "stddev"
            | "stddev_samp"
            | "stddev_pop"
            | "variance"
            | "var_samp"
            | "var_pop"
            | "bool_and"
            | "bool_or"
            | "every"
            | "percentile_cont"
            | "percentile_disc"
            | "mode"
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
        Expr::Function {
            name,
            args,
            star,
            distinct,
            filter,
            over: None,
        } if is_aggregate_name(name) => eval_aggregate(
            name,
            args,
            *star,
            *distinct,
            filter.as_deref(),
            col_names,
            rows,
        ),
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
    filter: Option<&Expr>,
    col_names: &[String],
    rows: &[Vec<Value>],
) -> Result<Value, String> {
    let lname = name.to_ascii_lowercase();

    let filtered_rows: Vec<&Vec<Value>> = match filter {
        Some(predicate) => rows
            .iter()
            .filter_map(|row| match eval_expr(predicate, col_names, row) {
                Ok(v) if v.is_true() => Some(Ok(row)),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            })
            .collect::<Result<Vec<_>, _>>()?,
        None => rows.iter().collect(),
    };

    // count(*) ignores the argument and counts rows.
    if lname == "count" && star {
        return Ok(Value::Int(filtered_rows.len() as i64));
    }

    // Ordered-set aggregates: `f([fraction]) WITHIN GROUP (ORDER BY expr)`.
    // Desugared so the ordered expression is the last argument; for
    // percentile_* the direct fraction argument is `args[0]`.
    if matches!(lname.as_str(), "percentile_cont" | "percentile_disc" | "mode") {
        let ordered_expr = args
            .last()
            .ok_or_else(|| format!("{lname}() requires a WITHIN GROUP (ORDER BY ...) clause"))?;
        // Collect the (non-null) ordered values.
        let mut vals: Vec<Value> = Vec::new();
        for row in &filtered_rows {
            let v = eval_expr(ordered_expr, col_names, row)?;
            if !v.is_null() {
                vals.push(v);
            }
        }

        if lname == "mode" {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            // Sort so ties resolve to the smallest value, then pick the value
            // with the longest run.
            vals.sort_by(|a, b| compare_values(a, b).unwrap_or(Ordering::Equal));
            let mut best = vals[0].clone();
            let mut best_count = 1usize;
            let mut cur = vals[0].clone();
            let mut cur_count = 1usize;
            for v in &vals[1..] {
                if compare_values(v, &cur) == Some(Ordering::Equal) {
                    cur_count += 1;
                } else {
                    cur = v.clone();
                    cur_count = 1;
                }
                if cur_count > best_count {
                    best_count = cur_count;
                    best = cur.clone();
                }
            }
            return Ok(best);
        }

        // percentile_cont / percentile_disc: fraction is the direct argument.
        let frac = {
            let empty = Vec::new();
            let row = filtered_rows.first().copied().unwrap_or(&empty);
            to_f64(&eval_expr(&args[0], col_names, row)?)?
        };
        if !(0.0..=1.0).contains(&frac) {
            return Err(format!(
                "percentile value {frac} is not between 0 and 1"
            ));
        }
        if vals.is_empty() {
            return Ok(Value::Null);
        }
        vals.sort_by(|a, b| compare_values(a, b).unwrap_or(Ordering::Equal));
        let n = vals.len();
        if lname == "percentile_disc" {
            // Smallest value whose cumulative fraction (i+1)/n >= frac.
            let idx = if frac == 0.0 {
                0
            } else {
                let mut idx = n - 1;
                for i in 0..n {
                    if ((i + 1) as f64) / (n as f64) >= frac {
                        idx = i;
                        break;
                    }
                }
                idx
            };
            return Ok(vals[idx].clone());
        }
        // percentile_cont: linear interpolation over the numeric values.
        let nums: Vec<f64> = vals.iter().map(to_f64).collect::<Result<_, _>>()?;
        let rank = frac * (n as f64 - 1.0);
        let lo = rank.floor() as usize;
        let hi = rank.ceil() as usize;
        let result = if lo == hi {
            nums[lo]
        } else {
            let weight = rank - lo as f64;
            nums[lo] + (nums[hi] - nums[lo]) * weight
        };
        return Ok(Value::Float(result));
    }

    // Collect the (non-null) argument values once, deduplicating for DISTINCT.
    let arg = args
        .first()
        .ok_or_else(|| format!("{lname}() requires an argument"))?;
    let mut vals: Vec<Value> = Vec::new();
    for row in &filtered_rows {
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
            Ok(if is_float {
                Value::Float(float_sum)
            } else {
                Value::Int(int_sum)
            })
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
                        let take = if want_min {
                            ord == Ordering::Less
                        } else {
                            ord == Ordering::Greater
                        };
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
                    let row = filtered_rows.first().copied().unwrap_or(&empty);
                    eval_expr(e, col_names, row)?.to_text().unwrap_or_default()
                }
                None => String::new(),
            };
            let parts: Vec<String> = vals
                .iter()
                .map(|v| v.to_text().unwrap_or_default())
                .collect();
            Ok(Value::Text(parts.join(&sep)))
        }
        // Statistical aggregates over the numeric (non-null) values.
        "stddev" | "stddev_samp" | "stddev_pop" | "variance" | "var_samp" | "var_pop" => {
            let nums: Vec<f64> = vals.iter().map(to_f64).collect::<Result<_, _>>()?;
            let n = nums.len();
            if n == 0 {
                return Ok(Value::Null);
            }
            let population = matches!(lname.as_str(), "stddev_pop" | "var_pop");
            // Sample stats need at least two values; PostgreSQL returns NULL.
            if !population && n < 2 {
                return Ok(Value::Null);
            }
            let mean = nums.iter().sum::<f64>() / n as f64;
            let ss: f64 = nums.iter().map(|x| (x - mean).powi(2)).sum();
            let variance = if population {
                ss / n as f64
            } else {
                ss / (n as f64 - 1.0)
            };
            let want_stddev = lname.starts_with("stddev");
            Ok(Value::Float(if want_stddev {
                variance.sqrt()
            } else {
                variance
            }))
        }
        // Boolean aggregates.
        "bool_and" | "every" | "bool_or" => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            let want_and = lname != "bool_or";
            let mut acc = want_and;
            for v in &vals {
                let b = v.is_true();
                if want_and {
                    acc = acc && b;
                } else {
                    acc = acc || b;
                }
            }
            Ok(Value::Bool(acc))
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
        DataType::Float4 | DataType::Float8 | DataType::Numeric | DataType::Money => match v {
            Value::Float(f) => Ok(Value::Float(f)),
            Value::Int(i) => Ok(Value::Float(i as f64)),
            Value::Text(s) => s.trim().parse::<f64>().map(Value::Float).map_err(|_| {
                format!(
                    "invalid input syntax for type {}: \"{s}\"",
                    target.sql_name()
                )
            }),
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
        // Interval text is canonicalised on the way in.
        DataType::Interval => {
            let text = v.to_text().unwrap_or_default();
            Ok(Value::Text(normalize_interval(&text)?))
        }
        // Text and the date/time/uuid/json family are stored as text.
        DataType::Text
        | DataType::Bytea
        | DataType::Date
        | DataType::Time
        | DataType::TimeTz
        | DataType::Timestamp
        | DataType::TimestampTz
        | DataType::Inet
        | DataType::Cidr
        | DataType::Macaddr
        | DataType::Macaddr8
        | DataType::Uuid
        | DataType::Json
        | DataType::Jsonb
        | DataType::Xml
        | DataType::TsVector
        | DataType::TsQuery => Ok(Value::Text(v.to_text().unwrap_or_default())),
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
        Expr::IsNull { .. } | Expr::IsDistinctFrom { .. } => DataType::Bool,
        Expr::Cast { target, .. } => *target,
        Expr::Like { .. }
        | Expr::InList { .. }
        | Expr::Between { .. }
        | Expr::QuantifiedCompare { .. } => DataType::Bool,
        Expr::Exists(_) | Expr::InSubquery { .. } => DataType::Bool,
        Expr::Row(_) | Expr::Array(_) => DataType::Text,
        // A scalar subquery's type is only known once executed; default to text
        // for the pre-execution RowDescription (the value is resolved later).
        Expr::ScalarSubquery(_) => DataType::Text,
        Expr::Case {
            whens, else_expr, ..
        } => {
            // Type of the first THEN result (fallback to ELSE, then text).
            if let Some((_, result)) = whens.first() {
                infer_expr_type(result, col_names, col_types)
            } else if let Some(e) = else_expr {
                infer_expr_type(e, col_names, col_types)
            } else {
                DataType::Text
            }
        }
        Expr::Unary {
            op: UnaryOp::Not, ..
        } => DataType::Bool,
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
            | BinaryOp::RegexNotMatch { .. }
            | BinaryOp::ArrayContains
            | BinaryOp::ArrayContainedBy
            | BinaryOp::ArrayOverlap
            | BinaryOp::NetworkContainedBy
            | BinaryOp::NetworkContainedByEq
            | BinaryOp::NetworkContains
            | BinaryOp::NetworkContainsEq
            | BinaryOp::TextSearchMatch => DataType::Bool,
            BinaryOp::Concat | BinaryOp::JsonGet | BinaryOp::JsonGetText => DataType::Text,
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
        // Ranking window functions are bigint; value windows (lag/lead/…) take
        // the type of their first argument.
        Expr::Function {
            name,
            args,
            over: Some(_),
            ..
        } => match name.to_ascii_lowercase().as_str() {
            "row_number" | "rank" | "dense_rank" | "ntile" | "count" => DataType::Int8,
            "lag" | "lead" | "first_value" | "last_value" | "nth_value" | "min" | "max" | "sum" => {
                args.first()
                    .map(|a| infer_expr_type(a, col_names, col_types))
                    .unwrap_or(DataType::Int8)
            }
            "avg" => DataType::Float8,
            _ => DataType::Text,
        },
        Expr::Function { name, args, .. } => match name.to_ascii_lowercase().as_str() {
            "count" => DataType::Int8,
            "sum" | "abs" => DataType::Int8,
            "percentile_cont" => DataType::Float8,
            // percentile_disc / mode return a value of the ordered column's type
            // (the desugared last argument).
            "percentile_disc" | "mode" => args
                .last()
                .map(|a| infer_expr_type(a, col_names, col_types))
                .unwrap_or(DataType::Float8),
            "stddev" | "stddev_samp" | "stddev_pop" | "variance" | "var_samp" | "var_pop" => {
                DataType::Float8
            }
            "bool_and" | "bool_or" | "every" => DataType::Bool,
            "avg" | "round" | "date_part" | "extract" | "ts_rank" => DataType::Float8,
            "length" | "char_length" | "character_length" | "array_length" | "cardinality"
            | "array_position" => DataType::Int8,
            "ascii" | "strpos" | "div" | "gcd" | "lcm" => DataType::Int8,
            "sqrt" | "cbrt" | "exp" | "ln" | "log" | "log10" | "power" | "pow" | "pi"
            | "radians" | "degrees" | "sin" | "cos" | "tan" | "cot" | "asin" | "acos" | "atan"
            | "atan2" | "ceil" | "ceiling" | "floor" | "sign" | "mod" => DataType::Float8,
            "starts_with" => DataType::Bool,
            "jsonb_path_exists" | "json_path_exists" => DataType::Bool,
            "jsonb_path_query" | "json_path_query" => DataType::Jsonb,
            "json_array_length" | "jsonb_array_length" => DataType::Int8,
            "to_tsvector" => DataType::TsVector,
            "plainto_tsquery" | "to_tsquery" => DataType::TsQuery,
            "pg_try_advisory_lock" | "pg_advisory_unlock" => DataType::Bool,
            _ => DataType::Text,
        },
    }
}
