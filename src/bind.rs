//! Binding of extended-protocol parameters into a parsed statement.
//!
//! The parser produces [`Expr::Param`] nodes for `$N` placeholders. Before a
//! prepared statement is executed, we substitute each placeholder with the
//! concrete [`Value`] supplied by the client's Bind message. This keeps the
//! executor itself parameter-free.

use crate::sql::ast::*;
use crate::types::Value;

/// Substitute every `$N` placeholder in `stmt` with the bound `params`
/// (0-indexed `Vec`, so `$1` is `params[0]`).
pub fn bind_statement(stmt: &mut Statement, params: &[Value]) -> Result<(), String> {
    match stmt {
        Statement::Insert(i) => {
            for tuple in &mut i.rows {
                for e in tuple {
                    bind_expr(e, params)?;
                }
            }
        }
        Statement::Select(s) => bind_select(s, params)?,
        Statement::Update(u) => {
            for (_, e) in &mut u.assignments {
                bind_expr(e, params)?;
            }
            if let Some(f) = &mut u.filter {
                bind_expr(f, params)?;
            }
        }
        Statement::Delete(d) => {
            if let Some(f) = &mut d.filter {
                bind_expr(f, params)?;
            }
        }
        // Other statements never contain parameters.
        _ => {}
    }
    Ok(())
}

fn bind_select(s: &mut Select, params: &[Value]) -> Result<(), String> {
    for item in &mut s.projection {
        if let SelectItem::Expr { expr, .. } = item {
            bind_expr(expr, params)?;
        }
    }
    if let Some(f) = &mut s.filter {
        bind_expr(f, params)?;
    }
    for o in &mut s.order_by {
        bind_expr(&mut o.expr, params)?;
    }
    if let Some(l) = &mut s.limit {
        bind_expr(l, params)?;
    }
    if let Some(o) = &mut s.offset {
        bind_expr(o, params)?;
    }
    Ok(())
}

fn bind_expr(expr: &mut Expr, params: &[Value]) -> Result<(), String> {
    match expr {
        Expr::Param(n) => {
            let idx = (*n as usize)
                .checked_sub(1)
                .ok_or_else(|| "parameter $0 is invalid".to_string())?;
            let v = params
                .get(idx)
                .ok_or_else(|| format!("bind message supplies too few parameters for ${n}"))?;
            *expr = value_to_expr(v);
            Ok(())
        }
        Expr::Unary { expr, .. } => bind_expr(expr, params),
        Expr::Binary { left, right, .. } => {
            bind_expr(left, params)?;
            bind_expr(right, params)
        }
        Expr::IsNull { expr, .. } => bind_expr(expr, params),
        Expr::Cast { expr, .. } => bind_expr(expr, params),
        // Parameters inside subqueries are not bound (uncommon); the IN-test's
        // left operand still is.
        Expr::InSubquery { expr, .. } => bind_expr(expr, params),
        Expr::ScalarSubquery(_) | Expr::Exists(_) => Ok(()),
        Expr::Like { expr, pattern, .. } => {
            bind_expr(expr, params)?;
            bind_expr(pattern, params)
        }
        Expr::InList { expr, list, .. } => {
            bind_expr(expr, params)?;
            for e in list {
                bind_expr(e, params)?;
            }
            Ok(())
        }
        Expr::Between { expr, low, high, .. } => {
            bind_expr(expr, params)?;
            bind_expr(low, params)?;
            bind_expr(high, params)
        }
        Expr::Case { operand, whens, else_expr } => {
            if let Some(o) = operand {
                bind_expr(o, params)?;
            }
            for (c, r) in whens {
                bind_expr(c, params)?;
                bind_expr(r, params)?;
            }
            if let Some(e) = else_expr {
                bind_expr(e, params)?;
            }
            Ok(())
        }
        Expr::Function { args, .. } => {
            for a in args {
                bind_expr(a, params)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn value_to_expr(v: &Value) -> Expr {
    match v {
        Value::Null => Expr::Null,
        Value::Int(i) => Expr::Int(*i),
        Value::Float(f) => Expr::Float(*f),
        Value::Text(s) => Expr::Str(s.clone()),
        Value::Bool(b) => Expr::Bool(*b),
    }
}

/// Decode a single Bind parameter into a [`Value`].
///
/// `format` is 0 for text, 1 for binary. `oid` is the parameter's type OID
/// from the Parse message (0 = unspecified). For unspecified text parameters
/// we infer a reasonable type from the textual form.
pub fn decode_param(raw: &Option<Vec<u8>>, format: i16, oid: i32) -> Result<Value, String> {
    let Some(bytes) = raw else {
        return Ok(Value::Null);
    };
    if format == 1 {
        decode_binary(bytes, oid)
    } else {
        let s = String::from_utf8_lossy(bytes).into_owned();
        Ok(decode_text(s, oid))
    }
}

fn decode_text(s: String, oid: i32) -> Value {
    match oid {
        16 => Value::Bool(matches!(s.as_str(), "t" | "true" | "1" | "yes" | "on")),
        20 | 21 | 23 => s.parse::<i64>().map(Value::Int).unwrap_or(Value::Text(s)),
        700 | 701 | 1700 => s.parse::<f64>().map(Value::Float).unwrap_or(Value::Text(s)),
        25 | 1042 | 1043 | 0 => {
            // Unspecified: infer from the lexical form so comparisons work.
            if oid == 0 {
                if let Ok(i) = s.parse::<i64>() {
                    return Value::Int(i);
                }
                if let Ok(f) = s.parse::<f64>() {
                    return Value::Float(f);
                }
            }
            Value::Text(s)
        }
        _ => Value::Text(s),
    }
}

fn decode_binary(bytes: &[u8], oid: i32) -> Result<Value, String> {
    let bad = |t: &str| format!("invalid binary length for {t}");
    match oid {
        16 => Ok(Value::Bool(bytes.first().copied().unwrap_or(0) != 0)),
        21 => {
            let b: [u8; 2] = bytes.try_into().map_err(|_| bad("int2"))?;
            Ok(Value::Int(i16::from_be_bytes(b) as i64))
        }
        23 => {
            let b: [u8; 4] = bytes.try_into().map_err(|_| bad("int4"))?;
            Ok(Value::Int(i32::from_be_bytes(b) as i64))
        }
        20 => {
            let b: [u8; 8] = bytes.try_into().map_err(|_| bad("int8"))?;
            Ok(Value::Int(i64::from_be_bytes(b)))
        }
        700 => {
            let b: [u8; 4] = bytes.try_into().map_err(|_| bad("float4"))?;
            Ok(Value::Float(f32::from_be_bytes(b) as f64))
        }
        701 => {
            let b: [u8; 8] = bytes.try_into().map_err(|_| bad("float8"))?;
            Ok(Value::Float(f64::from_be_bytes(b)))
        }
        _ => Ok(Value::Text(String::from_utf8_lossy(bytes).into_owned())),
    }
}
