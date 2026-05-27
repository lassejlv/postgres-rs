//! Recursive-descent SQL parser with precedence climbing for expressions.

use super::ast::*;
use super::lexer::{Lexer, Token};
use crate::types::DataType;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    /// Parse a SQL string into a sequence of statements (separated by `;`).
    pub fn parse_sql(sql: &str) -> Result<Vec<Statement>, String> {
        let tokens = Lexer::new(sql).tokenize()?;
        let mut parser = Parser { tokens, pos: 0 };
        let mut stmts = Vec::new();
        loop {
            // Consume any stray statement separators.
            while parser.eat(&Token::Semicolon) {}
            if parser.peek().is_none() {
                break;
            }
            stmts.push(parser.parse_statement()?);
            // A statement must be followed by `;` or end-of-input.
            if !parser.eat(&Token::Semicolon) && parser.peek().is_some() {
                return Err(format!("unexpected token after statement: {:?}", parser.peek()));
            }
        }
        if stmts.is_empty() {
            stmts.push(Statement::Empty);
        }
        Ok(stmts)
    }

    // --- token cursor helpers ------------------------------------------------

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, t: &Token) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Token) -> Result<(), String> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(format!("expected {:?}, found {:?}", t, self.peek()))
        }
    }

    /// Match the next token if it is a keyword equal to `kw` (case-insensitive).
    fn eat_keyword(&mut self, kw: &str) -> bool {
        if let Some(Token::Word(w)) = self.peek() {
            if w.eq_ignore_ascii_case(kw) {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    /// Peek whether the next token is the given keyword, without consuming.
    fn is_keyword(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(w)) if w.eq_ignore_ascii_case(kw))
    }

    /// Peek whether the token `n` ahead is the given keyword.
    fn is_keyword_at(&self, n: usize, kw: &str) -> bool {
        matches!(self.tokens.get(self.pos + n), Some(Token::Word(w)) if w.eq_ignore_ascii_case(kw))
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), String> {
        if self.eat_keyword(kw) {
            Ok(())
        } else {
            Err(format!("expected keyword `{}`, found {:?}", kw, self.peek()))
        }
    }

    /// Parse an identifier (bare word or quoted). Keywords are accepted as
    /// identifiers in contexts where the grammar permits it.
    fn parse_ident(&mut self) -> Result<String, String> {
        match self.advance() {
            Some(Token::Word(w)) => Ok(w),
            Some(Token::QuotedIdent(w)) => Ok(w),
            other => Err(format!("expected identifier, found {other:?}")),
        }
    }

    /// Parse a possibly schema-qualified name, returning the final component.
    /// We don't model schemas yet, so `public.t` collapses to `t`.
    fn parse_object_name(&mut self) -> Result<String, String> {
        let mut name = self.parse_ident()?;
        while self.eat(&Token::Dot) {
            name = self.parse_ident()?;
        }
        Ok(name)
    }

    // --- statements ----------------------------------------------------------

    fn parse_statement(&mut self) -> Result<Statement, String> {
        // Look at the leading keyword to dispatch.
        let Some(Token::Word(w)) = self.peek() else {
            return Err(format!("expected a statement, found {:?}", self.peek()));
        };
        let kw = w.to_ascii_lowercase();
        match kw.as_str() {
            "create" => self.parse_create(),
            "drop" => self.parse_drop(),
            "insert" => self.parse_insert(),
            "select" => Ok(Statement::Select(self.parse_select()?)),
            "update" => self.parse_update(),
            "delete" => self.parse_delete(),
            "begin" | "start" => {
                self.advance();
                // `START TRANSACTION` / `BEGIN [TRANSACTION|WORK]` — swallow rest.
                self.eat_keyword("transaction");
                self.eat_keyword("work");
                Ok(Statement::Begin)
            }
            "commit" | "end" => {
                self.advance();
                self.eat_keyword("transaction");
                self.eat_keyword("work");
                Ok(Statement::Commit)
            }
            "rollback" | "abort" => {
                self.advance();
                self.eat_keyword("transaction");
                self.eat_keyword("work");
                Ok(Statement::Rollback)
            }
            "set" => self.parse_set(),
            "show" => {
                self.advance();
                let name = self.parse_ident()?;
                Ok(Statement::Show { name })
            }
            other => Err(format!("unsupported statement: `{other}`")),
        }
    }

    fn parse_create(&mut self) -> Result<Statement, String> {
        self.expect_keyword("create")?;
        self.expect_keyword("table")?;
        let if_not_exists = self.parse_if_not_exists();
        let name = self.parse_object_name()?;
        self.expect(&Token::LParen)?;

        let mut columns = Vec::new();
        loop {
            // Skip standalone table constraints like `PRIMARY KEY (...)`.
            if self.is_keyword("primary")
                || self.is_keyword("unique")
                || self.is_keyword("constraint")
                || self.is_keyword("foreign")
                || self.is_keyword("check")
            {
                self.skip_balanced_until_comma_or_rparen();
            } else {
                columns.push(self.parse_column_def()?);
            }
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Token::RParen)?;
        if columns.is_empty() {
            return Err("table must have at least one column".to_string());
        }
        Ok(Statement::CreateTable(CreateTable { name, columns, if_not_exists }))
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef, String> {
        let name = self.parse_ident()?;
        let (data_type, serial) = self.parse_column_type()?;
        // `serial` types are implicitly NOT NULL with a sequence default.
        let mut not_null = serial;
        let mut primary_key = false;
        let mut default = None;
        loop {
            if self.eat_keyword("primary") {
                self.expect_keyword("key")?;
                primary_key = true;
                not_null = true;
            } else if self.eat_keyword("not") {
                self.expect_keyword("null")?;
                not_null = true;
            } else if self.eat_keyword("null") {
                // explicit nullable
            } else if self.eat_keyword("unique") {
                // accepted, not yet enforced
            } else if self.eat_keyword("default") {
                default = Some(self.parse_expr()?);
            } else {
                break;
            }
        }
        Ok(ColumnDef { name, data_type, not_null, primary_key, default, serial })
    }

    /// Parse a column type, recognizing the `serial` family as auto-increment
    /// integer types. Returns the underlying type and whether it is serial.
    fn parse_column_type(&mut self) -> Result<(DataType, bool), String> {
        if self.is_keyword("serial") || self.is_keyword("serial4") {
            self.advance();
            return Ok((DataType::Int4, true));
        }
        if self.is_keyword("bigserial") || self.is_keyword("serial8") {
            self.advance();
            return Ok((DataType::Int8, true));
        }
        if self.is_keyword("smallserial") || self.is_keyword("serial2") {
            self.advance();
            return Ok((DataType::Int2, true));
        }
        Ok((self.parse_data_type()?, false))
    }

    fn parse_data_type(&mut self) -> Result<DataType, String> {
        let mut name = self.parse_ident()?.to_ascii_lowercase();
        // Schema-qualified type (e.g. `pg_catalog.regtype`): keep the type name.
        if self.eat(&Token::Dot) {
            name = self.parse_ident()?.to_ascii_lowercase();
        }
        // Multi-word types like `double precision` / `character varying`.
        if (name == "double" && self.is_keyword("precision"))
            || (name == "character" && self.is_keyword("varying"))
        {
            let second = self.parse_ident()?.to_ascii_lowercase();
            name = format!("{name} {second}");
        } else if (name == "timestamp" || name == "time")
            && (self.is_keyword("with") || self.is_keyword("without"))
        {
            // `timestamp/time [with|without] time zone`.
            let wo = self.parse_ident()?.to_ascii_lowercase();
            self.eat_keyword("time");
            self.eat_keyword("zone");
            name = format!("{name} {wo} time zone");
        }
        // Optional length/precision modifier, e.g. `varchar(255)`, `numeric(10,2)`.
        if self.eat(&Token::LParen) {
            while !self.eat(&Token::RParen) {
                if self.advance().is_none() {
                    return Err("unterminated type modifier".to_string());
                }
            }
        }
        // Unknown types map to text (PostgreSQL has hundreds; this keeps casts
        // like `::regclass` and unusual column types working).
        Ok(DataType::from_sql_name(&name).unwrap_or(DataType::Text))
    }

    fn parse_drop(&mut self) -> Result<Statement, String> {
        self.expect_keyword("drop")?;
        self.expect_keyword("table")?;
        let if_exists = self.parse_if_exists();
        let name = self.parse_object_name()?;
        // Swallow trailing `CASCADE`/`RESTRICT`.
        self.eat_keyword("cascade");
        self.eat_keyword("restrict");
        Ok(Statement::DropTable(DropTable { name, if_exists }))
    }

    fn parse_insert(&mut self) -> Result<Statement, String> {
        self.expect_keyword("insert")?;
        self.expect_keyword("into")?;
        let table = self.parse_object_name()?;

        let columns = if self.peek() == Some(&Token::LParen) {
            self.advance();
            let mut cols = Vec::new();
            loop {
                cols.push(self.parse_ident()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
            self.expect(&Token::RParen)?;
            Some(cols)
        } else {
            None
        };

        self.expect_keyword("values")?;
        let mut rows = Vec::new();
        loop {
            self.expect(&Token::LParen)?;
            let mut tuple = Vec::new();
            if self.peek() != Some(&Token::RParen) {
                loop {
                    tuple.push(self.parse_expr()?);
                    if self.eat(&Token::Comma) {
                        continue;
                    }
                    break;
                }
            }
            self.expect(&Token::RParen)?;
            rows.push(tuple);
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        let returning = self.parse_returning()?;
        Ok(Statement::Insert(Insert { table, columns, rows, returning }))
    }

    /// Parse a comma-separated select list: `*` or `expr [AS alias]`.
    /// Shared by `SELECT` and `RETURNING`.
    fn parse_select_list(&mut self) -> Result<Vec<SelectItem>, String> {
        let mut projection = Vec::new();
        loop {
            if self.peek() == Some(&Token::Star) {
                self.advance();
                projection.push(SelectItem::Wildcard);
            } else {
                let expr = self.parse_expr()?;
                let alias = if self.eat_keyword("as") {
                    Some(self.parse_ident()?)
                } else if let Some(Token::Word(w)) = self.peek() {
                    // Bare alias, as long as it isn't a clause keyword.
                    if is_select_clause_keyword(w) {
                        None
                    } else {
                        Some(self.parse_ident()?)
                    }
                } else {
                    None
                };
                projection.push(SelectItem::Expr { expr, alias });
            }
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        Ok(projection)
    }

    /// Parse an optional trailing `RETURNING <select-list>`.
    fn parse_returning(&mut self) -> Result<Vec<SelectItem>, String> {
        if self.eat_keyword("returning") {
            self.parse_select_list()
        } else {
            Ok(Vec::new())
        }
    }

    fn parse_select(&mut self) -> Result<Select, String> {
        self.expect_keyword("select")?;
        // `ALL` is the default; `DISTINCT` deduplicates.
        self.eat_keyword("all");
        let distinct = self.eat_keyword("distinct");

        let projection = self.parse_select_list()?;

        let from = if self.eat_keyword("from") {
            Some(self.parse_from_clause()?)
        } else {
            None
        };

        let filter = if self.eat_keyword("where") {
            Some(self.parse_expr()?)
        } else {
            None
        };

        let mut group_by = Vec::new();
        if self.eat_keyword("group") {
            self.expect_keyword("by")?;
            loop {
                group_by.push(self.parse_expr()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
        }

        let having = if self.eat_keyword("having") {
            Some(self.parse_expr()?)
        } else {
            None
        };

        let mut order_by = Vec::new();
        if self.eat_keyword("order") {
            self.expect_keyword("by")?;
            loop {
                let expr = self.parse_expr()?;
                let asc = if self.eat_keyword("desc") {
                    false
                } else {
                    self.eat_keyword("asc");
                    true
                };
                order_by.push(OrderByItem { expr, asc });
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
        }

        // LIMIT / OFFSET in either order.
        let mut limit = None;
        let mut offset = None;
        loop {
            if limit.is_none() && self.eat_keyword("limit") {
                limit = Some(self.parse_expr()?);
            } else if offset.is_none() && self.eat_keyword("offset") {
                offset = Some(self.parse_expr()?);
                self.eat_keyword("row");
                self.eat_keyword("rows");
            } else {
                break;
            }
        }

        Ok(Select { distinct, projection, from, filter, group_by, having, order_by, limit, offset })
    }

    /// Parse `FROM base [alias] (JOIN ...)*`.
    fn parse_from_clause(&mut self) -> Result<FromClause, String> {
        let base = self.parse_table_ref()?;
        let mut joins = Vec::new();
        loop {
            // Determine join kind from optional leading keyword.
            let kind = if self.eat_keyword("inner") {
                self.expect_keyword("join")?;
                JoinKind::Inner
            } else if self.eat_keyword("left") {
                self.eat_keyword("outer");
                self.expect_keyword("join")?;
                JoinKind::Left
            } else if self.eat_keyword("right") {
                self.eat_keyword("outer");
                self.expect_keyword("join")?;
                JoinKind::Right
            } else if self.eat_keyword("full") {
                self.eat_keyword("outer");
                self.expect_keyword("join")?;
                JoinKind::Full
            } else if self.eat_keyword("cross") {
                self.expect_keyword("join")?;
                JoinKind::Cross
            } else if self.eat_keyword("join") {
                JoinKind::Inner
            } else {
                break;
            };
            let table = self.parse_table_ref()?;
            // CROSS JOIN takes no ON clause; all others require one.
            let on = if kind == JoinKind::Cross {
                None
            } else {
                self.expect_keyword("on")?;
                Some(self.parse_expr()?)
            };
            joins.push(Join { kind, table, on });
        }
        Ok(FromClause { base, joins })
    }

    /// Parse a table name with an optional alias (`t` / `t a` / `t AS a`),
    /// preserving any schema qualifier (`information_schema.tables`).
    fn parse_table_ref(&mut self) -> Result<TableRef, String> {
        let (schema, name) = self.parse_qualified_name()?;
        let alias = if self.eat_keyword("as") {
            Some(self.parse_ident()?)
        } else if let Some(Token::Word(w)) = self.peek() {
            // A bare alias, unless it's a keyword that continues the query.
            if is_table_ref_keyword(w) {
                None
            } else {
                Some(self.parse_ident()?)
            }
        } else {
            None
        };
        Ok(TableRef { schema, name, alias })
    }

    /// Parse a dotted name, returning `(schema, name)` where `schema` is the
    /// second-to-last component (catalog/database prefixes are dropped).
    fn parse_qualified_name(&mut self) -> Result<(Option<String>, String), String> {
        let mut name = self.parse_ident()?;
        let mut schema = None;
        while self.eat(&Token::Dot) {
            schema = Some(name);
            name = self.parse_ident()?;
        }
        Ok((schema, name))
    }

    fn parse_update(&mut self) -> Result<Statement, String> {
        self.expect_keyword("update")?;
        let table = self.parse_object_name()?;
        self.expect_keyword("set")?;
        let mut assignments = Vec::new();
        loop {
            let col = self.parse_ident()?;
            self.expect(&Token::Eq)?;
            let val = self.parse_expr()?;
            assignments.push((col, val));
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        let filter = if self.eat_keyword("where") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let returning = self.parse_returning()?;
        Ok(Statement::Update(Update { table, assignments, filter, returning }))
    }

    fn parse_delete(&mut self) -> Result<Statement, String> {
        self.expect_keyword("delete")?;
        self.expect_keyword("from")?;
        let table = self.parse_object_name()?;
        let filter = if self.eat_keyword("where") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let returning = self.parse_returning()?;
        Ok(Statement::Delete(Delete { table, filter, returning }))
    }

    fn parse_set(&mut self) -> Result<Statement, String> {
        self.expect_keyword("set")?;
        // `SET [SESSION|LOCAL] name {=|TO} value`.
        self.eat_keyword("session");
        self.eat_keyword("local");
        let name = self.parse_object_name()?;
        let _ = self.eat(&Token::Eq) || self.eat_keyword("to");
        // Capture a textual value (best-effort); consume one expression.
        let value = match self.parse_expr() {
            Ok(e) => format!("{e:?}"),
            Err(_) => {
                // Some values are bare words like `SET client_min_messages TO warning`.
                self.advance();
                String::new()
            }
        };
        Ok(Statement::Set { name, value })
    }

    // --- helpers for optional clauses ---------------------------------------

    fn parse_if_not_exists(&mut self) -> bool {
        if self.is_keyword("if") {
            self.advance();
            let _ = self.eat_keyword("not") && self.eat_keyword("exists");
            true
        } else {
            false
        }
    }

    fn parse_if_exists(&mut self) -> bool {
        if self.is_keyword("if") {
            self.advance();
            self.eat_keyword("exists");
            true
        } else {
            false
        }
    }

    /// Skip tokens (balancing parens) until a top-level comma or `)`.
    /// Used to ignore table-level constraints we don't model yet.
    fn skip_balanced_until_comma_or_rparen(&mut self) {
        let mut depth = 0;
        loop {
            match self.peek() {
                None => break,
                Some(Token::LParen) => {
                    depth += 1;
                    self.advance();
                }
                Some(Token::RParen) => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                    self.advance();
                }
                Some(Token::Comma) if depth == 0 => break,
                _ => {
                    self.advance();
                }
            }
        }
    }

    // --- expressions (precedence climbing) -----------------------------------

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_expr_bp(0)
    }

    /// Parse an expression with binding power at least `min_bp`.
    fn parse_expr_bp(&mut self, min_bp: u8) -> Result<Expr, String> {
        let mut lhs = self.parse_prefix()?;

        loop {
            // Postfix: IS [NOT] NULL.
            if self.is_keyword("is") {
                let (l_bp, _) = (7, 7);
                if l_bp < min_bp {
                    break;
                }
                self.advance();
                let negated = self.eat_keyword("not");
                self.expect_keyword("null")?;
                lhs = Expr::IsNull { expr: Box::new(lhs), negated };
                continue;
            }

            // Postfix predicates LIKE / ILIKE / IN / BETWEEN, optionally with a
            // leading NOT, all at comparison precedence.
            if self.peek_predicate_keyword().is_some() {
                if 5 < min_bp {
                    break;
                }
                lhs = self.parse_predicate(lhs)?;
                continue;
            }

            // The `OPERATOR(schema.op)` construct, treated as the op itself.
            if self.is_keyword("operator") {
                if 5 < min_bp {
                    break;
                }
                let op = self.parse_operator_construct()?;
                let rhs = self.parse_expr_bp(6)?;
                lhs = Expr::Binary { op, left: Box::new(lhs), right: Box::new(rhs) };
                continue;
            }

            let Some(op) = self.peek_binary_op() else {
                break;
            };
            let (l_bp, r_bp) = binding_power(op);
            if l_bp < min_bp {
                break;
            }
            self.advance_binary_op();
            let rhs = self.parse_expr_bp(r_bp)?;
            lhs = Expr::Binary { op, left: Box::new(lhs), right: Box::new(rhs) };
        }

        Ok(lhs)
    }

    fn parse_prefix(&mut self) -> Result<Expr, String> {
        // Unary operators.
        if self.eat_keyword("not") {
            let e = self.parse_expr_bp(3)?;
            return Ok(Expr::Unary { op: UnaryOp::Not, expr: Box::new(e) });
        }
        if self.peek() == Some(&Token::Minus) {
            self.advance();
            let e = self.parse_expr_bp(9)?;
            return Ok(Expr::Unary { op: UnaryOp::Neg, expr: Box::new(e) });
        }
        if self.peek() == Some(&Token::Plus) {
            self.advance();
            return self.parse_expr_bp(9);
        }

        self.parse_atom()
    }

    /// Parse an atom, then consume any trailing `::type` casts or `COLLATE`
    /// clauses (tightest binding, so they attach to the atom).
    fn parse_atom(&mut self) -> Result<Expr, String> {
        let mut e = self.parse_atom_inner()?;
        loop {
            if self.eat(&Token::DoubleColon) {
                let target = self.parse_data_type()?;
                e = Expr::Cast { expr: Box::new(e), target };
            } else if self.eat_keyword("collate") {
                // Collation is accepted and ignored.
                let _ = self.parse_qualified_name()?;
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn parse_atom_inner(&mut self) -> Result<Expr, String> {
        match self.advance() {
            Some(Token::Number(n)) => {
                if n.contains('.') || n.contains('e') || n.contains('E') {
                    n.parse::<f64>()
                        .map(Expr::Float)
                        .map_err(|_| format!("invalid number `{n}`"))
                } else {
                    match n.parse::<i64>() {
                        Ok(i) => Ok(Expr::Int(i)),
                        // Out-of-range integers fall back to float.
                        Err(_) => n
                            .parse::<f64>()
                            .map(Expr::Float)
                            .map_err(|_| format!("invalid number `{n}`")),
                    }
                }
            }
            Some(Token::StringLit(s)) => Ok(Expr::Str(s)),
            Some(Token::Param(n)) => Ok(Expr::Param(n)),
            Some(Token::QuotedIdent(name)) => Ok(Expr::Column(name)),
            Some(Token::LParen) => {
                let e = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                Ok(e)
            }
            Some(Token::Word(w)) => {
                let lw = w.to_ascii_lowercase();
                match lw.as_str() {
                    "true" => Ok(Expr::Bool(true)),
                    "false" => Ok(Expr::Bool(false)),
                    "null" => Ok(Expr::Null),
                    "case" => self.parse_case(),
                    "cast" => {
                        self.expect(&Token::LParen)?;
                        let inner = self.parse_expr()?;
                        self.expect_keyword("as")?;
                        let target = self.parse_data_type()?;
                        self.expect(&Token::RParen)?;
                        Ok(Expr::Cast { expr: Box::new(inner), target })
                    }
                    // Niladic SQL functions usable without parentheses.
                    "current_user" | "current_role" | "session_user" | "current_schema"
                    | "current_catalog" | "current_date" | "current_timestamp"
                        if self.peek() != Some(&Token::LParen) =>
                    {
                        Ok(Expr::Function { name: lw, args: Vec::new(), star: false })
                    }
                    _ => {
                        // Function call?
                        if self.peek() == Some(&Token::LParen) {
                            self.advance();
                            self.parse_function_args(w)
                        } else if self.eat(&Token::Dot) {
                            // Qualified column `table.col`, or a schema-qualified
                            // function call `schema.func(...)`.
                            let col = self.parse_ident()?;
                            if self.eat(&Token::Dot) {
                                // Three-part: schema.table.col or schema.x.func(...)
                                let col2 = self.parse_ident()?;
                                if self.peek() == Some(&Token::LParen) {
                                    self.advance();
                                    self.parse_function_args(col2)
                                } else {
                                    Ok(Expr::QualifiedColumn { qualifier: col, name: col2 })
                                }
                            } else if self.peek() == Some(&Token::LParen) {
                                // Two-part function call `schema.func(...)`.
                                self.advance();
                                self.parse_function_args(col)
                            } else {
                                Ok(Expr::QualifiedColumn { qualifier: w, name: col })
                            }
                        } else {
                            Ok(Expr::Column(w))
                        }
                    }
                }
            }
            other => Err(format!("unexpected token in expression: {other:?}")),
        }
    }

    fn parse_function_args(&mut self, name: String) -> Result<Expr, String> {
        // `count(*)` special case.
        if self.peek() == Some(&Token::Star) {
            self.advance();
            self.expect(&Token::RParen)?;
            return Ok(Expr::Function { name, args: Vec::new(), star: true });
        }
        let mut args = Vec::new();
        if self.peek() != Some(&Token::RParen) {
            // Accept and ignore `DISTINCT` inside aggregates.
            self.eat_keyword("distinct");
            loop {
                args.push(self.parse_expr()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&Token::RParen)?;
        Ok(Expr::Function { name, args, star: false })
    }

    /// If a `[NOT] LIKE/ILIKE/IN/BETWEEN` predicate begins here, return its
    /// keyword (the bare one, ignoring a leading `NOT`).
    fn peek_predicate_keyword(&self) -> Option<&'static str> {
        // Skip a leading NOT when followed by a predicate keyword.
        let offset = if self.is_keyword("not") { 1 } else { 0 };
        for kw in ["like", "ilike", "in", "between"] {
            if self.is_keyword_at(offset, kw) {
                return Some(match kw {
                    "like" => "like",
                    "ilike" => "ilike",
                    "in" => "in",
                    _ => "between",
                });
            }
        }
        None
    }

    /// Parse a postfix predicate (`LIKE`/`ILIKE`/`IN`/`BETWEEN`) given the
    /// already-parsed left operand.
    fn parse_predicate(&mut self, lhs: Expr) -> Result<Expr, String> {
        let negated = self.eat_keyword("not");
        // Operands bind tighter than comparison/AND so e.g. BETWEEN's `AND`
        // and a trailing boolean `AND` are not swallowed.
        const OPERAND_BP: u8 = 7;
        if self.eat_keyword("like") || self.eat_keyword("ilike") {
            // Re-check which one we consumed: look back one token.
            let case_insensitive = matches!(
                self.tokens.get(self.pos - 1),
                Some(Token::Word(w)) if w.eq_ignore_ascii_case("ilike")
            );
            let pattern = self.parse_expr_bp(OPERAND_BP)?;
            Ok(Expr::Like {
                expr: Box::new(lhs),
                pattern: Box::new(pattern),
                negated,
                case_insensitive,
            })
        } else if self.eat_keyword("between") {
            let low = self.parse_expr_bp(OPERAND_BP)?;
            self.expect_keyword("and")?;
            let high = self.parse_expr_bp(OPERAND_BP)?;
            Ok(Expr::Between {
                expr: Box::new(lhs),
                low: Box::new(low),
                high: Box::new(high),
                negated,
            })
        } else if self.eat_keyword("in") {
            self.expect(&Token::LParen)?;
            let mut list = Vec::new();
            if self.peek() != Some(&Token::RParen) {
                loop {
                    list.push(self.parse_expr()?);
                    if self.eat(&Token::Comma) {
                        continue;
                    }
                    break;
                }
            }
            self.expect(&Token::RParen)?;
            Ok(Expr::InList { expr: Box::new(lhs), list, negated })
        } else {
            Err(format!("expected a predicate after operand, found {:?}", self.peek()))
        }
    }

    /// Parse a `CASE ... END` expression.
    fn parse_case(&mut self) -> Result<Expr, String> {
        // `case` keyword already consumed by the caller.
        let operand = if self.is_keyword("when") {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };
        let mut whens = Vec::new();
        while self.eat_keyword("when") {
            let cond = self.parse_expr()?;
            self.expect_keyword("then")?;
            let result = self.parse_expr()?;
            whens.push((cond, result));
        }
        if whens.is_empty() {
            return Err("CASE requires at least one WHEN clause".to_string());
        }
        let else_expr = if self.eat_keyword("else") {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect_keyword("end")?;
        Ok(Expr::Case { operand, whens, else_expr })
    }

    /// Parse `OPERATOR ( [schema.] <op> )` and return the equivalent binary op.
    fn parse_operator_construct(&mut self) -> Result<BinaryOp, String> {
        self.expect_keyword("operator")?;
        self.expect(&Token::LParen)?;
        // Optional schema qualifier, e.g. `pg_catalog.`.
        if matches!(self.peek(), Some(Token::Word(_)))
            && self.tokens.get(self.pos + 1) == Some(&Token::Dot)
        {
            self.advance();
            self.advance();
        }
        let op = match self.advance() {
            Some(Token::Match) => BinaryOp::RegexMatch { ci: false },
            Some(Token::MatchCi) => BinaryOp::RegexMatch { ci: true },
            Some(Token::NotMatch) => BinaryOp::RegexNotMatch { ci: false },
            Some(Token::NotMatchCi) => BinaryOp::RegexNotMatch { ci: true },
            Some(Token::Eq) => BinaryOp::Eq,
            Some(Token::NotEq) => BinaryOp::NotEq,
            Some(Token::Lt) => BinaryOp::Lt,
            Some(Token::LtEq) => BinaryOp::LtEq,
            Some(Token::Gt) => BinaryOp::Gt,
            Some(Token::GtEq) => BinaryOp::GtEq,
            other => return Err(format!("unsupported OPERATOR(...): {other:?}")),
        };
        self.expect(&Token::RParen)?;
        Ok(op)
    }

    /// Peek the current token as a binary operator, including keyword operators.
    fn peek_binary_op(&self) -> Option<BinaryOp> {
        match self.peek()? {
            Token::Plus => Some(BinaryOp::Add),
            Token::Minus => Some(BinaryOp::Sub),
            Token::Star => Some(BinaryOp::Mul),
            Token::Slash => Some(BinaryOp::Div),
            Token::Percent => Some(BinaryOp::Mod),
            Token::Concat => Some(BinaryOp::Concat),
            Token::Eq => Some(BinaryOp::Eq),
            Token::NotEq => Some(BinaryOp::NotEq),
            Token::Lt => Some(BinaryOp::Lt),
            Token::LtEq => Some(BinaryOp::LtEq),
            Token::Gt => Some(BinaryOp::Gt),
            Token::GtEq => Some(BinaryOp::GtEq),
            Token::Match => Some(BinaryOp::RegexMatch { ci: false }),
            Token::MatchCi => Some(BinaryOp::RegexMatch { ci: true }),
            Token::NotMatch => Some(BinaryOp::RegexNotMatch { ci: false }),
            Token::NotMatchCi => Some(BinaryOp::RegexNotMatch { ci: true }),
            Token::Word(w) if w.eq_ignore_ascii_case("and") => Some(BinaryOp::And),
            Token::Word(w) if w.eq_ignore_ascii_case("or") => Some(BinaryOp::Or),
            _ => None,
        }
    }

    /// Consume the binary operator token previously seen by `peek_binary_op`.
    fn advance_binary_op(&mut self) {
        self.advance();
    }
}

/// Operator binding powers. Higher binds tighter. Returned as `(left, right)`;
/// a right-bp greater than left-bp would make an operator right-associative.
fn binding_power(op: BinaryOp) -> (u8, u8) {
    match op {
        BinaryOp::Or => (1, 2),
        BinaryOp::And => (3, 4),
        // (NOT handled as a prefix at bp 3.)
        BinaryOp::Eq
        | BinaryOp::NotEq
        | BinaryOp::Lt
        | BinaryOp::LtEq
        | BinaryOp::Gt
        | BinaryOp::GtEq
        | BinaryOp::RegexMatch { .. }
        | BinaryOp::RegexNotMatch { .. } => (5, 6),
        BinaryOp::Concat => (8, 9),
        BinaryOp::Add | BinaryOp::Sub => (10, 11),
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => (12, 13),
    }
}

/// Keywords that begin a clause after the SELECT projection, so a bare word
/// here is not an alias.
fn is_select_clause_keyword(w: &str) -> bool {
    const KW: &[&str] = &[
        "from", "where", "order", "limit", "offset", "group", "having", "as", "and", "or",
    ];
    KW.iter().any(|k| w.eq_ignore_ascii_case(k))
}

/// Keywords that may follow a table reference, so a bare word here is a clause
/// rather than a table alias.
fn is_table_ref_keyword(w: &str) -> bool {
    const KW: &[&str] = &[
        "where", "order", "limit", "offset", "group", "having", "join", "inner", "left", "right",
        "full", "cross", "outer", "on", "union", "as",
    ];
    KW.iter().any(|k| w.eq_ignore_ascii_case(k))
}
