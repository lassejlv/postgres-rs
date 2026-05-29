//! A small PL/pgSQL interpreter for `LANGUAGE plpgsql` scalar functions.
//!
//! This implements a usable subset of PL/pgSQL sufficient for scalar functions
//! that compute over their arguments and local variables:
//!
//! - an optional `DECLARE <var> <type> [:= <expr>];` section preceding the
//!   mandatory `BEGIN ... END` block (a trailing `;` after `END` is allowed);
//! - assignment: `var := <expr>;` and `SELECT <expr> INTO var;`;
//! - `IF cond THEN ... [ELSIF cond THEN ...] [ELSE ...] END IF;`;
//! - `RETURN <expr>;`;
//! - `WHILE cond LOOP ... END LOOP;` and
//!   `FOR i IN a..b LOOP ... END LOOP;`;
//! - `RAISE [NOTICE|EXCEPTION] '<msg>';` (EXCEPTION aborts with an error;
//!   NOTICE is a no-op here).
//!
//! Expressions and conditions are evaluated by reusing the engine's SQL
//! expression evaluator: the expression text is parsed (by wrapping it in a
//! `SELECT <expr>` and reusing the existing parser) into an [`Expr`], then
//! evaluated with [`crate::executor::eval_expr`] using the current variable
//! bindings as the row scope (variable name -> [`Value`]). The function's
//! parameters are bound by their declared names and as `$1`, `$2`, ... .
//!
//! Scope / limitation: expression evaluation here has no database handle, so
//! `SELECT ... INTO` is limited to expressions with **no `FROM` clause** (i.e.
//! computing over variables/constants). `SELECT ... INTO` *from a table* is not
//! supported and reports a clean error. All required pure-computation
//! constructs (e.g. factorial via a loop) work.
//!
//! Anything outside the supported subset reports a clean `Err(String)` and
//! never panics, leaving SQL-language functions unaffected.

use crate::sql::ast::{Expr, SelectItem, Statement};
use crate::types::{DataType, Value};

/// A parsed PL/pgSQL statement.
#[derive(Debug, Clone)]
enum PlStmt {
    /// `var := <expr>;` or `SELECT <expr> INTO var;`.
    Assign { var: String, expr: Expr },
    /// `RETURN <expr>;`
    Return(Expr),
    /// `RAISE EXCEPTION '<msg>';` (`exception` true) or `RAISE NOTICE '<msg>';`.
    Raise { exception: bool, message: String },
    /// `IF ... THEN ... [ELSIF ...] [ELSE ...] END IF;`
    If {
        branches: Vec<(Expr, Vec<PlStmt>)>,
        else_body: Vec<PlStmt>,
    },
    /// `WHILE cond LOOP ... END LOOP;`
    While { cond: Expr, body: Vec<PlStmt> },
    /// `FOR i IN a..b LOOP ... END LOOP;`
    For {
        var: String,
        from: Expr,
        to: Expr,
        body: Vec<PlStmt>,
    },
}

/// A declared local variable.
#[derive(Debug, Clone)]
struct Decl {
    name: String,
    ty: Option<DataType>,
    init: Option<Expr>,
}

/// Control-flow outcome of executing a statement list.
enum Flow {
    /// Fell through without returning.
    Normal,
    /// `RETURN <value>` was executed.
    Return(Value),
}

/// Safety valve so a runaway `WHILE` reports an error instead of hanging.
const MAX_LOOP_ITERS: u64 = 100_000_000;

/// Interpret a PL/pgSQL function `body` with the call `params` bound to the
/// function's parameter names (and to `$1`, `$2`, ...). Returns the value
/// produced by the function's `RETURN`, or `Value::Null` if it ran off the end.
///
/// `param_names` are the (lowercased) declared parameter names, parallel to
/// `params`. Unnamed parameters get an empty string and are reachable only as
/// `$N`.
pub fn eval_plpgsql(
    body: &str,
    param_names: &[String],
    params: &[Value],
) -> Result<Value, String> {
    let tokens = tokenize(body)?;
    let mut p = PlParser {
        body,
        toks: &tokens,
        pos: 0,
    };
    let (decls, stmts) = p.parse_program()?;

    // Build the variable scope: parameters by name and as $N, then declared
    // locals (whose initializers may reference parameters / earlier locals).
    let mut scope = Scope::new();
    for (i, v) in params.iter().enumerate() {
        scope.set(&format!("${}", i + 1), v.clone());
        if let Some(name) = param_names.get(i).filter(|n| !n.is_empty()) {
            scope.set(name, v.clone());
        }
    }
    for d in &decls {
        let v = match &d.init {
            Some(e) => coerce_opt(scope.eval(e)?, d.ty)?,
            None => Value::Null,
        };
        scope.set(&d.name, v);
    }

    match exec_block(&stmts, &mut scope)? {
        Flow::Return(v) => Ok(v),
        Flow::Normal => Ok(Value::Null),
    }
}

/// Coerce a value to an optional declared type (no-op when `None`).
fn coerce_opt(v: Value, ty: Option<DataType>) -> Result<Value, String> {
    match ty {
        Some(t) => crate::executor::coerce_value(v, t),
        None => Ok(v),
    }
}

/// Execute a list of statements, threading the variable scope. A `RETURN`
/// short-circuits the remaining statements.
fn exec_block(stmts: &[PlStmt], scope: &mut Scope) -> Result<Flow, String> {
    for s in stmts {
        if let ret @ Flow::Return(_) = exec_stmt(s, scope)? {
            return Ok(ret);
        }
    }
    Ok(Flow::Normal)
}

fn exec_stmt(s: &PlStmt, scope: &mut Scope) -> Result<Flow, String> {
    match s {
        PlStmt::Assign { var, expr } => {
            if !scope.contains(var) {
                return Err(format!("\"{var}\" is not a known variable"));
            }
            let v = scope.eval(expr)?;
            scope.set(var, v);
            Ok(Flow::Normal)
        }
        PlStmt::Return(expr) => Ok(Flow::Return(scope.eval(expr)?)),
        PlStmt::Raise { exception, message } => {
            if *exception {
                Err(message.clone())
            } else {
                Ok(Flow::Normal) // NOTICE: no-op on this path.
            }
        }
        PlStmt::If {
            branches,
            else_body,
        } => {
            for (cond, body) in branches {
                if scope.eval(cond)?.is_true() {
                    return exec_block(body, scope);
                }
            }
            exec_block(else_body, scope)
        }
        PlStmt::While { cond, body } => {
            let mut guard = 0u64;
            while scope.eval(cond)?.is_true() {
                if let Flow::Return(v) = exec_block(body, scope)? {
                    return Ok(Flow::Return(v));
                }
                guard += 1;
                if guard > MAX_LOOP_ITERS {
                    return Err("WHILE loop exceeded iteration limit".into());
                }
            }
            Ok(Flow::Normal)
        }
        PlStmt::For {
            var,
            from,
            to,
            body,
        } => {
            let lo = as_int(scope.eval(from)?, "FOR loop bound")?;
            let hi = as_int(scope.eval(to)?, "FOR loop bound")?;
            let mut i = lo;
            while i <= hi {
                scope.set(var, Value::Int(i));
                if let Flow::Return(v) = exec_block(body, scope)? {
                    return Ok(Flow::Return(v));
                }
                if i == i64::MAX {
                    break;
                }
                i += 1;
            }
            Ok(Flow::Normal)
        }
    }
}

fn as_int(v: Value, ctx: &str) -> Result<i64, String> {
    match crate::executor::coerce_value(v, DataType::Int8)? {
        Value::Int(i) => Ok(i),
        Value::Null => Err(format!("{ctx} cannot be NULL")),
        other => Err(format!("{ctx} must be an integer, found {other:?}")),
    }
}

/// The variable scope: ordered name->value bindings reused as the `col_names`/
/// `row` pair passed to the engine's expression evaluator.
struct Scope {
    names: Vec<String>,
    values: Vec<Value>,
}

impl Scope {
    fn new() -> Self {
        Scope {
            names: Vec::new(),
            values: Vec::new(),
        }
    }

    fn contains(&self, name: &str) -> bool {
        self.names.iter().any(|n| n.eq_ignore_ascii_case(name))
    }

    /// Bind (or rebind) `name` to `value`.
    fn set(&mut self, name: &str, value: Value) {
        if let Some(i) = self.names.iter().position(|n| n.eq_ignore_ascii_case(name)) {
            self.values[i] = value;
        } else {
            self.names.push(name.to_string());
            self.values.push(value);
        }
    }

    /// Evaluate a parsed expression against the current bindings.
    fn eval(&self, expr: &Expr) -> Result<Value, String> {
        crate::executor::eval_expr(expr, &self.names, &self.values)
    }
}

/// Parse an expression from its source text by wrapping it in `SELECT <text>`
/// and reusing the SQL parser, returning the single projection expression.
fn parse_expr_text(text: &str) -> Result<Expr, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("empty expression in plpgsql body".into());
    }
    let sql = format!("SELECT {trimmed}");
    let stmts = crate::sql::Parser::parse_sql(&sql)
        .map_err(|e| format!("invalid plpgsql expression `{trimmed}`: {e}"))?;
    let Some(Statement::Select(sel)) = stmts.into_iter().next() else {
        return Err(format!("invalid plpgsql expression `{trimmed}`"));
    };
    if sel.from.is_some() {
        return Err("SELECT INTO from a table is not supported in plpgsql functions here".into());
    }
    if sel.projection.len() != 1 {
        return Err(format!("invalid plpgsql expression `{trimmed}`"));
    }
    match sel.projection.into_iter().next() {
        Some(SelectItem::Expr { expr, .. }) => Ok(expr),
        _ => Err(format!("invalid plpgsql expression `{trimmed}`")),
    }
}

// --- Tokenizer ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum TokKind {
    Word(String), // identifier or keyword (compared case-insensitively)
    Assign,       // :=
    DotDot,       // ..
    Semi,         // ;
    Str(String),  // single-quoted string literal (unescaped contents)
    /// Any other lexeme (operator, number, paren, `$N`, `::`, ...).
    Other,
}

/// A token plus its byte span in the original body, so expression text can be
/// recovered verbatim by slicing the source between two tokens (preserving the
/// exact spelling of operators, numbers and casts).
#[derive(Debug, Clone)]
struct Tok {
    kind: TokKind,
    start: usize,
    end: usize,
}

/// Tokenize a PL/pgSQL body into structural tokens with source spans. Comments
/// are stripped; everything else becomes a token carrying its byte range.
fn tokenize(body: &str) -> Result<Vec<Tok>, String> {
    let mut toks = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // Line comment `-- ...`
        if c == '-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment `/* ... */`
        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        let start = i;
        if c == ':' && i + 1 < bytes.len() && bytes[i + 1] == b'=' {
            i += 2;
            toks.push(Tok { kind: TokKind::Assign, start, end: i });
            continue;
        }
        if c == '.' && i + 1 < bytes.len() && bytes[i + 1] == b'.' {
            i += 2;
            toks.push(Tok { kind: TokKind::DotDot, start, end: i });
            continue;
        }
        if c == ';' {
            i += 1;
            toks.push(Tok { kind: TokKind::Semi, start, end: i });
            continue;
        }
        if c == '\'' {
            let mut s = String::new();
            i += 1;
            loop {
                if i >= bytes.len() {
                    return Err("unterminated string literal in plpgsql body".into());
                }
                if bytes[i] == b'\'' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        s.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                s.push(bytes[i] as char);
                i += 1;
            }
            toks.push(Tok { kind: TokKind::Str(s), start, end: i });
            continue;
        }
        // A word: identifier-ish run.
        if c == '_' || c.is_ascii_alphabetic() {
            while i < bytes.len() {
                let ch = bytes[i] as char;
                if ch == '_' || ch.is_ascii_alphanumeric() {
                    i += 1;
                } else {
                    break;
                }
            }
            toks.push(Tok {
                kind: TokKind::Word(body[start..i].to_string()),
                start,
                end: i,
            });
            continue;
        }
        // Any other lexeme: a maximal run of non-word, non-whitespace,
        // non-structural characters (operators like `>=`, numbers, `$1`, `::`).
        while i < bytes.len() {
            let ch = bytes[i] as char;
            if ch.is_whitespace()
                || ch == ';'
                || ch == '\''
                || ch == '_'
                || ch.is_ascii_alphabetic()
            {
                break;
            }
            if (ch == ':' && i + 1 < bytes.len() && bytes[i + 1] == b'=')
                || (ch == '.' && i + 1 < bytes.len() && bytes[i + 1] == b'.')
                || (ch == '-' && i + 1 < bytes.len() && bytes[i + 1] == b'-')
                || (ch == '/' && i + 1 < bytes.len() && bytes[i + 1] == b'*')
            {
                break;
            }
            i += 1;
        }
        if i == start {
            i += 1;
        }
        toks.push(Tok { kind: TokKind::Other, start, end: i });
    }
    Ok(toks)
}

// --- Parser ---------------------------------------------------------------

struct PlParser<'a> {
    body: &'a str,
    toks: &'a [Tok],
    pos: usize,
}

impl<'a> PlParser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn bump(&mut self) -> Option<&Tok> {
        let t = self.toks.get(self.pos);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Tok { kind: TokKind::Word(x), .. }) if x.eq_ignore_ascii_case(w))
    }

    fn eat_word(&mut self, w: &str) -> bool {
        if self.is_word(w) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self, w: &str) -> Result<(), String> {
        if self.eat_word(w) {
            Ok(())
        } else {
            Err(format!(
                "expected `{w}` in plpgsql body, found {:?}",
                self.peek().map(|t| &t.kind)
            ))
        }
    }

    fn expect_semi(&mut self) -> Result<(), String> {
        match self.bump().map(|t| &t.kind) {
            Some(TokKind::Semi) => Ok(()),
            other => Err(format!("expected `;` in plpgsql body, found {other:?}")),
        }
    }

    /// Parse the whole function: optional `DECLARE ...` then `BEGIN ... END`.
    fn parse_program(&mut self) -> Result<(Vec<Decl>, Vec<PlStmt>), String> {
        let decls = if self.eat_word("declare") {
            self.parse_decls()?
        } else {
            Vec::new()
        };
        self.expect_word("begin")?;
        let body = self.parse_stmts_until(&["end"])?;
        self.expect_word("end")?;
        // Optional label after END and optional trailing `;`.
        if matches!(self.peek().map(|t| &t.kind), Some(TokKind::Word(_))) {
            self.bump();
        }
        if matches!(self.peek().map(|t| &t.kind), Some(TokKind::Semi)) {
            self.bump();
        }
        Ok((decls, body))
    }

    /// Parse declarations until `BEGIN`.
    fn parse_decls(&mut self) -> Result<Vec<Decl>, String> {
        let mut decls = Vec::new();
        while !self.is_word("begin") {
            if self.peek().is_none() {
                return Err("unexpected end of plpgsql body in DECLARE section".into());
            }
            let name = match self.bump().map(|t| t.kind.clone()) {
                Some(TokKind::Word(w)) => w,
                other => return Err(format!("expected variable name, found {other:?}")),
            };
            // The type name: a run of tokens until `:=` or `;`.
            let mut type_text = String::new();
            while !matches!(self.peek().map(|t| &t.kind), Some(TokKind::Assign) | Some(TokKind::Semi))
            {
                match self.peek() {
                    Some(_) => {
                        let frag = self.span_text();
                        if !type_text.is_empty() {
                            type_text.push(' ');
                        }
                        type_text.push_str(frag.trim());
                        self.bump();
                    }
                    None => return Err("malformed type in DECLARE".into()),
                }
            }
            let ty = DataType::from_sql_name(type_text.trim());
            let init = if matches!(self.peek().map(|t| &t.kind), Some(TokKind::Assign)) {
                self.bump();
                Some(self.parse_expr_until(|p| {
                    matches!(p.peek().map(|t| &t.kind), Some(TokKind::Semi) | None)
                })?)
            } else {
                None
            };
            self.expect_semi()?;
            decls.push(Decl {
                name: name.to_ascii_lowercase(),
                ty,
                init,
            });
        }
        Ok(decls)
    }

    /// Source text of the current token.
    fn span_text(&self) -> &str {
        match self.peek() {
            Some(t) => &self.body[t.start..t.end],
            None => "",
        }
    }

    /// Parse statements until one of `terminators` keywords is next (left
    /// unconsumed).
    fn parse_stmts_until(&mut self, terminators: &[&str]) -> Result<Vec<PlStmt>, String> {
        let mut out = Vec::new();
        loop {
            match self.peek().map(|t| &t.kind) {
                None => return Err("unexpected end of plpgsql body".into()),
                Some(TokKind::Word(w))
                    if terminators.iter().any(|t| w.eq_ignore_ascii_case(t)) =>
                {
                    break;
                }
                Some(TokKind::Semi) => {
                    self.bump();
                }
                _ => out.push(self.parse_stmt()?),
            }
        }
        Ok(out)
    }

    fn parse_stmt(&mut self) -> Result<PlStmt, String> {
        if self.eat_word("return") {
            let e = self.parse_expr_until(|p| {
                matches!(p.peek().map(|t| &t.kind), Some(TokKind::Semi) | None)
            })?;
            self.expect_semi()?;
            return Ok(PlStmt::Return(e));
        }
        if self.eat_word("raise") {
            return self.parse_raise();
        }
        if self.eat_word("if") {
            return self.parse_if();
        }
        if self.eat_word("while") {
            return self.parse_while();
        }
        if self.eat_word("for") {
            return self.parse_for();
        }
        if self.eat_word("select") {
            return self.parse_select_into();
        }
        // Otherwise: assignment `var := expr;`.
        let var = match self.bump().map(|t| t.kind.clone()) {
            Some(TokKind::Word(w)) => w.to_ascii_lowercase(),
            other => return Err(format!("unexpected token in plpgsql statement: {other:?}")),
        };
        match self.bump().map(|t| &t.kind) {
            Some(TokKind::Assign) => {}
            other => {
                return Err(format!(
                    "expected `:=` after `{var}` in plpgsql assignment, found {other:?}"
                ))
            }
        }
        let expr = self.parse_expr_until(|p| {
            matches!(p.peek().map(|t| &t.kind), Some(TokKind::Semi) | None)
        })?;
        self.expect_semi()?;
        Ok(PlStmt::Assign { var, expr })
    }

    fn parse_raise(&mut self) -> Result<PlStmt, String> {
        let mut exception = false;
        if self.eat_word("exception") {
            exception = true;
        } else {
            for lvl in ["notice", "warning", "info", "log", "debug"] {
                if self.eat_word(lvl) {
                    break;
                }
            }
        }
        let message = match self.peek().map(|t| t.kind.clone()) {
            Some(TokKind::Str(s)) => {
                self.bump();
                s
            }
            Some(TokKind::Semi) => {
                if exception {
                    "raised exception".to_string()
                } else {
                    String::new()
                }
            }
            other => return Err(format!("expected message after RAISE, found {other:?}")),
        };
        // Discard trailing format arguments / USING clause up to `;`.
        while !matches!(self.peek().map(|t| &t.kind), Some(TokKind::Semi) | None) {
            self.bump();
        }
        self.expect_semi()?;
        Ok(PlStmt::Raise { exception, message })
    }

    fn parse_if(&mut self) -> Result<PlStmt, String> {
        let mut branches = Vec::new();
        let cond = self.parse_expr_until(|p| p.is_word("then") || p.peek().is_none())?;
        self.expect_word("then")?;
        let body = self.parse_stmts_until(&["elsif", "elseif", "else", "end"])?;
        branches.push((cond, body));
        while self.eat_word("elsif") || self.eat_word("elseif") {
            let c = self.parse_expr_until(|p| p.is_word("then") || p.peek().is_none())?;
            self.expect_word("then")?;
            let b = self.parse_stmts_until(&["elsif", "elseif", "else", "end"])?;
            branches.push((c, b));
        }
        let else_body = if self.eat_word("else") {
            self.parse_stmts_until(&["end"])?
        } else {
            Vec::new()
        };
        self.expect_word("end")?;
        self.expect_word("if")?;
        self.expect_semi()?;
        Ok(PlStmt::If {
            branches,
            else_body,
        })
    }

    fn parse_while(&mut self) -> Result<PlStmt, String> {
        let cond = self.parse_expr_until(|p| p.is_word("loop") || p.peek().is_none())?;
        self.expect_word("loop")?;
        let body = self.parse_stmts_until(&["end"])?;
        self.expect_word("end")?;
        self.expect_word("loop")?;
        self.expect_semi()?;
        Ok(PlStmt::While { cond, body })
    }

    fn parse_for(&mut self) -> Result<PlStmt, String> {
        let var = match self.bump().map(|t| t.kind.clone()) {
            Some(TokKind::Word(w)) => w.to_ascii_lowercase(),
            other => return Err(format!("expected loop variable in FOR, found {other:?}")),
        };
        self.expect_word("in")?;
        if self.is_word("reverse") {
            return Err("FOR ... REVERSE is not supported in plpgsql functions here".into());
        }
        let from = self.parse_expr_until(|p| {
            matches!(p.peek().map(|t| &t.kind), Some(TokKind::DotDot) | None)
        })?;
        match self.bump().map(|t| &t.kind) {
            Some(TokKind::DotDot) => {}
            other => return Err(format!("expected `..` in FOR range, found {other:?}")),
        }
        let to = self.parse_expr_until(|p| p.is_word("loop") || p.peek().is_none())?;
        self.expect_word("loop")?;
        let body = self.parse_stmts_until(&["end"])?;
        self.expect_word("end")?;
        self.expect_word("loop")?;
        self.expect_semi()?;
        Ok(PlStmt::For {
            var,
            from,
            to,
            body,
        })
    }

    fn parse_select_into(&mut self) -> Result<PlStmt, String> {
        // `SELECT <expr> INTO <var>;` — expression text up to INTO.
        let expr = self.parse_expr_until(|p| p.is_word("into") || p.peek().is_none())?;
        self.expect_word("into")?;
        let var = match self.bump().map(|t| t.kind.clone()) {
            Some(TokKind::Word(w)) => w.to_ascii_lowercase(),
            other => return Err(format!("expected target variable after INTO, found {other:?}")),
        };
        if self.is_word("from") {
            return Err(
                "SELECT INTO from a table is not supported in plpgsql functions here".into(),
            );
        }
        self.expect_semi()?;
        Ok(PlStmt::Assign { var, expr })
    }

    /// Consume tokens until `stop` returns true, slicing the original source
    /// between the first and last consumed token, then parse it as an SQL
    /// expression. Preserves the exact source spelling of operators/numbers.
    fn parse_expr_until(&mut self, stop: impl Fn(&PlParser) -> bool) -> Result<Expr, String> {
        if stop(self) {
            return Err("empty expression in plpgsql body".into());
        }
        let start = self.peek().map(|t| t.start).unwrap_or(0);
        let mut end = start;
        while !stop(self) {
            match self.bump() {
                Some(t) => end = t.end,
                None => break,
            }
        }
        parse_expr_text(&self.body[start..end])
    }
}
