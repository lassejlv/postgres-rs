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
        Statement::Insert(i) => insert_sql(i),
        Statement::Update(u) => update_sql(u),
        Statement::Delete(d) => delete_sql(d),
        Statement::Select(_) => String::new(),
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
