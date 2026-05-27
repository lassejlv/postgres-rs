//! Serialize an AST back into SQL text.
//!
//! Used by the write-ahead log: mutating statements are re-emitted as
//! canonical SQL and replayed on startup. Correctness over prettiness —
//! binary expressions are fully parenthesized so the result re-parses to an
//! identical tree regardless of operator precedence.

use super::ast::*;
use crate::types::{DataType, Value};

/// Serialize a statement to SQL. Only the variants the WAL persists
/// (DDL/DML) need to round-trip; others produce a best-effort rendering.
pub fn statement_to_sql(stmt: &Statement) -> String {
    match stmt {
        Statement::CreateTable(c) => create_table_sql(c),
        Statement::DropTable(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!("DROP TABLE {exists}{}", ident(&d.name))
        }
        Statement::CreateIndex(c) => create_index_sql(c),
        Statement::DropIndex(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!("DROP INDEX {exists}{}", ident(&d.name))
        }
        Statement::Insert(i) => insert_sql(i),
        Statement::Update(u) => update_sql(u),
        Statement::Delete(d) => delete_sql(d),
        Statement::Select(s) => select_to_sql(s),
        Statement::Begin => "BEGIN".into(),
        Statement::Commit => "COMMIT".into(),
        Statement::Rollback => "ROLLBACK".into(),
        Statement::Set { name, .. } => format!("SET {name}"),
        Statement::Show { name } => format!("SHOW {name}"),
        Statement::Empty => String::new(),
    }
}

fn create_table_sql(c: &CreateTable) -> String {
    let exists = if c.if_not_exists { "IF NOT EXISTS " } else { "" };
    let cols: Vec<String> = c
        .columns
        .iter()
        .map(|col| {
            // `serial` columns re-emit as serial so replay rebuilds the
            // sequence; their NOT NULL and default are implicit.
            let type_name = if col.serial {
                match col.data_type {
                    DataType::Int2 => "smallserial",
                    DataType::Int8 => "bigserial",
                    _ => "serial",
                }
            } else {
                col.data_type.sql_name()
            };
            let mut s = format!("{} {}", ident(&col.name), type_name);
            if col.primary_key {
                s.push_str(" PRIMARY KEY");
            } else if col.not_null && !col.serial {
                s.push_str(" NOT NULL");
            }
            if !col.serial {
                if let Some(default) = &col.default {
                    s.push_str(&format!(" DEFAULT {}", expr_to_sql(default)));
                }
            }
            s
        })
        .collect();
    format!("CREATE TABLE {exists}{} ({})", ident(&c.name), cols.join(", "))
}

fn create_index_sql(c: &CreateIndex) -> String {
    let unique = if c.unique { "UNIQUE " } else { "" };
    let exists = if c.if_not_exists { "IF NOT EXISTS " } else { "" };
    let name = match &c.name {
        // A name is required by our parser unless `ON` follows immediately, so
        // re-emit the (possibly auto-generated) name to keep replay stable.
        Some(n) => format!("{} ", ident(n)),
        None => String::new(),
    };
    format!(
        "CREATE {unique}INDEX {exists}{name}ON {} ({})",
        ident(&c.table),
        ident(&c.column)
    )
}

fn insert_sql(i: &Insert) -> String {
    let cols = match &i.columns {
        Some(names) => {
            let list: Vec<String> = names.iter().map(|n| ident(n)).collect();
            format!(" ({})", list.join(", "))
        }
        None => String::new(),
    };
    let tuples: Vec<String> = i
        .rows
        .iter()
        .map(|tuple| {
            let vals: Vec<String> = tuple.iter().map(expr_to_sql).collect();
            format!("({})", vals.join(", "))
        })
        .collect();
    format!("INSERT INTO {}{} VALUES {}", ident(&i.table), cols, tuples.join(", "))
}

fn update_sql(u: &Update) -> String {
    let sets: Vec<String> = u
        .assignments
        .iter()
        .map(|(c, e)| format!("{} = {}", ident(c), expr_to_sql(e)))
        .collect();
    let mut s = format!("UPDATE {} SET {}", ident(&u.table), sets.join(", "));
    if let Some(f) = &u.filter {
        s.push_str(&format!(" WHERE {}", expr_to_sql(f)));
    }
    s
}

fn delete_sql(d: &Delete) -> String {
    let mut s = format!("DELETE FROM {}", ident(&d.table));
    if let Some(f) = &d.filter {
        s.push_str(&format!(" WHERE {}", expr_to_sql(f)));
    }
    s
}

/// Serialize a `SELECT` back to SQL. Used for subqueries embedded in logged
/// DML and (harmlessly) for any standalone SELECT.
pub fn select_to_sql(sel: &Select) -> String {
    let mut s = String::from("SELECT ");
    if sel.distinct {
        s.push_str("DISTINCT ");
    }
    let items: Vec<String> = sel.projection.iter().map(select_item_to_sql).collect();
    s.push_str(&items.join(", "));

    if let Some(from) = &sel.from {
        s.push_str(" FROM ");
        s.push_str(&from_clause_to_sql(from));
    }
    if let Some(f) = &sel.filter {
        s.push_str(&format!(" WHERE {}", expr_to_sql(f)));
    }
    if !sel.group_by.is_empty() {
        let g: Vec<String> = sel.group_by.iter().map(expr_to_sql).collect();
        s.push_str(&format!(" GROUP BY {}", g.join(", ")));
    }
    if let Some(h) = &sel.having {
        s.push_str(&format!(" HAVING {}", expr_to_sql(h)));
    }
    if !sel.order_by.is_empty() {
        let o: Vec<String> = sel
            .order_by
            .iter()
            .map(|ob| format!("{}{}", expr_to_sql(&ob.expr), if ob.asc { "" } else { " DESC" }))
            .collect();
        s.push_str(&format!(" ORDER BY {}", o.join(", ")));
    }
    if let Some(l) = &sel.limit {
        s.push_str(&format!(" LIMIT {}", expr_to_sql(l)));
    }
    if let Some(o) = &sel.offset {
        s.push_str(&format!(" OFFSET {}", expr_to_sql(o)));
    }
    s
}

fn select_item_to_sql(item: &SelectItem) -> String {
    match item {
        SelectItem::Wildcard => "*".to_string(),
        SelectItem::Expr { expr, alias } => match alias {
            Some(a) => format!("{} AS {}", expr_to_sql(expr), ident(a)),
            None => expr_to_sql(expr),
        },
    }
}

fn from_clause_to_sql(from: &FromClause) -> String {
    let mut s = table_ref_to_sql(&from.base);
    for j in &from.joins {
        let kw = match j.kind {
            JoinKind::Inner => "JOIN",
            JoinKind::Left => "LEFT JOIN",
            JoinKind::Right => "RIGHT JOIN",
            JoinKind::Full => "FULL JOIN",
            JoinKind::Cross => "CROSS JOIN",
        };
        s.push_str(&format!(" {kw} {}", table_ref_to_sql(&j.table)));
        if let Some(on) = &j.on {
            s.push_str(&format!(" ON {}", expr_to_sql(on)));
        }
    }
    s
}

fn table_ref_to_sql(t: &TableRef) -> String {
    let mut s = String::new();
    if let Some(schema) = &t.schema {
        s.push_str(&ident(schema));
        s.push('.');
    }
    s.push_str(&ident(&t.name));
    if let Some(a) = &t.alias {
        s.push_str(&format!(" AS {}", ident(a)));
    }
    s
}

/// Serialize an expression. Binary/unary ops are parenthesized for safety.
pub fn expr_to_sql(e: &Expr) -> String {
    match e {
        Expr::Int(i) => i.to_string(),
        Expr::Float(f) => Value::Float(*f).to_text().unwrap_or_else(|| "0".into()),
        Expr::Str(s) => quote_string(s),
        Expr::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Expr::Null => "NULL".into(),
        Expr::Param(n) => format!("${n}"),
        Expr::Column(name) => ident(name),
        Expr::QualifiedColumn { qualifier, name } => format!("{}.{}", ident(qualifier), ident(name)),
        Expr::Unary { op, expr } => {
            let inner = expr_to_sql(expr);
            match op {
                UnaryOp::Neg => format!("(-{inner})"),
                UnaryOp::Not => format!("(NOT {inner})"),
            }
        }
        Expr::Binary { op, left, right } => {
            format!("({} {} {})", expr_to_sql(left), binop_sql(*op), expr_to_sql(right))
        }
        Expr::IsNull { expr, negated } => {
            let kw = if *negated { "IS NOT NULL" } else { "IS NULL" };
            format!("({} {kw})", expr_to_sql(expr))
        }
        Expr::Like { expr, pattern, negated, case_insensitive } => {
            let op = match (*negated, *case_insensitive) {
                (false, false) => "LIKE",
                (true, false) => "NOT LIKE",
                (false, true) => "ILIKE",
                (true, true) => "NOT ILIKE",
            };
            format!("({} {op} {})", expr_to_sql(expr), expr_to_sql(pattern))
        }
        Expr::InList { expr, list, negated } => {
            let items: Vec<String> = list.iter().map(expr_to_sql).collect();
            let op = if *negated { "NOT IN" } else { "IN" };
            format!("({} {op} ({}))", expr_to_sql(expr), items.join(", "))
        }
        Expr::Between { expr, low, high, negated } => {
            let op = if *negated { "NOT BETWEEN" } else { "BETWEEN" };
            format!("({} {op} {} AND {})", expr_to_sql(expr), expr_to_sql(low), expr_to_sql(high))
        }
        Expr::Case { operand, whens, else_expr } => {
            let mut s = String::from("CASE");
            if let Some(o) = operand {
                s.push(' ');
                s.push_str(&expr_to_sql(o));
            }
            for (c, r) in whens {
                s.push_str(&format!(" WHEN {} THEN {}", expr_to_sql(c), expr_to_sql(r)));
            }
            if let Some(e) = else_expr {
                s.push_str(&format!(" ELSE {}", expr_to_sql(e)));
            }
            s.push_str(" END");
            s
        }
        Expr::Cast { expr, target } => {
            format!("CAST({} AS {})", expr_to_sql(expr), target.sql_name())
        }
        Expr::ScalarSubquery(sel) => format!("({})", select_to_sql(sel)),
        Expr::Exists(sel) => format!("EXISTS ({})", select_to_sql(sel)),
        Expr::InSubquery { expr, subquery, negated } => {
            let op = if *negated { "NOT IN" } else { "IN" };
            format!("({} {op} ({}))", expr_to_sql(expr), select_to_sql(subquery))
        }
        Expr::Function { name, args, star } => {
            if *star {
                format!("{name}(*)")
            } else {
                let a: Vec<String> = args.iter().map(expr_to_sql).collect();
                format!("{name}({})", a.join(", "))
            }
        }
    }
}

fn binop_sql(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Mod => "%",
        BinaryOp::Concat => "||",
        BinaryOp::Eq => "=",
        BinaryOp::NotEq => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::LtEq => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::GtEq => ">=",
        BinaryOp::And => "AND",
        BinaryOp::Or => "OR",
        BinaryOp::RegexMatch { ci: false } => "~",
        BinaryOp::RegexMatch { ci: true } => "~*",
        BinaryOp::RegexNotMatch { ci: false } => "!~",
        BinaryOp::RegexNotMatch { ci: true } => "!~*",
    }
}

/// Emit an identifier, double-quoting it if it isn't a simple lowercase name
/// (so case and special characters round-trip through the parser).
fn ident(name: &str) -> String {
    let simple = !name.is_empty()
        && name.bytes().next().is_some_and(|b| b == b'_' || b.is_ascii_lowercase())
        && name.bytes().all(|b| b == b'_' || b.is_ascii_lowercase() || b.is_ascii_digit());
    if simple {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// Emit a single-quoted string literal, doubling embedded single quotes.
fn quote_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
