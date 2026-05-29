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
                return Err(format!(
                    "unexpected token after statement: {:?}",
                    parser.peek()
                ));
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

    /// Look ahead `n` tokens past the current position without consuming.
    fn peek_at(&self, n: usize) -> Option<&Token> {
        self.tokens.get(self.pos + n)
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
            Err(format!(
                "expected keyword `{}`, found {:?}",
                kw,
                self.peek()
            ))
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

    /// Parse a single-quoted string literal.
    fn parse_string_literal(&mut self) -> Result<String, String> {
        match self.advance() {
            Some(Token::StringLit(s)) => Ok(s),
            other => Err(format!("expected string literal, found {other:?}")),
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
            "alter" => self.parse_alter(),
            "drop" => self.parse_drop(),
            "insert" => self.parse_insert(),
            "copy" => self.parse_copy(),
            "truncate" => self.parse_truncate(),
            "declare" => self.parse_declare_cursor(),
            "fetch" => self.parse_fetch(),
            "select" | "with" => Ok(Statement::Select(self.parse_select()?)),
            "update" => self.parse_update(),
            "delete" => self.parse_delete(),
            "merge" => self.parse_merge(),
            "explain" => self.parse_explain(),
            "analyze" => self.parse_analyze(),
            "comment" => self.parse_comment(),
            "grant" => self.parse_grant(),
            "revoke" => self.parse_revoke(),
            "security" => self.parse_security_label(),
            "vacuum" => self.parse_vacuum(),
            "reindex" => self.parse_reindex(),
            "cluster" => self.parse_cluster(),
            "checkpoint" => {
                self.advance();
                Ok(Statement::Checkpoint)
            }
            "discard" => self.parse_discard(),
            "listen" => self.parse_listen(),
            "notify" => self.parse_notify(),
            "unlisten" => self.parse_unlisten(),
            "lock" => self.parse_lock_table(),
            "refresh" => self.parse_refresh(),
            "begin" | "start" => {
                self.advance();
                // `START TRANSACTION` / `BEGIN [TRANSACTION|WORK]` followed by
                // optional transaction modes (isolation level / read-only).
                self.eat_keyword("transaction");
                self.eat_keyword("work");
                let (isolation, read_only) = self.parse_transaction_modes()?;
                Ok(Statement::Begin {
                    isolation,
                    read_only,
                })
            }
            "commit" | "end" => {
                self.advance();
                // `COMMIT PREPARED 'gid'` is two-phase commit.
                if self.eat_keyword("prepared") {
                    let gid = self.parse_string_literal()?;
                    return Ok(Statement::CommitPrepared { gid });
                }
                self.eat_keyword("transaction");
                self.eat_keyword("work");
                Ok(Statement::Commit)
            }
            "prepare" => {
                self.advance();
                // `PREPARE TRANSACTION 'gid'` is two-phase commit. Other
                // `PREPARE name AS ...` (prepared statements) are unsupported.
                self.expect_keyword("transaction")?;
                let gid = self.parse_string_literal()?;
                Ok(Statement::PrepareTransaction { gid })
            }
            "rollback" | "abort" => {
                self.advance();
                if self.eat_keyword("to") {
                    self.eat_keyword("savepoint");
                    let name = self.parse_ident()?;
                    Ok(Statement::RollbackToSavepoint { name })
                } else if self.eat_keyword("prepared") {
                    let gid = self.parse_string_literal()?;
                    Ok(Statement::RollbackPrepared { gid })
                } else {
                    self.eat_keyword("transaction");
                    self.eat_keyword("work");
                    Ok(Statement::Rollback)
                }
            }
            "savepoint" => self.parse_savepoint(),
            "release" => self.parse_release(),
            "set" => self.parse_set(),
            "reset" => {
                self.advance();
                let name = if self.eat_keyword("all") {
                    None
                } else {
                    Some(self.parse_guc_name()?)
                };
                Ok(Statement::ResetConfig { name })
            }
            "show" => {
                self.advance();
                let name = if self.eat_keyword("all") {
                    "all".to_string()
                } else {
                    self.parse_guc_name()?
                };
                Ok(Statement::Show { name })
            }
            other => Err(format!("unsupported statement: `{other}`")),
        }
    }

    fn parse_explain(&mut self) -> Result<Statement, String> {
        self.expect_keyword("explain")?;
        let analyze = self.eat_keyword("analyze");
        let statement = self.parse_statement()?;
        Ok(Statement::Explain(Explain {
            analyze,
            statement: Box::new(statement),
        }))
    }

    fn parse_analyze(&mut self) -> Result<Statement, String> {
        self.expect_keyword("analyze")?;
        self.eat_keyword("verbose");
        let table = if matches!(self.peek(), Some(Token::Word(_))) {
            Some(self.parse_object_name()?)
        } else {
            None
        };
        Ok(Statement::Analyze(Analyze { table }))
    }

    fn parse_comment(&mut self) -> Result<Statement, String> {
        self.expect_keyword("comment")?;
        self.expect_keyword("on")?;
        let object = if self.eat_keyword("column") {
            let (table, column) = self.parse_column_comment_target()?;
            CommentObject::Column { table, column }
        } else {
            let _ = self.eat_keyword("table")
                || self.eat_keyword("view")
                || (self.eat_keyword("materialized") && {
                    self.expect_keyword("view")?;
                    true
                });
            let name = self.parse_object_name()?;
            CommentObject::Relation { name }
        };
        self.expect_keyword("is")?;
        let comment = if self.eat_keyword("null") {
            None
        } else {
            match self.advance() {
                Some(Token::StringLit(s)) => Some(s),
                other => return Err(format!("expected comment string or NULL, got {other:?}")),
            }
        };
        Ok(Statement::Comment(Comment { object, comment }))
    }

    /// `GRANT priv_list ON [TABLE] name TO grantee_list [WITH GRANT OPTION]`
    /// or `GRANT role[,...] TO role[,...] [WITH ADMIN OPTION]`.
    fn parse_grant(&mut self) -> Result<Statement, String> {
        self.expect_keyword("grant")?;
        let object = self.parse_grant_object()?;
        self.expect_keyword("to")?;
        let grantees = self.parse_grantee_list()?;
        // Accept and ignore WITH GRANT/ADMIN OPTION and GRANTED BY.
        if self.eat_keyword("with") {
            let _ = self.eat_keyword("grant") || self.eat_keyword("admin");
            self.eat_keyword("option");
        }
        if self.eat_keyword("granted") {
            self.expect_keyword("by")?;
            let _ = self.parse_ident()?;
        }
        Ok(Statement::Grant(Grant { object, grantees }))
    }

    /// `REVOKE [GRANT OPTION FOR] ... FROM ... [CASCADE|RESTRICT]`.
    fn parse_revoke(&mut self) -> Result<Statement, String> {
        self.expect_keyword("revoke")?;
        // Optional `GRANT OPTION FOR` / `ADMIN OPTION FOR` prefix — ignored.
        if (self.is_keyword("grant") || self.is_keyword("admin")) && self.is_keyword_at(1, "option") {
            self.advance();
            self.advance();
            self.expect_keyword("for")?;
        }
        let object = self.parse_grant_object()?;
        self.expect_keyword("from")?;
        let grantees = self.parse_grantee_list()?;
        if self.eat_keyword("granted") {
            self.expect_keyword("by")?;
            let _ = self.parse_ident()?;
        }
        // Accept and ignore CASCADE / RESTRICT.
        let _ = self.eat_keyword("cascade") || self.eat_keyword("restrict");
        Ok(Statement::Revoke(Revoke { object, grantees }))
    }

    /// Parse the body shared by GRANT/REVOKE: either privileges on a table, or
    /// a list of roles (role membership). Disambiguated by the presence of `ON`.
    fn parse_grant_object(&mut self) -> Result<GrantObject, String> {
        // `ALL [PRIVILEGES] ON ...` is always a table privilege grant.
        if self.is_keyword("all") {
            self.advance();
            self.eat_keyword("privileges");
            self.expect_keyword("on")?;
            let _ = self.eat_keyword("table");
            let table = self.parse_object_name()?;
            return Ok(GrantObject::Table {
                privileges: Privileges::All,
                table,
            });
        }
        // Gather a comma-separated list of names; each is either a privilege
        // keyword (table grant) or a role name (membership grant).
        let mut names = vec![self.parse_ident()?];
        while self.eat(&Token::Comma) {
            names.push(self.parse_ident()?);
        }
        if self.eat_keyword("on") {
            let _ = self.eat_keyword("table");
            let table = self.parse_object_name()?;
            let mut privileges = Vec::new();
            for name in &names {
                privileges.push(privilege_from_keyword(name)?);
            }
            Ok(GrantObject::Table {
                privileges: Privileges::List(privileges),
                table,
            })
        } else {
            // No ON: this is a role-membership grant of the listed roles.
            Ok(GrantObject::Roles { roles: names })
        }
    }

    fn parse_grantee_list(&mut self) -> Result<Vec<Grantee>, String> {
        let mut grantees = vec![self.parse_grantee()?];
        while self.eat(&Token::Comma) {
            grantees.push(self.parse_grantee()?);
        }
        Ok(grantees)
    }

    fn parse_grantee(&mut self) -> Result<Grantee, String> {
        // `GROUP name` is legacy syntax; swallow the keyword.
        self.eat_keyword("group");
        if self.is_keyword("public") {
            self.advance();
            Ok(Grantee::Public)
        } else {
            Ok(Grantee::Role(self.parse_ident()?))
        }
    }

    fn parse_security_label(&mut self) -> Result<Statement, String> {
        self.expect_keyword("security")?;
        self.expect_keyword("label")?;
        let provider = if self.eat_keyword("for") {
            self.parse_ident()?
        } else {
            "default".into()
        };
        self.expect_keyword("on")?;
        let object = if self.eat_keyword("column") {
            let (table, column) = self.parse_column_comment_target()?;
            CommentObject::Column { table, column }
        } else {
            let _ = self.eat_keyword("table")
                || self.eat_keyword("view")
                || (self.eat_keyword("materialized") && {
                    self.expect_keyword("view")?;
                    true
                });
            CommentObject::Relation {
                name: self.parse_object_name()?,
            }
        };
        self.expect_keyword("is")?;
        let label = if self.eat_keyword("null") {
            None
        } else {
            match self.advance() {
                Some(Token::StringLit(s)) => Some(s),
                other => {
                    return Err(format!(
                        "expected security label string or NULL, got {other:?}"
                    ));
                }
            }
        };
        Ok(Statement::SecurityLabel(SecurityLabel {
            provider,
            object,
            label,
        }))
    }

    fn parse_vacuum(&mut self) -> Result<Statement, String> {
        self.expect_keyword("vacuum")?;
        if self.eat(&Token::LParen) {
            while !self.eat(&Token::RParen) {
                self.advance()
                    .ok_or_else(|| "unterminated VACUUM option list".to_string())?;
            }
        }
        self.eat_keyword("verbose");
        self.eat_keyword("analyze");
        let table = if matches!(self.peek(), Some(Token::Word(_) | Token::QuotedIdent(_))) {
            Some(self.parse_object_name()?)
        } else {
            None
        };
        Ok(Statement::Vacuum(Vacuum { table }))
    }

    fn parse_reindex(&mut self) -> Result<Statement, String> {
        self.expect_keyword("reindex")?;
        let target = if self.eat_keyword("table") {
            ReindexTarget::Table(self.parse_object_name()?)
        } else if self.eat_keyword("index") {
            ReindexTarget::Index(self.parse_object_name()?)
        } else if self.eat_keyword("database") {
            ReindexTarget::Database(self.parse_object_name()?)
        } else if self.eat_keyword("system") {
            let db = if matches!(self.peek(), Some(Token::Word(_) | Token::QuotedIdent(_))) {
                Some(self.parse_object_name()?)
            } else {
                None
            };
            ReindexTarget::System(db)
        } else {
            return Err("REINDEX requires TABLE, INDEX, DATABASE, or SYSTEM".into());
        };
        Ok(Statement::Reindex(Reindex { target }))
    }

    fn parse_cluster(&mut self) -> Result<Statement, String> {
        self.expect_keyword("cluster")?;
        self.eat_keyword("verbose");
        let table = if matches!(self.peek(), Some(Token::Word(_) | Token::QuotedIdent(_))) {
            Some(self.parse_object_name()?)
        } else {
            None
        };
        let index = if self.eat_keyword("using") {
            Some(self.parse_object_name()?)
        } else {
            None
        };
        Ok(Statement::Cluster(Cluster { table, index }))
    }

    fn parse_role_options(&mut self) -> Result<RoleOptions, String> {
        let mut options = RoleOptions::default();
        self.eat_keyword("with");
        while self.peek().is_some() && !matches!(self.peek(), Some(Token::Semicolon)) {
            if self.eat_keyword("superuser") {
                options.superuser = Some(true);
            } else if self.eat_keyword("nosuperuser") {
                options.superuser = Some(false);
            } else if self.eat_keyword("inherit") {
                options.inherit = Some(true);
            } else if self.eat_keyword("noinherit") {
                options.inherit = Some(false);
            } else if self.eat_keyword("createrole") {
                options.create_role = Some(true);
            } else if self.eat_keyword("nocreaterole") {
                options.create_role = Some(false);
            } else if self.eat_keyword("createdb") {
                options.create_db = Some(true);
            } else if self.eat_keyword("nocreatedb") {
                options.create_db = Some(false);
            } else if self.eat_keyword("login") {
                options.login = Some(true);
            } else if self.eat_keyword("nologin") {
                options.login = Some(false);
            } else if self.eat_keyword("replication") {
                options.replication = Some(true);
            } else if self.eat_keyword("noreplication") {
                options.replication = Some(false);
            } else if self.eat_keyword("bypassrls") {
                options.bypass_rls = Some(true);
            } else if self.eat_keyword("nobypassrls") {
                options.bypass_rls = Some(false);
            } else if self.eat_keyword("connection") {
                self.expect_keyword("limit")?;
                let limit = match self.advance() {
                    Some(Token::Number(s)) => s
                        .parse::<i64>()
                        .map_err(|_| format!("invalid CONNECTION LIMIT: {s}"))?,
                    other => {
                        return Err(format!("expected CONNECTION LIMIT number, got {other:?}"));
                    }
                };
                options.connection_limit = Some(limit);
            } else if self.eat_keyword("password") {
                let password = if self.eat_keyword("null") {
                    None
                } else {
                    match self.advance() {
                        Some(Token::StringLit(s)) => Some(s),
                        other => return Err(format!("expected PASSWORD string, got {other:?}")),
                    }
                };
                options.password = Some(password);
            } else if self.eat_keyword("valid") {
                self.expect_keyword("until")?;
                let valid_until = if self.eat_keyword("infinity") {
                    None
                } else {
                    match self.advance() {
                        Some(Token::StringLit(s)) => Some(s),
                        other => return Err(format!("expected VALID UNTIL string, got {other:?}")),
                    }
                };
                options.valid_until = Some(valid_until);
            } else if self.eat_keyword("in") {
                // `IN ROLE name[,...]` / `IN GROUP name[,...]`.
                let _ = self.eat_keyword("role") || self.eat_keyword("group");
                options.in_role.extend(self.parse_role_name_list()?);
            } else if self.eat_keyword("role") {
                options.role_members.extend(self.parse_role_name_list()?);
            } else if self.eat_keyword("admin") {
                options.admin_members.extend(self.parse_role_name_list()?);
            } else if self.eat_keyword("user") {
                // `USER name[,...]` is a legacy alias for `ROLE name[,...]`.
                options.role_members.extend(self.parse_role_name_list()?);
            } else if self.eat_keyword("sysid") {
                let _ = self.advance();
            } else {
                return Err(format!("unsupported role option near {:?}", self.peek()));
            }
        }
        Ok(options)
    }

    /// Parse a comma-separated list of role names (used by `IN ROLE`, `ROLE`,
    /// `ADMIN` in CREATE/ALTER ROLE).
    fn parse_role_name_list(&mut self) -> Result<Vec<String>, String> {
        let mut names = vec![self.parse_ident()?];
        while self.eat(&Token::Comma) {
            names.push(self.parse_ident()?);
        }
        Ok(names)
    }

    fn parse_sequence_options(&mut self) -> Result<(i64, i64), String> {
        let mut start = 1;
        let mut increment = 1;
        while self.peek().is_some() && !matches!(self.peek(), Some(Token::Semicolon)) {
            if self.eat_keyword("start") {
                self.eat_keyword("with");
                start = self.parse_i64_literal("START")?;
            } else if self.eat_keyword("increment") {
                self.eat_keyword("by");
                increment = self.parse_i64_literal("INCREMENT")?;
            } else if self.eat_keyword("minvalue")
                || self.eat_keyword("maxvalue")
                || self.eat_keyword("cache")
            {
                let _ = self.advance();
            } else if self.eat_keyword("no") {
                let _ = self.advance();
            } else if self.eat_keyword("cycle") || self.eat_keyword("owned") {
                if self.is_keyword("by") {
                    self.advance();
                    let _ = self.parse_object_name();
                }
            } else {
                return Err(format!(
                    "unsupported sequence option near {:?}",
                    self.peek()
                ));
            }
        }
        Ok((start, increment))
    }

    fn parse_alter_sequence_options(&mut self) -> Result<(Option<i64>, Option<i64>), String> {
        let mut restart = None;
        let mut increment = None;
        while self.peek().is_some() && !matches!(self.peek(), Some(Token::Semicolon)) {
            if self.eat_keyword("restart") {
                self.eat_keyword("with");
                restart = Some(self.parse_i64_literal("RESTART")?);
            } else if self.eat_keyword("increment") {
                self.eat_keyword("by");
                increment = Some(self.parse_i64_literal("INCREMENT")?);
            } else {
                return Err(format!(
                    "unsupported ALTER SEQUENCE option near {:?}",
                    self.peek()
                ));
            }
        }
        Ok((restart, increment))
    }

    fn parse_i64_literal(&mut self, context: &str) -> Result<i64, String> {
        let sign = if self.eat(&Token::Minus) { -1 } else { 1 };
        match self.advance() {
            Some(Token::Number(s)) => s
                .parse::<i64>()
                .map(|value| value * sign)
                .map_err(|_| format!("invalid {context} value: {s}")),
            other => Err(format!("expected {context} value, got {other:?}")),
        }
    }

    fn parse_discard(&mut self) -> Result<Statement, String> {
        self.expect_keyword("discard")?;
        let target = if self.eat_keyword("all") {
            DiscardTarget::All
        } else if self.eat_keyword("plans") {
            DiscardTarget::Plans
        } else if self.eat_keyword("sequences") {
            DiscardTarget::Sequences
        } else if self.eat_keyword("temp") || self.eat_keyword("temporary") {
            DiscardTarget::Temp
        } else {
            return Err("DISCARD requires ALL, PLANS, SEQUENCES, or TEMP".into());
        };
        Ok(Statement::Discard(Discard { target }))
    }

    fn parse_listen(&mut self) -> Result<Statement, String> {
        self.expect_keyword("listen")?;
        let channel = self.parse_ident()?;
        Ok(Statement::Listen { channel })
    }

    fn parse_notify(&mut self) -> Result<Statement, String> {
        self.expect_keyword("notify")?;
        let channel = self.parse_ident()?;
        let payload = if self.eat(&Token::Comma) {
            match self.advance() {
                Some(Token::StringLit(s)) => Some(s),
                other => return Err(format!("expected NOTIFY payload string, got {other:?}")),
            }
        } else {
            None
        };
        Ok(Statement::Notify { channel, payload })
    }

    fn parse_unlisten(&mut self) -> Result<Statement, String> {
        self.expect_keyword("unlisten")?;
        let channel = if self.eat(&Token::Star) || self.eat_keyword("all") {
            None
        } else {
            Some(self.parse_ident()?)
        };
        Ok(Statement::Unlisten { channel })
    }

    fn parse_lock_table(&mut self) -> Result<Statement, String> {
        self.expect_keyword("lock")?;
        self.eat_keyword("table");
        let mut tables = Vec::new();
        loop {
            tables.push(self.parse_object_name()?);
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        let mut mode = None;
        if self.eat_keyword("in") {
            // Collect the mode words up to the `MODE` keyword (e.g. "ACCESS
            // EXCLUSIVE"), so the lock manager can interpret it.
            let mut words: Vec<String> = Vec::new();
            while self.peek().is_some()
                && !matches!(self.peek(), Some(Token::Semicolon))
                && !self.is_keyword("nowait")
            {
                if self.eat_keyword("mode") {
                    break;
                }
                if let Some(Token::Word(w)) = self.peek() {
                    words.push(w.clone());
                }
                self.advance();
            }
            if !words.is_empty() {
                mode = Some(words.join(" ").to_ascii_uppercase());
            }
        }
        let nowait = self.eat_keyword("nowait");
        Ok(Statement::LockTable(LockTable {
            tables,
            mode,
            nowait,
        }))
    }

    fn parse_column_comment_target(&mut self) -> Result<(String, String), String> {
        let mut parts = vec![self.parse_ident()?];
        while self.eat(&Token::Dot) {
            parts.push(self.parse_ident()?);
        }
        match parts.as_slice() {
            [table, column] => Ok((table.clone(), column.clone())),
            [_schema, table, column] => Ok((table.clone(), column.clone())),
            _ => Err("COMMENT ON COLUMN requires table.column".into()),
        }
    }

    fn parse_savepoint(&mut self) -> Result<Statement, String> {
        self.expect_keyword("savepoint")?;
        let name = self.parse_ident()?;
        Ok(Statement::Savepoint { name })
    }

    fn parse_release(&mut self) -> Result<Statement, String> {
        self.expect_keyword("release")?;
        self.eat_keyword("savepoint");
        let name = self.parse_ident()?;
        Ok(Statement::ReleaseSavepoint { name })
    }

    fn parse_create(&mut self) -> Result<Statement, String> {
        self.expect_keyword("create")?;
        // `CREATE [UNIQUE] INDEX ...` branches off here.
        if self.is_keyword("unique") || self.is_keyword("index") {
            return self.parse_create_index();
        }
        if self.eat_keyword("materialized") {
            self.expect_keyword("view")?;
            let if_not_exists = self.parse_if_not_exists();
            let name = self.parse_object_name()?;
            self.expect_keyword("as")?;
            let select = self.parse_select()?;
            return Ok(Statement::CreateMaterializedView(CreateMaterializedView {
                name,
                if_not_exists,
                select: Box::new(select),
            }));
        }
        let or_replace = if self.eat_keyword("or") {
            self.expect_keyword("replace")?;
            true
        } else {
            false
        };
        if self.eat_keyword("view") {
            let name = self.parse_object_name()?;
            self.expect_keyword("as")?;
            let select = self.parse_select()?;
            return Ok(Statement::CreateView(CreateView {
                name,
                or_replace,
                select: Box::new(select),
            }));
        }
        if self.eat_keyword("function") {
            return self.parse_create_function(or_replace);
        }
        if self.eat_keyword("trigger") {
            return self.parse_create_trigger();
        }
        if self.eat_keyword("rule") {
            return self.parse_create_rule(or_replace);
        }
        if self.eat_keyword("aggregate") {
            return self.parse_create_aggregate(or_replace);
        }
        if !or_replace && self.eat_keyword("policy") {
            return self.parse_create_policy();
        }
        if or_replace {
            return Err("CREATE OR REPLACE is only supported for VIEW, FUNCTION, RULE, AGGREGATE".into());
        }
        if self.eat_keyword("extension") {
            let if_not_exists = self.parse_if_not_exists();
            let name = self.parse_object_name()?;
            let mut version = None;
            self.eat_keyword("with");
            if self.eat_keyword("version") {
                version = Some(match self.advance() {
                    Some(Token::StringLit(s))
                    | Some(Token::Word(s))
                    | Some(Token::QuotedIdent(s)) => s,
                    other => return Err(format!("expected extension version, found {other:?}")),
                });
            }
            while self.eat_keyword("schema") {
                let _ = self.parse_object_name()?;
            }
            if self.eat_keyword("cascade") {
                // accepted for compatibility; no dependency graph yet
            }
            return Ok(Statement::CreateExtension(CreateExtension {
                name,
                if_not_exists,
                version,
            }));
        }
        if self.eat_keyword("role") {
            let name = self.parse_ident()?;
            let options = self.parse_role_options()?;
            return Ok(Statement::CreateRole(CreateRole {
                name,
                login: false,
                options,
            }));
        }
        if self.eat_keyword("sequence") {
            let if_not_exists = self.parse_if_not_exists();
            let name = self.parse_object_name()?;
            let (start, increment) = self.parse_sequence_options()?;
            return Ok(Statement::CreateSequence(CreateSequence {
                name,
                if_not_exists,
                start,
                increment,
            }));
        }
        if self.eat_keyword("user") {
            // `CREATE USER MAPPING FOR role SERVER s [OPTIONS(...)]` is a foreign
            // catalog object, distinct from `CREATE USER name` (a login role).
            if self.eat_keyword("mapping") {
                self.expect_keyword("for")?;
                let name = self.parse_object_name()?;
                let definition = self.collect_statement_tail();
                return Ok(Statement::CreateCatalogObject(CatalogObject {
                    kind: CatalogObjectKind::UserMapping,
                    name,
                    definition,
                }));
            }
            let name = self.parse_ident()?;
            let mut options = self.parse_role_options()?;
            options.login = Some(true);
            return Ok(Statement::CreateRole(CreateRole {
                name,
                login: true,
                options,
            }));
        }
        if self.eat_keyword("schema") {
            let if_not_exists = self.parse_if_not_exists();
            let name = self.parse_object_name()?;
            return Ok(Statement::CreateSchema(CreateSchema {
                name,
                if_not_exists,
            }));
        }
        if self.eat_keyword("database") {
            let name = self.parse_object_name()?;
            self.discard_statement_tail();
            return Ok(Statement::CreateDatabase(CreateDatabase { name }));
        }
        if self.eat_keyword("tablespace") {
            let name = self.parse_object_name()?;
            self.expect_keyword("location")?;
            let location = match self.advance() {
                Some(Token::StringLit(s)) => s,
                other => return Err(format!("expected tablespace location, found {other:?}")),
            };
            return Ok(Statement::CreateTablespace(CreateTablespace {
                name,
                location,
            }));
        }
        if self.eat_keyword("collation") {
            let if_not_exists = self.parse_if_not_exists();
            let name = self.parse_object_name()?;
            let locale = self.parse_collation_options()?;
            return Ok(Statement::CreateCollation(CreateCollation {
                name,
                if_not_exists,
                locale,
            }));
        }
        if self.eat_keyword("type") {
            return self.parse_create_type();
        }
        if self.eat_keyword("domain") {
            return self.parse_create_domain();
        }
        if self.eat_keyword("operator") {
            return self.parse_create_operator_object();
        }
        if self.eat_keyword("event") {
            self.expect_keyword("trigger")?;
            let name = self.parse_object_name()?;
            let definition = self.collect_statement_tail();
            return Ok(Statement::CreateCatalogObject(CatalogObject {
                kind: CatalogObjectKind::EventTrigger,
                name,
                definition,
            }));
        }
        if self.eat_keyword("foreign") {
            return self.parse_create_foreign();
        }
        if self.eat_keyword("server") {
            let name = self.parse_object_name()?;
            let definition = self.collect_statement_tail();
            return Ok(Statement::CreateCatalogObject(CatalogObject {
                kind: CatalogObjectKind::Server,
                name,
                definition,
            }));
        }
        if self.eat_keyword("publication") {
            let name = self.parse_object_name()?;
            let definition = self.collect_statement_tail();
            return Ok(Statement::CreateCatalogObject(CatalogObject {
                kind: CatalogObjectKind::Publication,
                name,
                definition,
            }));
        }
        if self.eat_keyword("subscription") {
            let name = self.parse_object_name()?;
            let definition = self.collect_statement_tail();
            return Ok(Statement::CreateCatalogObject(CatalogObject {
                kind: CatalogObjectKind::Subscription,
                name,
                definition,
            }));
        }
        let persistence = if self.eat_keyword("temporary") || self.eat_keyword("temp") {
            TablePersistence::Temporary
        } else if self.eat_keyword("unlogged") {
            TablePersistence::Unlogged
        } else {
            TablePersistence::Permanent
        };
        self.expect_keyword("table")?;
        let if_not_exists = self.parse_if_not_exists();
        let name = self.parse_object_name()?;

        // `CREATE TABLE p PARTITION OF parent FOR VALUES ...`: a partition has no
        // column list of its own (it inherits the parent's columns).
        if self.is_keyword("partition") && self.is_keyword_at(1, "of") {
            self.advance();
            self.advance();
            let parent = self.parse_object_name()?;
            let bound = self.parse_partition_bound()?;
            return Ok(Statement::CreateTable(CreateTable {
                name,
                columns: Vec::new(),
                constraints: Vec::new(),
                if_not_exists,
                persistence,
                inherits: Vec::new(),
                partition_by: None,
                partition_of: Some(PartitionOf { parent, bound }),
            }));
        }

        self.expect(&Token::LParen)?;

        let mut columns = Vec::new();
        let mut constraints = Vec::new();
        loop {
            if self.is_keyword("primary")
                || self.is_keyword("unique")
                || self.is_keyword("constraint")
                || self.is_keyword("check")
                || self.is_keyword("foreign")
                || self.is_keyword("exclude")
            {
                constraints.push(self.parse_table_constraint(&name)?);
            } else {
                let (col, mut inline) = self.parse_column_def_with_constraints(&name)?;
                columns.push(col);
                constraints.append(&mut inline);
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

        // `INHERITS (parent1, parent2, ...)`.
        let mut inherits = Vec::new();
        if self.eat_keyword("inherits") {
            self.expect(&Token::LParen)?;
            loop {
                inherits.push(self.parse_object_name()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
            self.expect(&Token::RParen)?;
        }

        // `PARTITION BY {RANGE|LIST|HASH} (col)`.
        let mut partition_by = None;
        if self.is_keyword("partition") && self.is_keyword_at(1, "by") {
            self.advance();
            self.advance();
            let strategy = if self.eat_keyword("range") {
                PartitionStrategy::Range
            } else if self.eat_keyword("list") {
                PartitionStrategy::List
            } else if self.eat_keyword("hash") {
                PartitionStrategy::Hash
            } else {
                return Err("expected RANGE, LIST or HASH after PARTITION BY".to_string());
            };
            self.expect(&Token::LParen)?;
            let column = self.parse_ident()?;
            self.expect(&Token::RParen)?;
            partition_by = Some(PartitionBy { strategy, column });
        }

        Ok(Statement::CreateTable(CreateTable {
            name,
            columns,
            constraints,
            if_not_exists,
            persistence,
            inherits,
            partition_by,
            partition_of: None,
        }))
    }

    /// Parse a partition `FOR VALUES ...` bound.
    fn parse_partition_bound(&mut self) -> Result<PartitionBound, String> {
        self.expect_keyword("for")?;
        self.expect_keyword("values")?;
        if self.eat_keyword("from") {
            self.expect(&Token::LParen)?;
            let from = self.parse_expr()?;
            self.expect(&Token::RParen)?;
            self.expect_keyword("to")?;
            self.expect(&Token::LParen)?;
            let to = self.parse_expr()?;
            self.expect(&Token::RParen)?;
            Ok(PartitionBound::Range { from, to })
        } else if self.eat_keyword("in") {
            self.expect(&Token::LParen)?;
            let mut list = Vec::new();
            loop {
                list.push(self.parse_expr()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
            self.expect(&Token::RParen)?;
            Ok(PartitionBound::List(list))
        } else if self.eat_keyword("with") {
            self.expect(&Token::LParen)?;
            let mut modulus = 0i64;
            let mut remainder = 0i64;
            loop {
                let key = self.parse_ident()?;
                let value = match self.parse_expr()? {
                    Expr::Int(n) => n,
                    _ => return Err("MODULUS/REMAINDER must be integers".to_string()),
                };
                if key.eq_ignore_ascii_case("modulus") {
                    modulus = value;
                } else if key.eq_ignore_ascii_case("remainder") {
                    remainder = value;
                } else {
                    return Err(format!("unexpected partition option \"{key}\""));
                }
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
            self.expect(&Token::RParen)?;
            Ok(PartitionBound::Hash { modulus, remainder })
        } else {
            Err("expected FROM, IN or WITH after FOR VALUES".to_string())
        }
    }

    /// Parse `[UNIQUE] INDEX [IF NOT EXISTS] [name] ON table (column)`.
    /// The leading `CREATE` has already been consumed.
    fn parse_create_index(&mut self) -> Result<Statement, String> {
        let unique = self.eat_keyword("unique");
        self.expect_keyword("index")?;
        let if_not_exists = self.parse_if_not_exists();
        // The index name is optional; `ON` immediately means an unnamed index.
        let name = if self.is_keyword("on") {
            None
        } else {
            Some(self.parse_ident()?)
        };
        self.expect_keyword("on")?;
        let table = self.parse_object_name()?;
        // An optional `USING <method>`. Recognised access methods map to their
        // backing structure; an unknown method is treated as a B-tree.
        let mut method = IndexMethod::Btree;
        if self.eat_keyword("using") {
            let m = self.parse_ident()?;
            method = if m.eq_ignore_ascii_case("hash") {
                IndexMethod::Hash
            } else if m.eq_ignore_ascii_case("gist") {
                IndexMethod::Gist
            } else if m.eq_ignore_ascii_case("spgist") {
                IndexMethod::SpGist
            } else if m.eq_ignore_ascii_case("brin") {
                IndexMethod::Brin
            } else if m.eq_ignore_ascii_case("gin") {
                IndexMethod::Gin
            } else {
                IndexMethod::Btree
            };
        }
        // The key list: each key is either a bare column or a parenthesised
        // expression `(expr)`. A doubled paren `((expr))` is just an expression
        // whose first token is `(`.
        self.expect(&Token::LParen)?;
        let mut keys = Vec::new();
        loop {
            let key = if self.peek() == Some(&Token::LParen) {
                // Parenthesised expression key.
                self.expect(&Token::LParen)?;
                let e = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                IndexKeyExpr::Expr(e)
            } else {
                IndexKeyExpr::Column(self.parse_ident()?)
            };
            // Accept and ignore a trailing `ASC`/`DESC` ordering on the key.
            let _ = self.eat_keyword("asc") || self.eat_keyword("desc");
            keys.push(key);
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        self.expect(&Token::RParen)?;
        // Optional `INCLUDE (col, ...)` covering columns.
        let mut include = Vec::new();
        if self.eat_keyword("include") {
            self.expect(&Token::LParen)?;
            loop {
                include.push(self.parse_ident()?);
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            self.expect(&Token::RParen)?;
        }
        // Optional `WHERE <predicate>` for a partial index.
        let predicate = if self.eat_keyword("where") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Statement::CreateIndex(CreateIndex {
            name,
            table,
            keys,
            unique,
            if_not_exists,
            method,
            include,
            predicate,
        }))
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef, String> {
        // Used by ALTER TABLE ADD COLUMN, where inline table constraints
        // (REFERENCES/UNIQUE/PRIMARY KEY/CHECK) are uncommon; collect and drop.
        Ok(self.parse_column_def_with_constraints("")?.0)
    }

    /// Parse a column definition, returning the column plus any inline
    /// column-level constraints (`REFERENCES`, `UNIQUE`, `CHECK`) translated
    /// into the equivalent table-level [`TableConstraint`]s so the executor's
    /// existing constraint machinery applies. `table` names the owning table
    /// for default constraint naming.
    fn parse_column_def_with_constraints(
        &mut self,
        table: &str,
    ) -> Result<(ColumnDef, Vec<TableConstraint>), String> {
        let name = self.parse_ident()?;
        let (data_type, type_name, serial) = self.parse_column_type()?;
        // `serial` types are implicitly NOT NULL with a sequence default.
        let mut not_null = serial;
        let mut primary_key = false;
        let mut default = None;
        let mut identity = false;
        let mut identity_always = false;
        let mut generated = None;
        let mut inline: Vec<TableConstraint> = Vec::new();
        loop {
            // Optional `CONSTRAINT name` prefix for a column-level constraint.
            // Only consume it when a constraint keyword actually follows.
            let explicit_name = if self.is_keyword("constraint")
                && ["primary", "references", "check", "unique", "not", "null", "default"]
                    .iter()
                    .any(|kw| self.is_keyword_at(2, kw))
            {
                self.advance();
                Some(self.parse_ident()?)
            } else {
                None
            };
            if self.eat_keyword("primary") {
                self.expect_keyword("key")?;
                primary_key = true;
                not_null = true;
            } else if self.eat_keyword("references") {
                // Column-level FK: `REFERENCES reftable [(refcol)] [actions]`.
                let ref_table = self.parse_object_name()?;
                let ref_column = if self.eat(&Token::LParen) {
                    let c = self.parse_ident()?;
                    self.expect(&Token::RParen)?;
                    c
                } else {
                    // Default to the referenced table's primary key column.
                    "id".to_string()
                };
                self.parse_fk_actions_tail();
                let validated = !self.parse_not_valid();
                self.parse_deferrable_tail();
                let cname =
                    explicit_name.unwrap_or_else(|| format!("{table}_{name}_fkey"));
                inline.push(TableConstraint::ForeignKey {
                    name: cname,
                    column: name.clone(),
                    ref_table,
                    ref_column,
                    validated,
                });
            } else if self.eat_keyword("check") {
                self.expect(&Token::LParen)?;
                let expr = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                let validated = !self.parse_not_valid();
                self.parse_deferrable_tail();
                let cname = explicit_name.unwrap_or_else(|| format!("{table}_{name}_check"));
                inline.push(TableConstraint::Check {
                    name: cname,
                    expr,
                    validated,
                });
            } else if self.is_keyword("not") && self.is_keyword_at(1, "null") {
                self.advance();
                self.advance();
                not_null = true;
            } else if self.eat_keyword("null") {
                // explicit nullable
            } else if self.eat_keyword("unique") {
                let cname = explicit_name.unwrap_or_else(|| format!("{table}_{name}_key"));
                inline.push(TableConstraint::Unique {
                    name: cname,
                    columns: vec![name.clone()],
                    primary_key: false,
                });
            } else if self.is_keyword("deferrable")
                || self.is_keyword("initially")
                || (self.is_keyword("not") && self.is_keyword_at(1, "deferrable"))
            {
                // Column-level `[NOT] DEFERRABLE` / `INITIALLY ...` — accepted,
                // no deferral semantics applied.
                self.parse_deferrable_tail();
            } else if self.eat_keyword("collate") {
                let _ = self.parse_qualified_name()?;
            } else if self.eat_keyword("default") {
                default = Some(self.parse_expr()?);
            } else if self.eat_keyword("generated") {
                let generated_always = if self.eat_keyword("always") {
                    true
                } else if self.eat_keyword("by") {
                    self.expect_keyword("default")?;
                    false
                } else {
                    false
                };
                if self.eat_keyword("as") {
                    if self.eat_keyword("identity") {
                        identity = true;
                        identity_always = generated_always;
                        not_null = true;
                        self.skip_identity_options();
                    } else {
                        self.expect(&Token::LParen)?;
                        generated = Some(self.parse_expr()?);
                        self.expect(&Token::RParen)?;
                        self.eat_keyword("stored");
                    }
                } else {
                    return Err("expected AS after GENERATED".into());
                }
            } else {
                break;
            }
        }
        Ok((ColumnDef {
            name,
            data_type,
            type_name,
            not_null,
            primary_key,
            default,
            serial,
            identity,
            identity_always,
            generated,
        }, inline))
    }

    fn skip_identity_options(&mut self) {
        if self.eat(&Token::LParen) {
            let mut depth = 1usize;
            while depth > 0 {
                match self.advance() {
                    Some(Token::LParen) => depth += 1,
                    Some(Token::RParen) => depth -= 1,
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }

    /// Parse a column type, recognizing the `serial` family as auto-increment
    /// integer types. Returns the underlying type, the declared user-type name
    /// (when not a built-in), and whether it is serial.
    fn parse_column_type(&mut self) -> Result<(DataType, Option<String>, bool), String> {
        if self.is_keyword("serial") || self.is_keyword("serial4") {
            self.advance();
            return Ok((DataType::Int4, None, true));
        }
        if self.is_keyword("bigserial") || self.is_keyword("serial8") {
            self.advance();
            return Ok((DataType::Int8, None, true));
        }
        if self.is_keyword("smallserial") || self.is_keyword("serial2") {
            self.advance();
            return Ok((DataType::Int2, None, true));
        }
        let (data_type, type_name) = self.parse_data_type_named()?;
        Ok((data_type, type_name, false))
    }

    fn parse_data_type(&mut self) -> Result<DataType, String> {
        Ok(self.parse_data_type_named()?.0)
    }

    /// Parse a type name. Returns the resolved built-in `DataType` (unknown
    /// names fall back to text) and, for names that are not built-ins, the raw
    /// lowercased type name so the executor can resolve user-defined types.
    fn parse_data_type_named(&mut self) -> Result<(DataType, Option<String>), String> {
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
        // Array type suffix: `integer[]`, `text[][]`, or `integer ARRAY[3]`.
        // Arrays are text-backed (PostgreSQL `{...}` encoding), so any element
        // type degrades to a text-backed array recording the declared name so
        // it round-trips and appears with a `[]`-suffixed type name.
        let mut is_array = false;
        if self.is_keyword("array") {
            self.advance();
            is_array = true;
        }
        while self.eat(&Token::LBracket) {
            is_array = true;
            // Optional dimension size, e.g. `[3]`; accepted and ignored.
            while !self.eat(&Token::RBracket) {
                if self.advance().is_none() {
                    return Err("unterminated array dimension".to_string());
                }
            }
        }
        if is_array {
            let elem = DataType::from_sql_name(&name)
                .map(|dt| dt.pg_type_name().to_string())
                .unwrap_or(name);
            return Ok((DataType::Text, Some(format!("{elem}[]"))));
        }
        // Unknown (non-built-in) names default to text but keep the declared
        // name, so a user-defined enum/domain/composite/range column can be
        // resolved and enforced at execution time.
        match DataType::from_sql_name(&name) {
            Some(dt) => Ok((dt, None)),
            None => Ok((DataType::Text, Some(name))),
        }
    }

    /// Parse `ALTER TABLE [IF EXISTS] name <action>`.
    fn parse_alter(&mut self) -> Result<Statement, String> {
        self.expect_keyword("alter")?;
        if self.eat_keyword("role") || self.eat_keyword("user") {
            let name = self.parse_ident()?;
            let options = self.parse_role_options()?;
            return Ok(Statement::AlterRole(AlterRole { name, options }));
        }
        if self.eat_keyword("system") {
            let action = if self.eat_keyword("set") {
                let name = self.parse_object_name()?;
                self.expect(&Token::Eq)?;
                let value = self.parse_setting_value();
                AlterSystemAction::Set { name, value }
            } else if self.eat_keyword("reset") {
                let name = if self.eat_keyword("all") {
                    None
                } else {
                    Some(self.parse_object_name()?)
                };
                AlterSystemAction::Reset { name }
            } else {
                return Err("ALTER SYSTEM requires SET or RESET".into());
            };
            return Ok(Statement::AlterSystem(AlterSystem { action }));
        }
        if self.eat_keyword("sequence") {
            let name = self.parse_object_name()?;
            let (restart, increment) = self.parse_alter_sequence_options()?;
            return Ok(Statement::AlterSequence(AlterSequence {
                name,
                restart,
                increment,
            }));
        }
        if self.eat_keyword("database") {
            let name = self.parse_object_name()?;
            let action = if self.eat_keyword("rename") {
                self.expect_keyword("to")?;
                AlterDatabaseAction::Rename {
                    to: self.parse_object_name()?,
                }
            } else {
                self.eat_keyword("with");
                self.expect_keyword("connection")?;
                self.expect_keyword("limit")?;
                let limit = match self.advance() {
                    Some(Token::Number(s)) => s
                        .parse::<i64>()
                        .map_err(|_| format!("invalid CONNECTION LIMIT: {s}"))?,
                    other => {
                        return Err(format!("expected CONNECTION LIMIT number, got {other:?}"));
                    }
                };
                AlterDatabaseAction::SetConnectionLimit { limit }
            };
            return Ok(Statement::AlterDatabase(AlterDatabase { name, action }));
        }
        if self.eat_keyword("policy") {
            return self.parse_alter_policy();
        }
        self.expect_keyword("table")?;
        self.parse_if_exists();
        // `ALTER TABLE ONLY t ...` — the ONLY (no-inheritance) qualifier is
        // accepted; this engine applies the action to the named table directly.
        self.eat_keyword("only");
        let table = self.parse_object_name()?;

        let action = if self.eat_keyword("add") {
            if self.is_keyword("constraint")
                || self.is_keyword("unique")
                || self.is_keyword("primary")
                || self.is_keyword("foreign")
                || self.is_keyword("check")
                || self.is_keyword("exclude")
            {
                AlterAction::AddConstraint {
                    constraint: self.parse_table_constraint(&table)?,
                }
            } else {
                self.eat_keyword("column");
                let if_not_exists = self.parse_if_not_exists();
                let column = self.parse_column_def()?;
                AlterAction::AddColumn {
                    column,
                    if_not_exists,
                }
            }
        } else if self.eat_keyword("drop") {
            if self.eat_keyword("constraint") {
                let if_exists = self.parse_if_exists();
                let name = self.parse_ident()?;
                self.eat_keyword("cascade");
                self.eat_keyword("restrict");
                AlterAction::DropConstraint { name, if_exists }
            } else {
                self.eat_keyword("column");
                let if_exists = self.parse_if_exists();
                let name = self.parse_ident()?;
                self.eat_keyword("cascade");
                self.eat_keyword("restrict");
                AlterAction::DropColumn { name, if_exists }
            }
        } else if self.eat_keyword("rename") {
            if self.eat_keyword("to") {
                let to = self.parse_object_name()?;
                AlterAction::RenameTable { to }
            } else {
                self.eat_keyword("column");
                let from = self.parse_ident()?;
                self.expect_keyword("to")?;
                let to = self.parse_ident()?;
                AlterAction::RenameColumn { from, to }
            }
        } else if self.eat_keyword("owner") {
            self.expect_keyword("to")?;
            let owner = self.parse_ident()?;
            AlterAction::OwnerTo { owner }
        } else if self.eat_keyword("enable") {
            self.parse_row_level_security_tail()?;
            AlterAction::RowSecurity {
                action: RowSecurityAction::Enable,
            }
        } else if self.eat_keyword("disable") {
            self.parse_row_level_security_tail()?;
            AlterAction::RowSecurity {
                action: RowSecurityAction::Disable,
            }
        } else if self.eat_keyword("force") {
            self.parse_row_level_security_tail()?;
            AlterAction::RowSecurity {
                action: RowSecurityAction::Force,
            }
        } else if self.eat_keyword("no") {
            self.expect_keyword("force")?;
            self.parse_row_level_security_tail()?;
            AlterAction::RowSecurity {
                action: RowSecurityAction::NoForce,
            }
        } else if self.is_keyword("alter") {
            self.expect_keyword("alter")?;
            self.eat_keyword("column");
            let column = self.parse_ident()?;
            if self.eat_keyword("set") {
                if self.eat_keyword("default") {
                    AlterAction::SetColumnDefault {
                        column,
                        default: self.parse_expr()?,
                    }
                } else if self.is_keyword("not") {
                    self.expect_keyword("not")?;
                    self.expect_keyword("null")?;
                    AlterAction::SetColumnNotNull {
                        column,
                        not_null: true,
                    }
                } else {
                    // SET STORAGE / STATISTICS / (...) / COMPRESSION: no-op.
                    self.skip_to_statement_end();
                    AlterAction::AlterColumnNoop
                }
            } else if self.eat_keyword("drop") {
                if self.eat_keyword("default") {
                    AlterAction::DropColumnDefault { column }
                } else {
                    self.expect_keyword("not")?;
                    self.expect_keyword("null")?;
                    AlterAction::SetColumnNotNull {
                        column,
                        not_null: false,
                    }
                }
            } else {
                // ADD GENERATED / RESET (...) / TYPE ...: accept as no-op.
                self.skip_to_statement_end();
                AlterAction::AlterColumnNoop
            }
        } else if self.is_keyword("set")
            || self.is_keyword("reset")
            || self.is_keyword("cluster")
            || self.is_keyword("inherit")
            || self.is_keyword("validate")
        {
            // Table-level clauses we accept but do not model.
            self.skip_to_statement_end();
            AlterAction::Noop
        } else {
            return Err(format!(
                "unsupported ALTER TABLE action near {:?}",
                self.peek()
            ));
        };
        Ok(Statement::AlterTable(AlterTable { table, action }))
    }

    /// Consume the `ROW LEVEL SECURITY` keyword tail of an
    /// `ALTER TABLE ... {ENABLE|DISABLE|FORCE|NO FORCE} ROW LEVEL SECURITY`.
    fn parse_row_level_security_tail(&mut self) -> Result<(), String> {
        self.expect_keyword("row")?;
        self.expect_keyword("level")?;
        self.expect_keyword("security")?;
        Ok(())
    }

    /// `CREATE POLICY name ON table [AS PERMISSIVE|RESTRICTIVE] [FOR cmd]
    /// [TO role[,...]] [USING (expr)] [WITH CHECK (expr)]`.
    fn parse_create_policy(&mut self) -> Result<Statement, String> {
        let name = self.parse_ident()?;
        self.expect_keyword("on")?;
        let table = self.parse_object_name()?;

        let mut permissive = true;
        if self.eat_keyword("as") {
            if self.eat_keyword("permissive") {
                permissive = true;
            } else if self.eat_keyword("restrictive") {
                permissive = false;
            } else {
                return Err("expected PERMISSIVE or RESTRICTIVE after AS".into());
            }
        }

        let mut command = "all".to_string();
        if self.eat_keyword("for") {
            command = self.parse_policy_command()?;
        }

        let mut roles = Vec::new();
        if self.eat_keyword("to") {
            roles = self.parse_policy_roles()?;
        }

        let mut using = None;
        if self.eat_keyword("using") {
            self.expect(&Token::LParen)?;
            using = Some(self.parse_expr()?);
            self.expect(&Token::RParen)?;
        }

        let mut with_check = None;
        if self.eat_keyword("with") {
            self.expect_keyword("check")?;
            self.expect(&Token::LParen)?;
            with_check = Some(self.parse_expr()?);
            self.expect(&Token::RParen)?;
        }

        Ok(Statement::CreatePolicy(CreatePolicy {
            name,
            table,
            permissive,
            command,
            roles,
            using,
            with_check,
        }))
    }

    /// `ALTER POLICY name ON table [TO role[,...]] [USING (expr)] [WITH CHECK (expr)]`.
    fn parse_alter_policy(&mut self) -> Result<Statement, String> {
        let name = self.parse_ident()?;
        self.expect_keyword("on")?;
        let table = self.parse_object_name()?;

        // `ALTER POLICY name ON t RENAME TO new` — accepted minimally.
        if self.eat_keyword("rename") {
            self.expect_keyword("to")?;
            let _new = self.parse_ident()?;
            return Ok(Statement::AlterPolicy(AlterPolicy {
                name,
                table,
                roles: None,
                using: None,
                with_check: None,
            }));
        }

        let mut roles = None;
        if self.eat_keyword("to") {
            roles = Some(self.parse_policy_roles()?);
        }

        let mut using = None;
        if self.eat_keyword("using") {
            self.expect(&Token::LParen)?;
            using = Some(self.parse_expr()?);
            self.expect(&Token::RParen)?;
        }

        let mut with_check = None;
        if self.eat_keyword("with") {
            self.expect_keyword("check")?;
            self.expect(&Token::LParen)?;
            with_check = Some(self.parse_expr()?);
            self.expect(&Token::RParen)?;
        }

        Ok(Statement::AlterPolicy(AlterPolicy {
            name,
            table,
            roles,
            using,
            with_check,
        }))
    }

    /// `DROP POLICY [IF EXISTS] name ON table`.
    fn parse_drop_policy(&mut self) -> Result<Statement, String> {
        let if_exists = self.parse_if_exists();
        let name = self.parse_ident()?;
        self.expect_keyword("on")?;
        let table = self.parse_object_name()?;
        self.eat_keyword("cascade");
        self.eat_keyword("restrict");
        Ok(Statement::DropPolicy(DropPolicy {
            name,
            table,
            if_exists,
        }))
    }

    /// The command of a policy `FOR` clause (`ALL`/`SELECT`/`INSERT`/`UPDATE`/`DELETE`).
    fn parse_policy_command(&mut self) -> Result<String, String> {
        for kw in ["all", "select", "insert", "update", "delete"] {
            if self.eat_keyword(kw) {
                return Ok(kw.to_string());
            }
        }
        Err("expected ALL/SELECT/INSERT/UPDATE/DELETE after FOR".into())
    }

    /// A comma-separated role list following `TO`. `public` collapses to an
    /// empty list (meaning PUBLIC).
    fn parse_policy_roles(&mut self) -> Result<Vec<String>, String> {
        let mut roles = Vec::new();
        loop {
            if self.eat_keyword("public") {
                // PUBLIC: represented as an empty role list.
            } else if self.eat_keyword("current_user") || self.eat_keyword("session_user") {
                roles.push("current_user".to_string());
            } else {
                roles.push(self.parse_ident()?);
            }
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        Ok(roles)
    }

    fn parse_table_constraint(&mut self, table: &str) -> Result<TableConstraint, String> {
        let explicit_name = if self.eat_keyword("constraint") {
            Some(self.parse_ident()?)
        } else {
            None
        };
        if self.is_keyword("exclude") {
            // Capture `EXCLUDE [USING m] (...)` verbatim; not enforced.
            let definition = self.collect_balanced_until_comma_or_rparen();
            self.parse_deferrable_tail();
            let name = explicit_name.unwrap_or_else(|| format!("{table}_exclusion"));
            return Ok(TableConstraint::Exclude { name, definition });
        }
        if self.eat_keyword("check") {
            self.expect(&Token::LParen)?;
            let expr = self.parse_expr()?;
            self.expect(&Token::RParen)?;
            let validated = !self.parse_not_valid();
            self.parse_deferrable_tail();
            let name = explicit_name.unwrap_or_else(|| format!("{table}_check"));
            return Ok(TableConstraint::Check {
                name,
                expr,
                validated,
            });
        }
        if self.eat_keyword("foreign") {
            self.expect_keyword("key")?;
            self.expect(&Token::LParen)?;
            let column = self.parse_ident()?;
            if self.eat(&Token::Comma) {
                return Err("multi-column foreign keys are not supported yet".into());
            }
            self.expect(&Token::RParen)?;
            self.expect_keyword("references")?;
            let ref_table = self.parse_object_name()?;
            self.expect(&Token::LParen)?;
            let ref_column = self.parse_ident()?;
            if self.eat(&Token::Comma) {
                return Err("multi-column foreign keys are not supported yet".into());
            }
            self.expect(&Token::RParen)?;
            // Accept and ignore ON DELETE/UPDATE actions and MATCH.
            self.parse_fk_actions_tail();
            let validated = !self.parse_not_valid();
            self.parse_deferrable_tail();
            let name = explicit_name.unwrap_or_else(|| format!("{table}_{column}_fkey"));
            return Ok(TableConstraint::ForeignKey {
                name,
                column,
                ref_table,
                ref_column,
                validated,
            });
        }
        let primary_key = if self.eat_keyword("primary") {
            self.expect_keyword("key")?;
            true
        } else if self.eat_keyword("unique") {
            false
        } else {
            return Err("only UNIQUE and PRIMARY KEY table constraints are supported".into());
        };
        self.expect(&Token::LParen)?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.parse_ident()?);
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Token::RParen)?;
        let _ = self.parse_not_valid();
        self.parse_deferrable_tail();
        let suffix = if primary_key { "pkey" } else { "key" };
        let name =
            explicit_name.unwrap_or_else(|| format!("{}_{}_{}", table, columns.join("_"), suffix));
        Ok(TableConstraint::Unique {
            name,
            columns,
            primary_key,
        })
    }

    fn parse_not_valid(&mut self) -> bool {
        if self.eat_keyword("not") {
            self.eat_keyword("valid")
        } else {
            false
        }
    }

    /// Accept and ignore a trailing `[NOT] DEFERRABLE` and/or `INITIALLY
    /// {DEFERRED|IMMEDIATE}` on a constraint. No deferral semantics are applied.
    fn parse_deferrable_tail(&mut self) {
        loop {
            if self.eat_keyword("not") {
                self.eat_keyword("deferrable");
            } else if self.eat_keyword("deferrable") {
                continue;
            } else if self.eat_keyword("initially") {
                let _ = self.eat_keyword("deferred") || self.eat_keyword("immediate");
            } else {
                break;
            }
        }
    }

    /// Accept and ignore foreign-key `MATCH` and `ON DELETE/UPDATE` action
    /// clauses. The referential actions are not enforced.
    fn parse_fk_actions_tail(&mut self) {
        if self.eat_keyword("match") {
            let _ = self.eat_keyword("full")
                || self.eat_keyword("partial")
                || self.eat_keyword("simple");
        }
        while self.eat_keyword("on") {
            let _ = self.eat_keyword("delete") || self.eat_keyword("update");
            if self.eat_keyword("no") {
                self.eat_keyword("action");
            } else if self.eat_keyword("set") {
                let _ = self.eat_keyword("null") || self.eat_keyword("default");
            } else {
                let _ = self.eat_keyword("restrict")
                    || self.eat_keyword("cascade");
            }
        }
    }

    /// Skip tokens (balancing parens) until a top-level comma or `)`, returning
    /// the rendered SQL text (for verbatim constraint round-tripping).
    fn collect_balanced_until_comma_or_rparen(&mut self) -> String {
        let mut out = String::new();
        let mut prev_open_paren = false;
        let mut depth = 0;
        loop {
            match self.peek() {
                None => break,
                Some(Token::LParen) => {
                    depth += 1;
                }
                Some(Token::RParen) => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                Some(Token::Comma) if depth == 0 => break,
                _ => {}
            }
            let tok = self.advance().expect("peeked token exists");
            let glue_left = matches!(tok, Token::Comma | Token::RParen | Token::Dot);
            if !out.is_empty() && !glue_left && !prev_open_paren {
                out.push(' ');
            }
            out.push_str(&token_text(&tok));
            prev_open_paren = matches!(tok, Token::LParen | Token::Dot);
        }
        out
    }

    fn parse_collation_options(&mut self) -> Result<String, String> {
        if self.eat_keyword("from") {
            return self.parse_object_name();
        }
        self.expect(&Token::LParen)?;
        let mut locale = None;
        loop {
            if self.eat_keyword("locale")
                || self.eat_keyword("lc_collate")
                || self.eat_keyword("lc_ctype")
            {
                self.expect(&Token::Eq)?;
                let value = match self.advance() {
                    Some(Token::StringLit(s))
                    | Some(Token::Word(s))
                    | Some(Token::QuotedIdent(s)) => s,
                    other => {
                        return Err(format!("expected collation option value, found {other:?}"));
                    }
                };
                locale.get_or_insert(value);
            } else if self.eat_keyword("provider")
                || self.eat_keyword("deterministic")
                || self.eat_keyword("version")
            {
                self.expect(&Token::Eq)?;
                self.advance()
                    .ok_or_else(|| "expected collation option value".to_string())?;
            } else {
                return Err("unsupported CREATE COLLATION option".into());
            }
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Token::RParen)?;
        Ok(locale.unwrap_or_else(|| "C".into()))
    }

    /// Parse `CREATE TYPE name AS ENUM (...) | AS (...) | AS RANGE (...)`. The
    /// leading `CREATE TYPE` has already been consumed.
    fn parse_create_type(&mut self) -> Result<Statement, String> {
        let name = self.parse_object_name()?;
        self.expect_keyword("as")?;
        let kind = if self.eat_keyword("enum") {
            self.expect(&Token::LParen)?;
            let mut labels = Vec::new();
            if self.peek() != Some(&Token::RParen) {
                loop {
                    match self.advance() {
                        Some(Token::StringLit(s)) => labels.push(s),
                        other => {
                            return Err(format!("expected enum label string, found {other:?}"));
                        }
                    }
                    if self.eat(&Token::Comma) {
                        continue;
                    }
                    break;
                }
            }
            self.expect(&Token::RParen)?;
            CreateTypeKind::Enum { labels }
        } else if self.eat_keyword("range") {
            self.expect(&Token::LParen)?;
            let mut subtype = DataType::Text;
            loop {
                let key = self.parse_ident()?.to_ascii_lowercase();
                self.expect(&Token::Eq)?;
                if key == "subtype" {
                    subtype = self.parse_data_type()?;
                } else {
                    // Accept and ignore other range options (subtype_opclass, etc.).
                    let _ = self.parse_object_name()?;
                }
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
            self.expect(&Token::RParen)?;
            CreateTypeKind::Range { subtype }
        } else {
            // Composite: `AS (attr type, ...)`.
            self.expect(&Token::LParen)?;
            let mut attributes = Vec::new();
            loop {
                let attr = self.parse_ident()?;
                let ty = self.parse_data_type()?;
                attributes.push((attr, ty));
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
            self.expect(&Token::RParen)?;
            CreateTypeKind::Composite { attributes }
        };
        Ok(Statement::CreateType(CreateType { name, kind }))
    }

    /// Parse `CREATE DOMAIN name [AS] base [NOT NULL | NULL] [CHECK (...)]`.
    /// The leading `CREATE DOMAIN` has already been consumed.
    fn parse_create_domain(&mut self) -> Result<Statement, String> {
        let name = self.parse_object_name()?;
        self.eat_keyword("as");
        let base = self.parse_data_type()?;
        let mut not_null = false;
        let mut check = None;
        loop {
            if self.eat_keyword("not") {
                self.expect_keyword("null")?;
                not_null = true;
            } else if self.eat_keyword("null") {
                // explicit nullable
            } else if self.eat_keyword("default") {
                // Domain defaults are accepted but not applied; swallow the expr.
                let _ = self.parse_expr()?;
            } else if self.eat_keyword("constraint") {
                // Optional constraint name before CHECK.
                let _ = self.parse_ident()?;
            } else if self.eat_keyword("check") {
                self.expect(&Token::LParen)?;
                check = Some(self.parse_expr()?);
                self.expect(&Token::RParen)?;
            } else {
                break;
            }
        }
        Ok(Statement::CreateDomain(CreateDomain {
            name,
            base,
            not_null,
            check,
        }))
    }

    /// Parse a type name, returning both the resolved `DataType` and the raw
    /// lowercased written name (for round-tripping and signature matching).
    fn parse_type_name_str(&mut self) -> Result<(DataType, String), String> {
        let start = self.pos;
        let (dt, raw) = self.parse_data_type_named()?;
        // Reconstruct the written name from the consumed words when the helper
        // resolved a built-in (it only returns the raw name for unknown types).
        let name = match raw {
            Some(n) => n,
            None => {
                // Stitch together the consumed word tokens between start and pos
                // (excluding any parenthesized modifier) into a lowercased name.
                let mut parts = Vec::new();
                for tok in &self.tokens[start..self.pos] {
                    match tok {
                        Token::Word(w) => parts.push(w.to_ascii_lowercase()),
                        Token::LParen => break,
                        _ => {}
                    }
                }
                parts.join(" ")
            }
        };
        Ok((dt, name))
    }

    /// `CREATE [OR REPLACE] FUNCTION name(args) [RETURNS rettype]
    /// AS '<body>' | $$<body>$$ LANGUAGE <lang> [other options...]`.
    fn parse_create_function(&mut self, or_replace: bool) -> Result<Statement, String> {
        let name = self.parse_object_name()?;
        let mut args = Vec::new();
        self.expect(&Token::LParen)?;
        if !self.eat(&Token::RParen) {
            loop {
                // Optional argument mode (IN/OUT/INOUT/VARIADIC): skip it.
                if self.is_keyword("in")
                    || self.is_keyword("out")
                    || self.is_keyword("inout")
                    || self.is_keyword("variadic")
                {
                    self.advance();
                }
                // An optional argument name precedes the type. We can't tell a
                // bare type from a named arg by one token, so peek: if the next
                // token is a word that is NOT immediately a `,`/`)` after, and
                // the token after it starts a type, treat the first as a name.
                let arg_name = if matches!(self.peek(), Some(Token::Word(_)) | Some(Token::QuotedIdent(_)))
                    && !matches!(
                        self.tokens.get(self.pos + 1),
                        Some(Token::Comma) | Some(Token::RParen) | Some(Token::LParen)
                    )
                {
                    Some(self.parse_ident()?)
                } else {
                    None
                };
                let (data_type, type_name) = self.parse_type_name_str()?;
                // Optional DEFAULT expr for the argument: accept and discard.
                if self.eat_keyword("default") || self.eat(&Token::Eq) {
                    let _ = self.parse_expr()?;
                }
                args.push(FunctionArg {
                    name: arg_name,
                    data_type,
                    type_name,
                });
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
            self.expect(&Token::RParen)?;
        }

        let mut return_type = None;
        let mut return_type_name = None;
        if self.eat_keyword("returns") {
            // `RETURNS TABLE (...)` or `RETURNS SETOF type` or a plain type.
            if self.eat_keyword("setof") {
                let (dt, name) = self.parse_type_name_str()?;
                return_type = Some(dt);
                return_type_name = Some(name);
            } else if self.is_keyword("table") {
                self.advance();
                self.skip_balanced_parens()?;
                return_type = Some(DataType::Text);
                return_type_name = Some("record".to_string());
            } else if self.is_keyword("trigger") {
                self.advance();
                return_type = Some(DataType::Text);
                return_type_name = Some("trigger".to_string());
            } else {
                let (dt, name) = self.parse_type_name_str()?;
                return_type = Some(dt);
                return_type_name = Some(name);
            }
        }

        // The remaining clauses (AS, LANGUAGE, and various attributes) may come
        // in any order. Loop collecting them.
        let mut body: Option<String> = None;
        let mut language = "sql".to_string();
        let mut security_definer = false;
        loop {
            if self.eat_keyword("as") {
                body = Some(self.parse_function_body()?);
            } else if self.eat_keyword("language") {
                language = match self.advance() {
                    Some(Token::Word(w)) => w.to_ascii_lowercase(),
                    Some(Token::QuotedIdent(w)) | Some(Token::StringLit(w)) => {
                        w.to_ascii_lowercase()
                    }
                    other => return Err(format!("expected function language, found {other:?}")),
                };
            } else if self.eat_keyword("immutable")
                || self.eat_keyword("stable")
                || self.eat_keyword("volatile")
                || self.eat_keyword("leakproof")
                || self.eat_keyword("strict")
                || self.eat_keyword("window")
                || self.eat_keyword("parallel")
                || self.eat_keyword("cost")
                || self.eat_keyword("rows")
                || self.eat_keyword("support")
            {
                // Accept and discard a trailing simple argument if present.
                if matches!(
                    self.peek(),
                    Some(Token::Word(_)) | Some(Token::Number(_))
                ) && !self.is_keyword("as")
                    && !self.is_keyword("language")
                {
                    self.advance();
                }
            } else if self.eat_keyword("called") {
                self.eat_keyword("on");
                self.eat_keyword("null");
                self.eat_keyword("input");
            } else if self.eat_keyword("returns") {
                // `RETURNS NULL ON NULL INPUT`.
                self.eat_keyword("null");
                self.eat_keyword("on");
                self.eat_keyword("null");
                self.eat_keyword("input");
            } else if self.eat_keyword("security") {
                if self.eat_keyword("definer") {
                    security_definer = true;
                } else if self.eat_keyword("invoker") {
                    security_definer = false;
                }
            } else if self.eat_keyword("external") {
                // `EXTERNAL SECURITY {DEFINER|INVOKER}` (SQL-standard spelling).
                if self.eat_keyword("security") {
                    if self.eat_keyword("definer") {
                        security_definer = true;
                    } else if self.eat_keyword("invoker") {
                        security_definer = false;
                    }
                }
            } else if self.eat_keyword("set") {
                // `SET config = value`: consume the assignment.
                let _ = self.parse_object_name()?;
                if self.eat(&Token::Eq) || self.eat_keyword("to") {
                    let _ = self.advance();
                }
            } else {
                break;
            }
        }

        let body = body.ok_or("CREATE FUNCTION requires an AS body")?;
        Ok(Statement::CreateFunction(CreateFunction {
            name,
            or_replace,
            args,
            return_type,
            return_type_name,
            body,
            language,
            security_definer,
        }))
    }

    /// Read the function body literal following `AS`: a string or dollar-quoted
    /// literal (the lexer hands both back as `StringLit`). A two-part `AS
    /// 'obj', 'sym'` form (C functions) keeps the first part.
    fn parse_function_body(&mut self) -> Result<String, String> {
        let body = match self.advance() {
            Some(Token::StringLit(s)) => s,
            other => return Err(format!("expected function body, found {other:?}")),
        };
        if self.eat(&Token::Comma) {
            // C-language link symbol; accept and discard.
            let _ = self.advance();
        }
        Ok(body)
    }

    /// Consume a balanced `( ... )` group, assuming the opening paren is next.
    fn skip_balanced_parens(&mut self) -> Result<(), String> {
        self.expect(&Token::LParen)?;
        let mut depth = 1;
        while depth > 0 {
            match self.advance() {
                Some(Token::LParen) => depth += 1,
                Some(Token::RParen) => depth -= 1,
                None => return Err("unterminated parenthesised group".into()),
                _ => {}
            }
        }
        Ok(())
    }

    fn parse_create_trigger(&mut self) -> Result<Statement, String> {
        let name = self.parse_ident()?;
        let timing = if self.eat_keyword("before") {
            TriggerTiming::Before
        } else if self.eat_keyword("after") {
            TriggerTiming::After
        } else if self.eat_keyword("instead") {
            self.expect_keyword("of")?;
            // INSTEAD OF triggers are accepted but treated like BEFORE for storage.
            TriggerTiming::Before
        } else {
            return Err("trigger must specify BEFORE, AFTER or INSTEAD OF".into());
        };
        let mut events = Vec::new();
        loop {
            if self.eat_keyword("insert") {
                events.push(TriggerEvent::Insert);
            } else if self.eat_keyword("delete") {
                events.push(TriggerEvent::Delete);
            } else if self.eat_keyword("update") {
                events.push(TriggerEvent::Update);
                // `UPDATE OF col, ...`: accept and discard the column list.
                if self.eat_keyword("of") {
                    loop {
                        let _ = self.parse_ident()?;
                        if self.eat(&Token::Comma) {
                            continue;
                        }
                        break;
                    }
                }
            } else if self.eat_keyword("truncate") {
                // Accepted but never fires (we have no statement-level firing).
            } else {
                return Err("trigger must name at least one event".into());
            }
            if self.eat_keyword("or") {
                continue;
            }
            break;
        }
        self.expect_keyword("on")?;
        let table = self.parse_object_name()?;
        // Optional `FROM reftable`, `NOT DEFERRABLE`, `REFERENCING ...`.
        if self.eat_keyword("from") {
            let _ = self.parse_object_name()?;
        }
        while self.eat_keyword("not")
            || self.eat_keyword("deferrable")
            || self.eat_keyword("initially")
        {
            self.eat_keyword("deferrable");
            self.eat_keyword("immediate");
            self.eat_keyword("deferred");
        }
        if self.eat_keyword("referencing") {
            // `REFERENCING OLD/NEW TABLE AS name ...`: skip to FOR.
            while !self.is_keyword("for") && self.peek().is_some() {
                self.advance();
            }
        }
        let mut for_each_row = false;
        if self.eat_keyword("for") {
            self.eat_keyword("each");
            if self.eat_keyword("row") {
                for_each_row = true;
            } else {
                self.eat_keyword("statement");
            }
        }
        // Optional `WHEN (condition)`: accept and discard.
        if self.eat_keyword("when") {
            self.skip_balanced_parens()?;
        }
        self.expect_keyword("execute")?;
        if !self.eat_keyword("function") {
            self.expect_keyword("procedure")?;
        }
        let function = self.parse_object_name()?;
        // The argument list `()` (possibly with literal args) is accepted.
        if self.peek() == Some(&Token::LParen) {
            self.skip_balanced_parens()?;
        }
        Ok(Statement::CreateTrigger(CreateTrigger {
            name,
            timing,
            events,
            table,
            for_each_row,
            function,
        }))
    }

    fn parse_create_rule(&mut self, or_replace: bool) -> Result<Statement, String> {
        let name = self.parse_ident()?;
        self.expect_keyword("as")?;
        self.expect_keyword("on")?;
        let event = if self.eat_keyword("insert") {
            TriggerEvent::Insert
        } else if self.eat_keyword("update") {
            TriggerEvent::Update
        } else if self.eat_keyword("delete") {
            TriggerEvent::Delete
        } else if self.eat_keyword("select") {
            // SELECT rules (views) are accepted; modelled as an INSERT event tag.
            TriggerEvent::Insert
        } else {
            return Err("rule event must be SELECT, INSERT, UPDATE or DELETE".into());
        };
        self.expect_keyword("to")?;
        let table = self.parse_object_name()?;
        // Capture the verbatim remaining text of the statement as the definition.
        let def_start = self.pos;
        // Optional WHERE and the DO clause: consume to the end of the statement.
        self.discard_statement_tail();
        let definition = self.tokens[def_start..self.pos]
            .iter()
            .map(token_text)
            .collect::<Vec<_>>()
            .join(" ");
        Ok(Statement::CreateRule(CreateRule {
            name,
            or_replace,
            event,
            table,
            definition,
        }))
    }

    fn parse_create_aggregate(&mut self, or_replace: bool) -> Result<Statement, String> {
        let name = self.parse_object_name()?;
        let mut arg_types = Vec::new();
        self.expect(&Token::LParen)?;
        if self.eat(&Token::Star) {
            arg_types.push("*".to_string());
            self.expect(&Token::RParen)?;
        } else if !self.eat(&Token::RParen) {
            loop {
                // Optional arg name before the type, and ORDER BY for ordered-set.
                if self.eat_keyword("order") {
                    self.eat_keyword("by");
                }
                let (_, name) = self.parse_type_name_str()?;
                arg_types.push(name);
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
            self.expect(&Token::RParen)?;
        }
        // The `( SFUNC = ..., STYPE = ..., ... )` option list.
        let mut options = Vec::new();
        self.expect(&Token::LParen)?;
        if !self.eat(&Token::RParen) {
            loop {
                let key = self.parse_ident()?.to_ascii_lowercase();
                self.expect(&Token::Eq)?;
                let value = match self.advance() {
                    Some(Token::Word(w))
                    | Some(Token::QuotedIdent(w))
                    | Some(Token::StringLit(w))
                    | Some(Token::Number(w)) => w,
                    other => return Err(format!("expected aggregate option value, found {other:?}")),
                };
                options.push((key, value));
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
            self.expect(&Token::RParen)?;
        }
        Ok(Statement::CreateAggregate(CreateAggregate {
            name,
            or_replace,
            arg_types,
            options,
        }))
    }

    /// After `CREATE OPERATOR`: disambiguate `CREATE OPERATOR CLASS/FAMILY name`
    /// from `CREATE OPERATOR symbol (...)`.
    fn parse_create_operator_object(&mut self) -> Result<Statement, String> {
        if self.eat_keyword("class") {
            let name = self.parse_object_name()?;
            let definition = self.collect_statement_tail();
            return Ok(Statement::CreateCatalogObject(CatalogObject {
                kind: CatalogObjectKind::OperatorClass,
                name,
                definition,
            }));
        }
        if self.eat_keyword("family") {
            let name = self.parse_object_name()?;
            let definition = self.collect_statement_tail();
            return Ok(Statement::CreateCatalogObject(CatalogObject {
                kind: CatalogObjectKind::OperatorFamily,
                name,
                definition,
            }));
        }
        // `CREATE OPERATOR symbol (LEFTARG=.., ...)`. The operator name is a
        // symbol (or schema-qualified `schema.symbol`); grab everything up to
        // the option-list `(`.
        let name = self.parse_operator_symbol()?;
        let definition = self.collect_statement_tail();
        Ok(Statement::CreateCatalogObject(CatalogObject {
            kind: CatalogObjectKind::Operator,
            name,
            definition,
        }))
    }

    /// Read an operator symbol/name (used by user-defined operators), which may
    /// be a word or a run of operator-symbol tokens, optionally schema-qualified.
    /// Stops at the next `(`.
    fn parse_operator_symbol(&mut self) -> Result<String, String> {
        let mut name = String::new();
        while let Some(tok) = self.peek() {
            if matches!(tok, Token::LParen | Token::Semicolon) {
                break;
            }
            let tok = self.advance().expect("peeked token exists");
            name.push_str(&token_text(&tok));
        }
        if name.is_empty() {
            return Err("expected operator name".into());
        }
        Ok(name)
    }

    /// After `CREATE FOREIGN`: either `DATA WRAPPER name` or `TABLE name (...)`.
    fn parse_create_foreign(&mut self) -> Result<Statement, String> {
        if self.eat_keyword("data") {
            self.expect_keyword("wrapper")?;
            let name = self.parse_object_name()?;
            let definition = self.collect_statement_tail();
            return Ok(Statement::CreateCatalogObject(CatalogObject {
                kind: CatalogObjectKind::ForeignDataWrapper,
                name,
                definition,
            }));
        }
        self.expect_keyword("table")?;
        // `CREATE FOREIGN TABLE name (cols) SERVER s [OPTIONS(...)]`. Parse the
        // column list like a regular table so it appears in catalogs, then
        // swallow the `SERVER ...`/`OPTIONS(...)` tail.
        let if_not_exists = self.parse_if_not_exists();
        let name = self.parse_object_name()?;
        self.expect(&Token::LParen)?;
        let mut columns = Vec::new();
        let mut constraints = Vec::new();
        loop {
            if self.is_keyword("primary")
                || self.is_keyword("unique")
                || self.is_keyword("constraint")
                || self.is_keyword("check")
                || self.is_keyword("foreign")
            {
                constraints.push(self.parse_table_constraint(&name)?);
            } else {
                columns.push(self.parse_column_def()?);
            }
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Token::RParen)?;
        // Discard the `SERVER ... [OPTIONS (...)]` tail; the table itself is
        // stored like an ordinary (empty) table.
        self.discard_statement_tail();
        if columns.is_empty() {
            return Err("foreign table must have at least one column".to_string());
        }
        Ok(Statement::CreateTable(CreateTable {
            name,
            columns,
            constraints,
            if_not_exists,
            persistence: TablePersistence::Permanent,
            inherits: Vec::new(),
            partition_by: None,
            partition_of: None,
        }))
    }

    fn parse_drop_function(&mut self) -> Result<Statement, String> {
        let if_exists = self.parse_if_exists();
        let name = self.parse_object_name()?;
        let arg_types = self.parse_optional_arg_types()?;
        self.eat_keyword("cascade");
        self.eat_keyword("restrict");
        Ok(Statement::DropFunction(DropFunction {
            name,
            if_exists,
            arg_types,
        }))
    }

    fn parse_drop_aggregate(&mut self) -> Result<Statement, String> {
        let if_exists = self.parse_if_exists();
        let name = self.parse_object_name()?;
        let arg_types = self.parse_optional_arg_types()?.unwrap_or_default();
        self.eat_keyword("cascade");
        self.eat_keyword("restrict");
        Ok(Statement::DropAggregate(DropAggregate {
            name,
            if_exists,
            arg_types,
        }))
    }

    /// Parse an optional `(type, ...)` or `(*)` argument-type signature.
    fn parse_optional_arg_types(&mut self) -> Result<Option<Vec<String>>, String> {
        if self.peek() != Some(&Token::LParen) {
            return Ok(None);
        }
        self.advance();
        let mut types = Vec::new();
        if self.eat(&Token::Star) {
            types.push("*".to_string());
            self.expect(&Token::RParen)?;
            return Ok(Some(types));
        }
        if self.eat(&Token::RParen) {
            return Ok(Some(types));
        }
        loop {
            // Skip an optional argument name preceding the type.
            if matches!(self.peek(), Some(Token::Word(_)))
                && !matches!(
                    self.tokens.get(self.pos + 1),
                    Some(Token::Comma) | Some(Token::RParen) | Some(Token::LParen)
                )
            {
                self.advance();
            }
            let (_, name) = self.parse_type_name_str()?;
            types.push(name);
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Token::RParen)?;
        Ok(Some(types))
    }

    fn parse_drop_trigger(&mut self) -> Result<Statement, String> {
        let if_exists = self.parse_if_exists();
        let name = self.parse_ident()?;
        self.expect_keyword("on")?;
        let table = self.parse_object_name()?;
        self.eat_keyword("cascade");
        self.eat_keyword("restrict");
        Ok(Statement::DropTrigger(DropTrigger {
            name,
            table,
            if_exists,
        }))
    }

    fn parse_drop_rule(&mut self) -> Result<Statement, String> {
        let if_exists = self.parse_if_exists();
        let name = self.parse_ident()?;
        self.expect_keyword("on")?;
        let table = self.parse_object_name()?;
        self.eat_keyword("cascade");
        self.eat_keyword("restrict");
        Ok(Statement::DropRule(DropRule {
            name,
            table,
            if_exists,
        }))
    }

    fn parse_drop(&mut self) -> Result<Statement, String> {
        self.expect_keyword("drop")?;
        if self.eat_keyword("role") {
            let if_exists = self.parse_if_exists();
            let name = self.parse_ident()?;
            self.eat_keyword("cascade");
            self.eat_keyword("restrict");
            return Ok(Statement::DropRole(DropRole { name, if_exists }));
        }
        if self.eat_keyword("user") {
            // `DROP USER MAPPING FOR role SERVER s` vs `DROP USER name`.
            if self.eat_keyword("mapping") {
                let if_exists = self.parse_if_exists();
                self.expect_keyword("for")?;
                let name = self.parse_object_name()?;
                let definition = self.collect_statement_tail();
                return Ok(Statement::DropCatalogObject(DropCatalogObject {
                    kind: CatalogObjectKind::UserMapping,
                    name,
                    if_exists,
                    definition,
                }));
            }
            let if_exists = self.parse_if_exists();
            let name = self.parse_ident()?;
            self.eat_keyword("cascade");
            self.eat_keyword("restrict");
            return Ok(Statement::DropRole(DropRole { name, if_exists }));
        }
        if self.eat_keyword("operator") {
            let kind = if self.eat_keyword("class") {
                CatalogObjectKind::OperatorClass
            } else if self.eat_keyword("family") {
                CatalogObjectKind::OperatorFamily
            } else {
                CatalogObjectKind::Operator
            };
            let if_exists = self.parse_if_exists();
            let name = if kind == CatalogObjectKind::Operator {
                self.parse_operator_symbol()?
            } else {
                self.parse_object_name()?
            };
            let definition = self.collect_statement_tail();
            return Ok(Statement::DropCatalogObject(DropCatalogObject {
                kind,
                name,
                if_exists,
                definition,
            }));
        }
        if self.eat_keyword("event") {
            self.expect_keyword("trigger")?;
            let if_exists = self.parse_if_exists();
            let name = self.parse_object_name()?;
            let definition = self.collect_statement_tail();
            return Ok(Statement::DropCatalogObject(DropCatalogObject {
                kind: CatalogObjectKind::EventTrigger,
                name,
                if_exists,
                definition,
            }));
        }
        if self.eat_keyword("foreign") {
            if self.eat_keyword("data") {
                self.expect_keyword("wrapper")?;
                let if_exists = self.parse_if_exists();
                let name = self.parse_object_name()?;
                let definition = self.collect_statement_tail();
                return Ok(Statement::DropCatalogObject(DropCatalogObject {
                    kind: CatalogObjectKind::ForeignDataWrapper,
                    name,
                    if_exists,
                    definition,
                }));
            }
            // `DROP FOREIGN TABLE name` — a foreign table is stored as an
            // ordinary table, so drop it as one.
            self.expect_keyword("table")?;
            let if_exists = self.parse_if_exists();
            let name = self.parse_object_name()?;
            self.eat_keyword("cascade");
            self.eat_keyword("restrict");
            return Ok(Statement::DropTable(DropTable { name, if_exists }));
        }
        if self.eat_keyword("server") {
            let if_exists = self.parse_if_exists();
            let name = self.parse_object_name()?;
            let definition = self.collect_statement_tail();
            return Ok(Statement::DropCatalogObject(DropCatalogObject {
                kind: CatalogObjectKind::Server,
                name,
                if_exists,
                definition,
            }));
        }
        if self.eat_keyword("publication") {
            let if_exists = self.parse_if_exists();
            let name = self.parse_object_name()?;
            let definition = self.collect_statement_tail();
            return Ok(Statement::DropCatalogObject(DropCatalogObject {
                kind: CatalogObjectKind::Publication,
                name,
                if_exists,
                definition,
            }));
        }
        if self.eat_keyword("subscription") {
            let if_exists = self.parse_if_exists();
            let name = self.parse_object_name()?;
            let definition = self.collect_statement_tail();
            return Ok(Statement::DropCatalogObject(DropCatalogObject {
                kind: CatalogObjectKind::Subscription,
                name,
                if_exists,
                definition,
            }));
        }
        if self.eat_keyword("sequence") {
            let if_exists = self.parse_if_exists();
            let name = self.parse_object_name()?;
            self.eat_keyword("cascade");
            self.eat_keyword("restrict");
            return Ok(Statement::DropSequence(DropSequence { name, if_exists }));
        }
        if self.eat_keyword("index") {
            let if_exists = self.parse_if_exists();
            let name = self.parse_object_name()?;
            self.eat_keyword("cascade");
            self.eat_keyword("restrict");
            return Ok(Statement::DropIndex(DropIndex { name, if_exists }));
        }
        if self.eat_keyword("extension") {
            let if_exists = self.parse_if_exists();
            let name = self.parse_object_name()?;
            self.eat_keyword("cascade");
            self.eat_keyword("restrict");
            return Ok(Statement::DropExtension(DropExtension { name, if_exists }));
        }
        if self.eat_keyword("schema") {
            let if_exists = self.parse_if_exists();
            let name = self.parse_object_name()?;
            self.eat_keyword("cascade");
            self.eat_keyword("restrict");
            return Ok(Statement::DropSchema(DropSchema { name, if_exists }));
        }
        if self.eat_keyword("database") {
            let if_exists = self.parse_if_exists();
            let name = self.parse_object_name()?;
            self.discard_statement_tail();
            return Ok(Statement::DropDatabase(DropDatabase { name, if_exists }));
        }
        if self.eat_keyword("tablespace") {
            let if_exists = self.parse_if_exists();
            let name = self.parse_object_name()?;
            self.eat_keyword("cascade");
            self.eat_keyword("restrict");
            return Ok(Statement::DropTablespace(DropTablespace {
                name,
                if_exists,
            }));
        }
        if self.eat_keyword("collation") {
            let if_exists = self.parse_if_exists();
            let name = self.parse_object_name()?;
            self.eat_keyword("cascade");
            self.eat_keyword("restrict");
            return Ok(Statement::DropCollation(DropCollation { name, if_exists }));
        }
        if self.eat_keyword("type") {
            let if_exists = self.parse_if_exists();
            let name = self.parse_object_name()?;
            self.eat_keyword("cascade");
            self.eat_keyword("restrict");
            return Ok(Statement::DropType(DropType { name, if_exists }));
        }
        if self.eat_keyword("domain") {
            let if_exists = self.parse_if_exists();
            let name = self.parse_object_name()?;
            self.eat_keyword("cascade");
            self.eat_keyword("restrict");
            return Ok(Statement::DropDomain(DropDomain { name, if_exists }));
        }
        if self.eat_keyword("view") {
            let if_exists = self.parse_if_exists();
            let name = self.parse_object_name()?;
            self.eat_keyword("cascade");
            self.eat_keyword("restrict");
            return Ok(Statement::DropView(DropView { name, if_exists }));
        }
        if self.eat_keyword("materialized") {
            self.expect_keyword("view")?;
            let if_exists = self.parse_if_exists();
            let name = self.parse_object_name()?;
            self.eat_keyword("cascade");
            self.eat_keyword("restrict");
            return Ok(Statement::DropMaterializedView(DropMaterializedView {
                name,
                if_exists,
            }));
        }
        if self.eat_keyword("function") {
            return self.parse_drop_function();
        }
        if self.eat_keyword("trigger") {
            return self.parse_drop_trigger();
        }
        if self.eat_keyword("rule") {
            return self.parse_drop_rule();
        }
        if self.eat_keyword("aggregate") {
            return self.parse_drop_aggregate();
        }
        if self.eat_keyword("policy") {
            return self.parse_drop_policy();
        }
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

        let overriding_system_value = if self.eat_keyword("overriding") {
            self.expect_keyword("system")?;
            self.expect_keyword("value")?;
            true
        } else {
            false
        };

        let mut select = None;
        let default_values = if self.eat_keyword("default") {
            self.expect_keyword("values")?;
            true
        } else {
            false
        };
        let mut rows = Vec::new();
        if !default_values {
            if self.is_keyword("select") {
                select = Some(Box::new(self.parse_select()?));
            } else {
                self.expect_keyword("values")?;
                rows = self.parse_values_tuples()?;
            }
        }
        let on_conflict = self.parse_on_conflict()?;
        let returning = self.parse_returning()?;
        Ok(Statement::Insert(Insert {
            table,
            columns,
            default_values,
            overriding_system_value,
            rows,
            select,
            on_conflict,
            returning,
        }))
    }

    /// When the current token is `(`, decide whether the parenthesized group is
    /// a query source for `COPY (SELECT ...) TO ...` (begins with
    /// SELECT/WITH/VALUES/TABLE) as opposed to a `COPY table (col, ...)` list.
    fn peek_query_after_lparen(&self) -> bool {
        matches!(
            self.peek_at(1),
            Some(Token::Word(w))
                if w.eq_ignore_ascii_case("select")
                    || w.eq_ignore_ascii_case("with")
                    || w.eq_ignore_ascii_case("values")
                    || w.eq_ignore_ascii_case("table")
        )
    }

    fn parse_copy(&mut self) -> Result<Statement, String> {
        self.expect_keyword("copy")?;

        // `COPY (SELECT ...) TO <dst>`: a parenthesized query source. We must
        // distinguish this from `COPY table (col, ...) ...` — the query form
        // begins with SELECT/WITH/VALUES/TABLE inside the parens.
        let mut query: Option<Box<Select>> = None;
        let mut table = String::new();
        if self.peek() == Some(&Token::LParen) && self.peek_query_after_lparen() {
            self.advance();
            let select = self.parse_select()?;
            self.expect(&Token::RParen)?;
            query = Some(Box::new(select));
        } else {
            table = self.parse_object_name()?;
        }

        let columns = if query.is_none() && self.peek() == Some(&Token::LParen) {
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

        let direction = if self.eat_keyword("from") {
            CopyDirection::From
        } else if self.eat_keyword("to") {
            CopyDirection::To
        } else {
            return Err("expected FROM or TO in COPY".into());
        };

        if query.is_some() && direction == CopyDirection::From {
            return Err("COPY FROM not supported with a query source".into());
        }

        // The endpoint: STDIN/STDOUT (COPY sub-protocol) or a single-quoted
        // server-side file path.
        let target = match direction {
            CopyDirection::From if self.eat_keyword("stdin") => CopyTarget::Stdin,
            CopyDirection::To if self.eat_keyword("stdout") => CopyTarget::Stdout,
            _ => match self.advance() {
                Some(Token::StringLit(path)) => CopyTarget::File(path),
                other => {
                    return Err(format!(
                        "expected STDIN/STDOUT or a file path in COPY, got {other:?}"
                    ))
                }
            },
        };

        let mut copy = Copy {
            table,
            query,
            columns,
            direction,
            target,
            format: CopyFormat::Text,
            delimiter: None,
            header: false,
            null: None,
        };

        self.eat_keyword("with");
        if self.eat(&Token::LParen) {
            // New-style option list: COPY ... WITH (FORMAT csv, DELIMITER ',', ...).
            while !self.eat(&Token::RParen) {
                self.parse_copy_option(&mut copy)?;
                self.eat(&Token::Comma);
            }
        } else {
            // Legacy options: WITH DELIMITER ',' CSV HEADER NULL 'x'.
            loop {
                if self.is_keyword("delimiter")
                    || self.is_keyword("csv")
                    || self.is_keyword("binary")
                    || self.is_keyword("header")
                    || self.is_keyword("null")
                    || self.is_keyword("format")
                {
                    self.parse_copy_option(&mut copy)?;
                } else {
                    break;
                }
            }
        }
        Ok(Statement::Copy(copy))
    }

    /// Parse one COPY option (shared by the legacy and parenthesized forms).
    fn parse_copy_option(&mut self, copy: &mut Copy) -> Result<(), String> {
        if self.eat_keyword("format") {
            let fmt = self.parse_ident()?;
            copy.format = match fmt.to_ascii_lowercase().as_str() {
                "text" => CopyFormat::Text,
                "csv" => CopyFormat::Csv,
                "binary" => CopyFormat::Binary,
                other => return Err(format!("unsupported COPY format: {other}")),
            };
        } else if self.eat_keyword("csv") {
            copy.format = CopyFormat::Csv;
        } else if self.eat_keyword("binary") {
            copy.format = CopyFormat::Binary;
        } else if self.eat_keyword("delimiter") {
            match self.advance() {
                Some(Token::StringLit(s)) if s.chars().count() == 1 => {
                    copy.delimiter = s.chars().next();
                }
                other => return Err(format!("COPY DELIMITER must be a single character, got {other:?}")),
            }
        } else if self.eat_keyword("header") {
            // Optional boolean argument; default true when bare.
            copy.header = !(self.eat_keyword("false") || self.eat_keyword("off"));
            self.eat_keyword("true");
            self.eat_keyword("on");
        } else if self.eat_keyword("null") {
            match self.advance() {
                Some(Token::StringLit(s)) => copy.null = Some(s),
                other => return Err(format!("COPY NULL must be a string, got {other:?}")),
            }
        } else {
            return Err(format!("unsupported COPY option near {:?}", self.peek()));
        }
        Ok(())
    }

    fn parse_truncate(&mut self) -> Result<Statement, String> {
        self.expect_keyword("truncate")?;
        self.eat_keyword("table");
        let mut tables = Vec::new();
        loop {
            tables.push(self.parse_object_name()?);
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        if self.eat_keyword("restart") || self.eat_keyword("continue") {
            self.expect_keyword("identity")?;
        }
        self.eat_keyword("cascade");
        self.eat_keyword("restrict");
        Ok(Statement::Truncate(Truncate { tables }))
    }

    fn parse_declare_cursor(&mut self) -> Result<Statement, String> {
        self.expect_keyword("declare")?;
        let name = self.parse_ident()?;
        while self.eat_keyword("binary")
            || self.eat_keyword("insensitive")
            || self.eat_keyword("scroll")
            || (self.eat_keyword("no") && {
                self.expect_keyword("scroll")?;
                true
            })
        {}
        self.expect_keyword("cursor")?;
        if self.eat_keyword("with") || self.eat_keyword("without") {
            self.expect_keyword("hold")?;
        }
        self.expect_keyword("for")?;
        let select = self.parse_select()?;
        Ok(Statement::DeclareCursor(DeclareCursor {
            name,
            select: Box::new(select),
        }))
    }

    fn parse_fetch(&mut self) -> Result<Statement, String> {
        self.expect_keyword("fetch")?;
        let count = if self.eat_keyword("next") {
            FetchCount::Next
        } else if self.eat_keyword("all") {
            FetchCount::All
        } else if let Some(Token::Number(s)) = self.peek().cloned() {
            self.advance();
            FetchCount::Count(
                s.parse::<i64>()
                    .map_err(|_| format!("invalid FETCH count: {s}"))?,
            )
        } else {
            FetchCount::Next
        };
        let _ = self.eat_keyword("from") || self.eat_keyword("in");
        let cursor = self.parse_ident()?;
        Ok(Statement::Fetch(Fetch { cursor, count }))
    }

    fn parse_on_conflict(&mut self) -> Result<Option<OnConflict>, String> {
        if !self.eat_keyword("on") {
            return Ok(None);
        }
        self.expect_keyword("conflict")?;
        let mut target = Vec::new();
        if self.eat(&Token::LParen) {
            loop {
                target.push(self.parse_ident()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
            self.expect(&Token::RParen)?;
        }
        self.expect_keyword("do")?;
        if self.eat_keyword("nothing") {
            Ok(Some(OnConflict::DoNothing { target }))
        } else if self.eat_keyword("update") {
            self.expect_keyword("set")?;
            let mut assignments = Vec::new();
            loop {
                let name = self.parse_ident()?;
                self.expect(&Token::Eq)?;
                let expr = self.parse_expr()?;
                assignments.push((name, expr));
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
            Ok(Some(OnConflict::DoUpdate {
                target,
                assignments,
                filter,
            }))
        } else {
            Err("expected NOTHING or UPDATE after ON CONFLICT DO".into())
        }
    }

    fn parse_values_tuples(&mut self) -> Result<Vec<Vec<Expr>>, String> {
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
        Ok(rows)
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
        let ctes = if self.is_keyword("with") {
            self.parse_with_clause()?
        } else {
            Vec::new()
        };
        let mut select = self.parse_select_core()?;
        select.ctes = ctes;
        while let Some(op) = self.eat_set_operator() {
            let all = self.eat_keyword("all");
            let rhs = self.parse_select_core()?;
            select.set_ops.push(SetOperation {
                op,
                all,
                select: Box::new(rhs),
            });
        }
        self.parse_select_tail(&mut select)?;
        Ok(select)
    }

    fn parse_with_clause(&mut self) -> Result<Vec<Cte>, String> {
        self.expect_keyword("with")?;
        // `RECURSIVE` applies to the whole WITH block; flag every CTE in it.
        let recursive = self.eat_keyword("recursive");
        let mut ctes = Vec::new();
        loop {
            let name = self.parse_ident()?;
            let mut columns = Vec::new();
            if self.eat(&Token::LParen) {
                loop {
                    columns.push(self.parse_ident()?);
                    if self.eat(&Token::Comma) {
                        continue;
                    }
                    break;
                }
                self.expect(&Token::RParen)?;
            }
            self.expect_keyword("as")?;
            self.expect(&Token::LParen)?;
            // A CTE body may be a data-modifying statement (writable CTE) or an
            // ordinary SELECT.
            let (select, dml) = if self.is_keyword("insert")
                || self.is_keyword("update")
                || self.is_keyword("delete")
            {
                let stmt = self.parse_statement()?;
                (Box::new(Select::default()), Some(Box::new(stmt)))
            } else {
                (Box::new(self.parse_select()?), None)
            };
            self.expect(&Token::RParen)?;
            ctes.push(Cte {
                name,
                columns,
                select,
                dml,
                recursive,
            });
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        Ok(ctes)
    }

    fn parse_select_core(&mut self) -> Result<Select, String> {
        // `VALUES (...), (...)` as a query operand. Each tuple becomes a
        // FROM-less SELECT projection (column names `column1`, `column2`, ...);
        // multiple tuples are folded into a `UNION ALL` chain.
        if self.is_keyword("values") {
            self.advance();
            let tuples = self.parse_values_tuples()?;
            let mut iter = tuples.into_iter();
            let first = iter.next().unwrap_or_default();
            let mut select = Select {
                projection: first
                    .into_iter()
                    .enumerate()
                    .map(|(i, e)| SelectItem::Expr {
                        expr: e,
                        alias: Some(format!("column{}", i + 1)),
                    })
                    .collect(),
                ..Select::default()
            };
            for tuple in iter {
                let rhs = Select {
                    projection: tuple
                        .into_iter()
                        .map(|e| SelectItem::Expr { expr: e, alias: None })
                        .collect(),
                    ..Select::default()
                };
                select.set_ops.push(SetOperation {
                    op: SetOperator::Union,
                    all: true,
                    select: Box::new(rhs),
                });
            }
            return Ok(select);
        }
        self.expect_keyword("select")?;
        // `ALL` is the default; `DISTINCT` deduplicates.
        self.eat_keyword("all");
        let distinct = self.eat_keyword("distinct");
        let mut distinct_on = Vec::new();
        if distinct && self.eat_keyword("on") {
            self.expect(&Token::LParen)?;
            loop {
                distinct_on.push(self.parse_expr()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
            self.expect(&Token::RParen)?;
        }

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
        let mut grouping_sets: Vec<Vec<Expr>> = Vec::new();
        if self.eat_keyword("group") {
            self.expect_keyword("by")?;
            // Each top-level comma-separated GROUP BY element contributes a list
            // of grouping sets; the overall result is their cross-product. A
            // plain expression `a` contributes the single set `[[a]]`.
            let mut elements: Vec<Vec<Vec<Expr>>> = Vec::new();
            let mut any_grouping = false;
            loop {
                if self.is_keyword("rollup") || self.is_keyword("cube") {
                    any_grouping = true;
                    let cube = self.eat_keyword("cube");
                    if !cube {
                        self.expect_keyword("rollup")?;
                    }
                    let cols = self.parse_grouping_paren_list()?;
                    elements.push(if cube {
                        cube_sets(&cols)
                    } else {
                        rollup_sets(&cols)
                    });
                } else if self.is_keyword("grouping") {
                    any_grouping = true;
                    self.expect_keyword("grouping")?;
                    self.expect_keyword("sets")?;
                    self.expect(&Token::LParen)?;
                    let mut sets: Vec<Vec<Expr>> = Vec::new();
                    loop {
                        // Each entry is `( a, b )`, `()`, or a bare expression.
                        if self.eat(&Token::LParen) {
                            sets.push(self.parse_expr_list_until_rparen()?);
                        } else {
                            sets.push(vec![self.parse_expr()?]);
                        }
                        if self.eat(&Token::Comma) {
                            continue;
                        }
                        break;
                    }
                    self.expect(&Token::RParen)?;
                    elements.push(sets);
                } else {
                    elements.push(vec![vec![self.parse_expr()?]]);
                }
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }

            if !any_grouping {
                // Ordinary GROUP BY: flatten the single-expression elements.
                for el in elements {
                    group_by.push(el.into_iter().next().unwrap().into_iter().next().unwrap());
                }
            } else {
                // Cross-product the per-element grouping-set lists.
                grouping_sets = vec![Vec::new()];
                for el in &elements {
                    let mut next: Vec<Vec<Expr>> = Vec::new();
                    for prefix in &grouping_sets {
                        for set in el {
                            let mut combined = prefix.clone();
                            combined.extend(set.iter().cloned());
                            next.push(combined);
                        }
                    }
                    grouping_sets = next;
                }
            }
        }

        let having = if self.eat_keyword("having") {
            Some(self.parse_expr()?)
        } else {
            None
        };

        Ok(Select {
            ctes: Vec::new(),
            distinct,
            distinct_on,
            projection,
            from,
            filter,
            group_by,
            grouping_sets,
            having,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            locking: Vec::new(),
            set_ops: Vec::new(),
        })
    }

    /// Parse a comma-separated `ORDER BY` item list (the `ORDER BY` keywords
    /// have already been consumed by the caller).
    fn parse_order_by_items(&mut self) -> Result<Vec<OrderByItem>, String> {
        let mut order_by = Vec::new();
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
        Ok(order_by)
    }

    fn parse_select_tail(&mut self, select: &mut Select) -> Result<(), String> {
        let mut order_by = Vec::new();
        if self.eat_keyword("order") {
            self.expect_keyword("by")?;
            order_by = self.parse_order_by_items()?;
        }
        select.order_by = order_by;
        while self.is_keyword("for") {
            select.locking.push(self.parse_row_locking_clause()?);
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
        select.limit = limit;
        select.offset = offset;
        while self.is_keyword("for") {
            select.locking.push(self.parse_row_locking_clause()?);
        }
        Ok(())
    }

    fn parse_row_locking_clause(&mut self) -> Result<RowLockingClause, String> {
        self.expect_keyword("for")?;
        let mode = if self.eat_keyword("no") {
            self.expect_keyword("key")?;
            self.expect_keyword("update")?;
            RowLockingMode::NoKeyUpdate
        } else if self.eat_keyword("key") {
            self.expect_keyword("share")?;
            RowLockingMode::KeyShare
        } else if self.eat_keyword("share") {
            RowLockingMode::Share
        } else {
            self.expect_keyword("update")?;
            RowLockingMode::Update
        };
        let mut tables = Vec::new();
        if self.eat_keyword("of") {
            loop {
                tables.push(self.parse_object_name()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
        }
        let wait_policy = if self.eat_keyword("nowait") {
            Some(RowLockingWaitPolicy::NoWait)
        } else if self.eat_keyword("skip") {
            self.expect_keyword("locked")?;
            Some(RowLockingWaitPolicy::SkipLocked)
        } else {
            None
        };
        Ok(RowLockingClause {
            mode,
            tables,
            wait_policy,
        })
    }

    fn eat_set_operator(&mut self) -> Option<SetOperator> {
        if self.eat_keyword("union") {
            Some(SetOperator::Union)
        } else if self.eat_keyword("intersect") {
            Some(SetOperator::Intersect)
        } else if self.eat_keyword("except") {
            Some(SetOperator::Except)
        } else {
            None
        }
    }

    /// Parse `FROM base [alias] (JOIN ...)*`.
    fn parse_from_clause(&mut self) -> Result<FromClause, String> {
        let base = self.parse_table_ref()?;
        let mut joins = Vec::new();
        loop {
            // A comma in FROM is a CROSS JOIN of the next table reference (which
            // may be `LATERAL (...)`, referencing earlier items).
            if self.eat(&Token::Comma) {
                let table = self.parse_table_ref()?;
                joins.push(Join {
                    kind: JoinKind::Cross,
                    table,
                    on: None,
                });
                continue;
            }
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
        // `LATERAL` prefix: the following subquery / function may reference
        // columns from preceding FROM items.
        let lateral = self.eat_keyword("lateral");

        // A parenthesised subquery in FROM (derived table): `(SELECT ...) [AS] a`.
        if self.peek() == Some(&Token::LParen) {
            // Peek past the paren to distinguish `(SELECT ...)` from a name.
            if matches!(self.peek_at(1), Some(Token::Word(w)) if w.eq_ignore_ascii_case("select") || w.eq_ignore_ascii_case("with"))
            {
                self.expect(&Token::LParen)?;
                let sub = self.parse_select()?;
                self.expect(&Token::RParen)?;
                let alias = self.parse_optional_table_alias()?;
                let name = alias.clone().unwrap_or_default();
                // Optional column-alias list for the derived table.
                let mut column_aliases = Vec::new();
                if self.eat(&Token::LParen) {
                    loop {
                        column_aliases.push(self.parse_ident()?);
                        if self.eat(&Token::Comma) {
                            continue;
                        }
                        break;
                    }
                    self.expect(&Token::RParen)?;
                }
                return Ok(TableRef {
                    schema: None,
                    name,
                    args: Vec::new(),
                    alias,
                    subquery: Some(Box::new(sub)),
                    lateral,
                    only: false,
                    column_aliases,
                    with_ordinality: false,
                });
            }
        }

        // `ONLY t` restricts a scan to the named table's own rows (no
        // inheritance children / partitions).
        let only = self.eat_keyword("only");
        let (schema, name) = self.parse_qualified_name()?;
        let args = if self.eat(&Token::LParen) {
            self.parse_call_args()?
        } else {
            Vec::new()
        };
        // `WITH ORDINALITY` on a set-returning function appends a 1-based
        // ordinality column to the output.
        let mut with_ordinality = false;
        if self.is_keyword("with") && self.is_keyword_at(1, "ordinality") {
            self.advance();
            self.advance();
            with_ordinality = true;
        }
        let alias = self.parse_optional_table_alias()?;
        // Optional column-alias list: `AS a(c1, c2)` / `a(c1, c2)`.
        let mut column_aliases = Vec::new();
        if self.eat(&Token::LParen) {
            loop {
                column_aliases.push(self.parse_ident()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
            self.expect(&Token::RParen)?;
        }
        Ok(TableRef {
            schema,
            name,
            args,
            alias,
            subquery: None,
            lateral,
            only,
            column_aliases,
            with_ordinality,
        })
    }

    /// Parse an optional table alias (`AS a` / bare `a`), skipping query
    /// keywords that would otherwise be mistaken for an alias.
    fn parse_optional_table_alias(&mut self) -> Result<Option<String>, String> {
        if self.eat_keyword("as") {
            Ok(Some(self.parse_ident()?))
        } else if let Some(Token::Word(w)) = self.peek() {
            if is_table_ref_keyword(w) {
                Ok(None)
            } else {
                Ok(Some(self.parse_ident()?))
            }
        } else {
            Ok(None)
        }
    }

    /// Parse an optional interval field qualifier following an interval literal,
    /// e.g. `YEAR TO MONTH`, `DAY TO SECOND`, or a single `DAY`. Returns the
    /// canonical lower-case form joined by spaces, or `None` if absent.
    fn parse_interval_qualifier(&mut self) -> Option<String> {
        const FIELDS: &[&str] = &[
            "year", "month", "day", "hour", "minute", "second",
        ];
        let is_field = |p: Option<&Token>| match p {
            Some(Token::Word(w)) => FIELDS.contains(&w.to_ascii_lowercase().as_str()),
            _ => false,
        };
        if !is_field(self.peek()) {
            return None;
        }
        let Some(Token::Word(first)) = self.advance() else {
            return None;
        };
        let mut q = first.to_ascii_lowercase();
        if self.eat_keyword("to") {
            if let Some(Token::Word(second)) = self.advance() {
                q.push_str(" to ");
                q.push_str(&second.to_ascii_lowercase());
            }
        }
        Some(q)
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
        let returning = self.parse_returning()?;
        Ok(Statement::Update(Update {
            table,
            assignments,
            from,
            filter,
            returning,
        }))
    }

    fn parse_delete(&mut self) -> Result<Statement, String> {
        self.expect_keyword("delete")?;
        self.expect_keyword("from")?;
        let table = self.parse_object_name()?;
        let using = if self.eat_keyword("using") {
            Some(self.parse_from_clause()?)
        } else {
            None
        };
        let filter = if self.eat_keyword("where") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let returning = self.parse_returning()?;
        Ok(Statement::Delete(Delete {
            table,
            using,
            filter,
            returning,
        }))
    }

    fn parse_merge(&mut self) -> Result<Statement, String> {
        self.expect_keyword("merge")?;
        self.expect_keyword("into")?;
        let target = self.parse_object_name()?;
        let target_alias = self.parse_optional_alias();

        self.expect_keyword("using")?;
        let source = self.parse_merge_source()?;

        self.expect_keyword("on")?;
        let on = self.parse_expr()?;

        let mut clauses = Vec::new();
        while self.eat_keyword("when") {
            clauses.push(self.parse_merge_when()?);
        }
        if clauses.is_empty() {
            return Err("MERGE requires at least one WHEN clause".into());
        }

        Ok(Statement::Merge(Merge {
            target,
            target_alias,
            source,
            on,
            clauses,
        }))
    }

    /// Parse an optional `[AS] alias` after a relation, rejecting words that
    /// continue the MERGE grammar (`USING`/`ON`).
    fn parse_optional_alias(&mut self) -> Option<String> {
        if self.eat_keyword("as") {
            return self.parse_ident().ok();
        }
        if let Some(Token::Word(w)) = self.peek() {
            if is_merge_keyword(w) {
                return None;
            }
            return self.parse_ident().ok();
        }
        None
    }

    fn parse_merge_source(&mut self) -> Result<MergeSource, String> {
        if self.eat(&Token::LParen) {
            // Either `(VALUES ...)` or `(SELECT ...)`, followed by `AS alias`.
            let rows = if self.is_keyword("values") {
                self.expect_keyword("values")?;
                Some(self.parse_values_tuples()?)
            } else {
                None
            };
            let select = if rows.is_none() {
                Some(self.parse_select()?)
            } else {
                None
            };
            self.expect(&Token::RParen)?;
            self.eat_keyword("as");
            let alias = self.parse_ident()?;
            // Optional column alias list `(col, ...)`.
            let mut columns = Vec::new();
            if self.eat(&Token::LParen) {
                loop {
                    columns.push(self.parse_ident()?);
                    if self.eat(&Token::Comma) {
                        continue;
                    }
                    break;
                }
                self.expect(&Token::RParen)?;
            }
            if let Some(rows) = rows {
                Ok(MergeSource::Values {
                    rows,
                    alias,
                    columns,
                })
            } else {
                Ok(MergeSource::Subquery {
                    select: Box::new(select.expect("select parsed when not VALUES")),
                    alias,
                })
            }
        } else {
            let name = self.parse_object_name()?;
            let alias = self.parse_optional_alias();
            Ok(MergeSource::Table { name, alias })
        }
    }

    fn parse_merge_when(&mut self) -> Result<MergeWhen, String> {
        let matched = if self.eat_keyword("not") {
            self.expect_keyword("matched")?;
            false
        } else {
            self.expect_keyword("matched")?;
            true
        };
        let condition = if self.eat_keyword("and") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect_keyword("then")?;
        let action = self.parse_merge_action(matched)?;
        Ok(MergeWhen {
            matched,
            condition,
            action,
        })
    }

    fn parse_merge_action(&mut self, matched: bool) -> Result<MergeAction, String> {
        if self.eat_keyword("do") {
            self.expect_keyword("nothing")?;
            return Ok(MergeAction::DoNothing);
        }
        if matched {
            if self.eat_keyword("update") {
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
                Ok(MergeAction::Update { assignments })
            } else if self.eat_keyword("delete") {
                Ok(MergeAction::Delete)
            } else {
                Err("expected UPDATE, DELETE or DO NOTHING after WHEN MATCHED THEN".into())
            }
        } else {
            self.expect_keyword("insert")?;
            if self.eat_keyword("default") {
                self.expect_keyword("values")?;
                return Ok(MergeAction::Insert {
                    columns: None,
                    values: Vec::new(),
                    default_values: true,
                });
            }
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
            self.expect(&Token::LParen)?;
            let mut values = Vec::new();
            if self.peek() != Some(&Token::RParen) {
                loop {
                    values.push(self.parse_expr()?);
                    if self.eat(&Token::Comma) {
                        continue;
                    }
                    break;
                }
            }
            self.expect(&Token::RParen)?;
            Ok(MergeAction::Insert {
                columns,
                values,
                default_values: false,
            })
        }
    }

    /// Parse a comma-separated list of transaction modes following `BEGIN`,
    /// `START TRANSACTION`, or `SET TRANSACTION`: `ISOLATION LEVEL <lvl>`,
    /// `READ ONLY`, `READ WRITE`, `[NOT] DEFERRABLE` (the last accepted as a
    /// no-op). Returns the isolation level and read-only flag if specified.
    fn parse_transaction_modes(
        &mut self,
    ) -> Result<(Option<IsolationLevel>, Option<bool>), String> {
        let mut isolation = None;
        let mut read_only = None;
        loop {
            if self.eat_keyword("isolation") {
                self.expect_keyword("level")?;
                isolation = Some(self.parse_isolation_level()?);
            } else if self.eat_keyword("read") {
                if self.eat_keyword("only") {
                    read_only = Some(true);
                } else if self.eat_keyword("write") {
                    read_only = Some(false);
                } else {
                    return Err("expected ONLY or WRITE after READ".into());
                }
            } else if self.eat_keyword("deferrable") {
                // accepted no-op
            } else if self.eat_keyword("not") {
                self.expect_keyword("deferrable")?;
            } else {
                break;
            }
            // Modes may be separated by an optional comma.
            self.eat(&Token::Comma);
        }
        Ok((isolation, read_only))
    }

    /// Parse an isolation level name (`READ COMMITTED`, `READ UNCOMMITTED`,
    /// `REPEATABLE READ`, `SERIALIZABLE`).
    fn parse_isolation_level(&mut self) -> Result<IsolationLevel, String> {
        if self.eat_keyword("serializable") {
            Ok(IsolationLevel::Serializable)
        } else if self.eat_keyword("repeatable") {
            self.expect_keyword("read")?;
            Ok(IsolationLevel::RepeatableRead)
        } else if self.eat_keyword("read") {
            if self.eat_keyword("committed") {
                Ok(IsolationLevel::ReadCommitted)
            } else if self.eat_keyword("uncommitted") {
                Ok(IsolationLevel::ReadUncommitted)
            } else {
                Err("expected COMMITTED or UNCOMMITTED after READ".into())
            }
        } else {
            Err("expected a transaction isolation level".into())
        }
    }

    fn parse_set(&mut self) -> Result<Statement, String> {
        self.expect_keyword("set")?;
        // `SET CONSTRAINTS { ALL | name[,...] } { DEFERRED | IMMEDIATE }` —
        // accepted as a no-op (no deferred-constraint machinery).
        if self.eat_keyword("constraints") {
            self.discard_statement_tail();
            return Ok(Statement::SetConstraints);
        }
        // `SET SESSION CHARACTERISTICS AS TRANSACTION <modes>` sets the session
        // default; `SET [SESSION|LOCAL] TRANSACTION <modes>` sets the current
        // transaction.
        if self.eat_keyword("session") {
            if self.eat_keyword("characteristics") {
                self.expect_keyword("as")?;
                self.expect_keyword("transaction")?;
                let (isolation, read_only) = self.parse_transaction_modes()?;
                return Ok(Statement::SetTransaction {
                    isolation,
                    read_only,
                    session: true,
                });
            }
            if self.eat_keyword("transaction") {
                let (isolation, read_only) = self.parse_transaction_modes()?;
                return Ok(Statement::SetTransaction {
                    isolation,
                    read_only,
                    session: false,
                });
            }
            // `SET SESSION name = value` — fall through to the generic GUC path.
            let name = self.parse_guc_name()?;
            return self.finish_set_guc(name, false);
        }
        if self.eat_keyword("transaction") {
            let (isolation, read_only) = self.parse_transaction_modes()?;
            return Ok(Statement::SetTransaction {
                isolation,
                read_only,
                session: false,
            });
        }
        // `SET [LOCAL] name {=|TO} value`.
        let local = self.eat_keyword("local");
        if self.eat_keyword("transaction") {
            let (isolation, read_only) = self.parse_transaction_modes()?;
            return Ok(Statement::SetTransaction {
                isolation,
                read_only,
                session: false,
            });
        }
        let name = self.parse_guc_name()?;
        self.finish_set_guc(name, local)
    }

    /// Finish parsing a generic `SET name {=|TO} value` after the name.
    fn finish_set_guc(&mut self, name: String, local: bool) -> Result<Statement, String> {
        let _ = self.eat(&Token::Eq) || self.eat_keyword("to");
        // `SET name TO DEFAULT` / `SET name = DEFAULT` resets the parameter.
        if self.eat_keyword("default") {
            return Ok(Statement::ResetConfig { name: Some(name) });
        }
        let value = self.parse_setting_value();
        Ok(Statement::Set { name, value, local })
    }

    /// Parse a (possibly dotted, e.g. `myapp.setting`) GUC parameter name,
    /// preserving every segment joined by `.` (unlike [`parse_object_name`],
    /// which keeps only the trailing identifier).
    fn parse_guc_name(&mut self) -> Result<String, String> {
        let mut name = self.parse_ident()?;
        while self.eat(&Token::Dot) {
            name.push('.');
            name.push_str(&self.parse_ident()?);
        }
        Ok(name)
    }

    fn parse_refresh(&mut self) -> Result<Statement, String> {
        self.expect_keyword("refresh")?;
        self.expect_keyword("materialized")?;
        self.expect_keyword("view")?;
        self.eat_keyword("concurrently");
        let name = self.parse_object_name()?;
        return Ok(Statement::RefreshMaterializedView(
            RefreshMaterializedView { name },
        ));
    }

    fn parse_setting_value(&mut self) -> String {
        let mut out = String::new();
        while let Some(tok) = self.peek() {
            if matches!(tok, Token::Semicolon) {
                break;
            }
            let piece = match self.advance().expect("peeked token exists") {
                Token::Word(s) | Token::QuotedIdent(s) | Token::StringLit(s) | Token::Number(s) => {
                    s
                }
                Token::Comma => ",".into(),
                Token::Dot => ".".into(),
                Token::LBracket => "[".into(),
                Token::RBracket => "]".into(),
                Token::Eq => "=".into(),
                Token::Star => "*".into(),
                Token::Plus => "+".into(),
                Token::Minus => "-".into(),
                Token::Arrow => "->".into(),
                Token::ArrowText => "->>".into(),
                Token::ArrayContains => "@>".into(),
                Token::TextSearchMatch => "@@".into(),
                Token::ArrayContainedBy => "<@".into(),
                Token::ArrayOverlap => "&&".into(),
                Token::NetworkContainedBy => "<<".into(),
                Token::NetworkContainedByEq => "<<=".into(),
                Token::NetworkContains => ">>".into(),
                Token::NetworkContainsEq => ">>=".into(),
                Token::Slash => "/".into(),
                Token::Percent => "%".into(),
                Token::Param(n) => format!("${n}"),
                Token::LParen => "(".into(),
                Token::RParen => ")".into(),
                Token::NotEq => "!=".into(),
                Token::Lt => "<".into(),
                Token::LtEq => "<=".into(),
                Token::Gt => ">".into(),
                Token::GtEq => ">=".into(),
                Token::Concat => "||".into(),
                Token::DoubleColon => "::".into(),
                Token::Match => "~".into(),
                Token::MatchCi => "~*".into(),
                Token::NotMatch => "!~".into(),
                Token::NotMatchCi => "!~*".into(),
                Token::Semicolon => unreachable!(),
            };
            if piece == "," {
                out.push(',');
                out.push(' ');
            } else {
                if !out.is_empty()
                    && !matches!(out.chars().last(), Some(' ' | ',' | '.' | '(' | ':'))
                {
                    out.push(' ');
                }
                out.push_str(&piece);
            }
        }
        out.trim().to_string()
    }

    fn discard_statement_tail(&mut self) {
        while let Some(tok) = self.peek() {
            if matches!(tok, Token::Semicolon) {
                break;
            }
            self.advance();
        }
    }

    /// Consume the remaining tokens of the current statement (up to a `;` or
    /// end of input) and render them back to canonical SQL text. Used to keep
    /// the verbatim definition of accept-and-store catalog statements so they
    /// round-trip through the WAL. String literals are re-quoted so the result
    /// re-parses identically.
    fn collect_statement_tail(&mut self) -> String {
        let mut out = String::new();
        let mut prev_open_paren = false;
        while let Some(tok) = self.peek() {
            if matches!(tok, Token::Semicolon) {
                break;
            }
            let tok = self.advance().expect("peeked token exists");
            // Glue against `(` `.` and before `,` `)` `.` to keep the rendering
            // re-parseable and tidy.
            let glue_left = matches!(tok, Token::Comma | Token::RParen | Token::Dot);
            if !out.is_empty() && !glue_left && !prev_open_paren {
                out.push(' ');
            }
            out.push_str(&token_text(&tok));
            prev_open_paren = matches!(tok, Token::LParen | Token::Dot);
        }
        out
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

    // --- expressions (precedence climbing) -----------------------------------

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_expr_bp(0)
    }

    /// Parse an expression with binding power at least `min_bp`.
    fn parse_expr_bp(&mut self, min_bp: u8) -> Result<Expr, String> {
        let mut lhs = self.parse_prefix()?;

        loop {
            // Postfix: IS [NOT] NULL / IS [NOT] DISTINCT FROM.
            if self.is_keyword("is") {
                let (l_bp, _) = (7, 7);
                if l_bp < min_bp {
                    break;
                }
                self.advance();
                let negated = self.eat_keyword("not");
                if self.eat_keyword("null") {
                    lhs = Expr::IsNull {
                        expr: Box::new(lhs),
                        negated,
                    };
                } else {
                    self.expect_keyword("distinct")?;
                    self.expect_keyword("from")?;
                    let rhs = self.parse_expr_bp(8)?;
                    lhs = Expr::IsDistinctFrom {
                        left: Box::new(lhs),
                        right: Box::new(rhs),
                        negated,
                    };
                }
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
                lhs = Expr::Binary {
                    op,
                    left: Box::new(lhs),
                    right: Box::new(rhs),
                };
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
            if let Some(quantifier) = self.eat_quantifier() {
                self.expect(&Token::LParen)?;
                let list = self.parse_expr_list_until_rparen()?;
                lhs = Expr::QuantifiedCompare {
                    left: Box::new(lhs),
                    op,
                    quantifier,
                    list,
                };
                continue;
            }
            let rhs = self.parse_expr_bp(r_bp)?;
            lhs = Expr::Binary {
                op,
                left: Box::new(lhs),
                right: Box::new(rhs),
            };
        }

        Ok(lhs)
    }

    fn eat_quantifier(&mut self) -> Option<Quantifier> {
        if self.eat_keyword("any") {
            Some(Quantifier::Any)
        } else if self.eat_keyword("some") {
            Some(Quantifier::Some)
        } else if self.eat_keyword("all") {
            Some(Quantifier::All)
        } else {
            None
        }
    }

    fn parse_expr_list_until_rparen(&mut self) -> Result<Vec<Expr>, String> {
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
        Ok(list)
    }

    fn parse_prefix(&mut self) -> Result<Expr, String> {
        // Unary operators.
        if self.eat_keyword("not") {
            let e = self.parse_expr_bp(3)?;
            return Ok(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(e),
            });
        }
        if self.peek() == Some(&Token::Minus) {
            self.advance();
            let e = self.parse_expr_bp(9)?;
            return Ok(Expr::Unary {
                op: UnaryOp::Neg,
                expr: Box::new(e),
            });
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
                let (target, raw) = self.parse_data_type_named()?;
                // `reg*` casts (regclass/regtype/regnamespace/regproc/regrole)
                // are object-name <-> OID conversions. Model them as function
                // calls the executor resolves against the catalog, since the
                // plain `DataType` cast would lose the distinction.
                e = match raw.as_deref() {
                    Some(
                        name @ ("regclass" | "regtype" | "regnamespace" | "regproc"
                        | "regrole" | "regprocedure" | "regoper" | "regoperator"
                        | "regconfig" | "regdictionary"),
                    ) => Expr::Function {
                        name: format!("__cast_{name}"),
                        args: vec![e],
                        star: false,
                        distinct: false,
                        filter: None,
                        over: None,
                    },
                    _ => Expr::Cast {
                        expr: Box::new(e),
                        target,
                    },
                };
            } else if self.peek() == Some(&Token::LBracket) {
                // Array element subscript `expr[idx]` (1-based). Modelled as a
                // call to the internal `__subscript` function so all expression
                // traversal/serialisation paths handle it via the Function arm.
                self.advance();
                let idx = self.parse_expr()?;
                self.expect(&Token::RBracket)?;
                e = Expr::Function {
                    name: "__subscript".to_string(),
                    args: vec![e, idx],
                    star: false,
                    distinct: false,
                    filter: None,
                    over: None,
                };
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
                // A parenthesized scalar subquery, or a grouped expression.
                if self.is_keyword("select") {
                    let sub = self.parse_select()?;
                    self.expect(&Token::RParen)?;
                    Ok(Expr::ScalarSubquery(Box::new(sub)))
                } else {
                    let first = self.parse_expr()?;
                    if self.eat(&Token::Comma) {
                        let mut items = vec![first];
                        loop {
                            items.push(self.parse_expr()?);
                            if self.eat(&Token::Comma) {
                                continue;
                            }
                            break;
                        }
                        self.expect(&Token::RParen)?;
                        return Ok(Expr::Row(items));
                    }
                    self.expect(&Token::RParen)?;
                    Ok(first)
                }
            }
            Some(Token::Word(w)) => {
                let lw = w.to_ascii_lowercase();
                match lw.as_str() {
                    "true" => Ok(Expr::Bool(true)),
                    "false" => Ok(Expr::Bool(false)),
                    "null" => Ok(Expr::Null),
                    "row" if self.peek() == Some(&Token::LParen) => {
                        self.advance();
                        Ok(Expr::Row(self.parse_call_args()?))
                    }
                    // `INTERVAL '1 year 2 months'` (optionally with a trailing
                    // field qualifier like `YEAR TO MONTH`). Parsed as a cast of
                    // the raw text to `interval`; the field qualifier is consumed
                    // and, for the `Y-M` packed form, folded into the literal so
                    // the executor's normaliser can interpret it.
                    "interval" if matches!(self.peek(), Some(Token::StringLit(_))) => {
                        let Some(Token::StringLit(s)) = self.advance() else {
                            unreachable!()
                        };
                        let qual = self.parse_interval_qualifier();
                        let literal = match qual {
                            Some(q) => format!("{s}\u{1}{q}"),
                            None => s,
                        };
                        Ok(Expr::Cast {
                            expr: Box::new(Expr::Str(literal)),
                            target: DataType::Interval,
                        })
                    }
                    // Other typed string literals: `DATE '...'`, `TIMESTAMP '...'`,
                    // `TIME '...'`. Parsed as a cast of the text to that type.
                    "date" | "timestamp" | "timestamptz" | "time"
                        if matches!(self.peek(), Some(Token::StringLit(_))) =>
                    {
                        let Some(Token::StringLit(s)) = self.advance() else {
                            unreachable!()
                        };
                        let target = match lw.as_str() {
                            "date" => DataType::Date,
                            "time" => DataType::Time,
                            "timestamptz" => DataType::TimestampTz,
                            _ => DataType::Timestamp,
                        };
                        Ok(Expr::Cast {
                            expr: Box::new(Expr::Str(s)),
                            target,
                        })
                    }
                    "array" if self.peek() == Some(&Token::LBracket) => {
                        self.advance();
                        let mut items = Vec::new();
                        if self.peek() != Some(&Token::RBracket) {
                            loop {
                                items.push(self.parse_expr()?);
                                if self.eat(&Token::Comma) {
                                    continue;
                                }
                                break;
                            }
                        }
                        self.expect(&Token::RBracket)?;
                        Ok(Expr::Array(items))
                    }
                    // `ARRAY(SELECT ...)`: array constructor over a subquery.
                    // Modeled as a scalar (uncorrelated) subquery. In this engine
                    // such constructs only appear in psql catalog probes that
                    // return no rows (e.g. RLS policy role lists), so collapsing
                    // to a scalar subquery is sufficient for compatibility.
                    "array" if self.peek() == Some(&Token::LParen) => {
                        self.advance();
                        let sub = self.parse_select()?;
                        self.expect(&Token::RParen)?;
                        Ok(Expr::ScalarSubquery(Box::new(sub)))
                    }
                    "extract" => {
                        // EXTRACT(field FROM source) → date_part('field', source)
                        self.expect(&Token::LParen)?;
                        let field = self.parse_ident()?.to_ascii_lowercase();
                        self.expect_keyword("from")?;
                        let source = self.parse_expr()?;
                        self.expect(&Token::RParen)?;
                        Ok(Expr::Function {
                            name: "date_part".to_string(),
                            args: vec![Expr::Str(field), source],
                            star: false,
                            distinct: false,
                            filter: None,
                            over: None,
                        })
                    }
                    "exists" => {
                        self.expect(&Token::LParen)?;
                        let sub = self.parse_select()?;
                        self.expect(&Token::RParen)?;
                        Ok(Expr::Exists(Box::new(sub)))
                    }
                    "case" => self.parse_case(),
                    "cast" => {
                        self.expect(&Token::LParen)?;
                        let inner = self.parse_expr()?;
                        self.expect_keyword("as")?;
                        let target = self.parse_data_type()?;
                        self.expect(&Token::RParen)?;
                        Ok(Expr::Cast {
                            expr: Box::new(inner),
                            target,
                        })
                    }
                    // Niladic SQL functions usable without parentheses.
                    "current_user" | "current_role" | "session_user" | "current_schema"
                    | "current_catalog" | "current_date" | "current_timestamp"
                        if self.peek() != Some(&Token::LParen) =>
                    {
                        Ok(Expr::Function {
                            name: lw,
                            args: Vec::new(),
                            star: false,
                            distinct: false,
                            filter: None,
                            over: None,
                        })
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
                                    Ok(Expr::QualifiedColumn {
                                        qualifier: col,
                                        name: col2,
                                    })
                                }
                            } else if self.peek() == Some(&Token::LParen) {
                                // Two-part function call `schema.func(...)`.
                                self.advance();
                                self.parse_function_args(col)
                            } else {
                                Ok(Expr::QualifiedColumn {
                                    qualifier: w,
                                    name: col,
                                })
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

    /// Parse the parenthesised column list of a `ROLLUP (...)` / `CUBE (...)`.
    fn parse_grouping_paren_list(&mut self) -> Result<Vec<Expr>, String> {
        self.expect(&Token::LParen)?;
        self.parse_expr_list_until_rparen()
    }

    fn parse_function_args(&mut self, name: String) -> Result<Expr, String> {
        // `count(*)` special case.
        if self.peek() == Some(&Token::Star) {
            self.advance();
            self.expect(&Token::RParen)?;
            let filter = self.parse_aggregate_filter()?;
            let over = self.parse_over_clause()?;
            return Ok(Expr::Function {
                name,
                args: Vec::new(),
                star: true,
                distinct: false,
                filter,
                over,
            });
        }
        let mut distinct = false;
        let mut args = if self.peek() != Some(&Token::RParen) {
            // `DISTINCT` inside an aggregate, e.g. `count(DISTINCT x)`.
            distinct = self.eat_keyword("distinct");
            self.parse_call_args()?
        } else {
            self.expect(&Token::RParen)?;
            Vec::new()
        };
        // Ordered-set aggregates: `f(direct_args) WITHIN GROUP (ORDER BY expr)`.
        // We desugar by appending the ORDER BY expression to `args` (the direct
        // arguments come first), which the executor recognises by aggregate name.
        if self.is_keyword("within") {
            self.expect_keyword("within")?;
            self.expect_keyword("group")?;
            self.expect(&Token::LParen)?;
            self.expect_keyword("order")?;
            self.expect_keyword("by")?;
            args.push(self.parse_expr()?);
            // Direction keywords are accepted but the ordered-set aggregates we
            // support (percentile_*/mode) are direction-insensitive in result.
            self.eat_keyword("asc");
            self.eat_keyword("desc");
            self.expect(&Token::RParen)?;
        }
        let filter = self.parse_aggregate_filter()?;
        let over = self.parse_over_clause()?;
        Ok(Expr::Function {
            name,
            args,
            star: false,
            distinct,
            filter,
            over,
        })
    }

    /// Parse an optional `OVER ( [PARTITION BY ...] [ORDER BY ...] [frame] )`.
    /// Frame clauses are accepted and ignored (the executor uses the default).
    fn parse_over_clause(&mut self) -> Result<Option<Box<WindowSpec>>, String> {
        if !self.eat_keyword("over") {
            return Ok(None);
        }
        self.expect(&Token::LParen)?;
        let mut partition_by = Vec::new();
        if self.eat_keyword("partition") {
            self.expect_keyword("by")?;
            loop {
                partition_by.push(self.parse_expr()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
        }
        let mut order_by = Vec::new();
        if self.eat_keyword("order") {
            self.expect_keyword("by")?;
            order_by = self.parse_order_by_items()?;
        }
        // Swallow an optional frame clause (ROWS/RANGE/GROUPS ...) up to the
        // closing paren; the executor applies the SQL default frame.
        while self.peek() != Some(&Token::RParen) {
            if self.advance().is_none() {
                return Err("unterminated OVER clause".into());
            }
        }
        self.expect(&Token::RParen)?;
        Ok(Some(Box::new(WindowSpec {
            partition_by,
            order_by,
        })))
    }

    fn parse_aggregate_filter(&mut self) -> Result<Option<Box<Expr>>, String> {
        if !self.eat_keyword("filter") {
            return Ok(None);
        }
        self.expect(&Token::LParen)?;
        self.expect_keyword("where")?;
        let expr = self.parse_expr()?;
        self.expect(&Token::RParen)?;
        Ok(Some(Box::new(expr)))
    }

    /// Consume the remaining tokens of the current statement (up to but not
    /// including a `;`). Used to accept-and-ignore unmodeled clause tails.
    fn skip_to_statement_end(&mut self) {
        while self.peek().is_some() && !matches!(self.peek(), Some(Token::Semicolon)) {
            self.advance();
        }
    }

    fn parse_call_args(&mut self) -> Result<Vec<Expr>, String> {
        let mut args = Vec::new();
        if self.peek() != Some(&Token::RParen) {
            loop {
                args.push(self.parse_expr()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
        }
        // Aggregate-call `ORDER BY` (e.g. `array_agg(x ORDER BY y)`): we accept
        // and discard the ordering. The aggregates that carry it in the catalog
        // queries pg_dump issues are order-insensitive for our purposes; we do
        // not currently produce ordered aggregate output.
        if self.is_keyword("order") {
            self.expect_keyword("order")?;
            self.expect_keyword("by")?;
            let _ = self.parse_order_by_items()?;
        }
        self.expect(&Token::RParen)?;
        Ok(args)
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
            // `IN (SELECT ...)` is a subquery; otherwise a value list.
            if self.is_keyword("select") {
                let sub = self.parse_select()?;
                self.expect(&Token::RParen)?;
                return Ok(Expr::InSubquery {
                    expr: Box::new(lhs),
                    subquery: Box::new(sub),
                    negated,
                });
            }
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
            Ok(Expr::InList {
                expr: Box::new(lhs),
                list,
                negated,
            })
        } else {
            Err(format!(
                "expected a predicate after operand, found {:?}",
                self.peek()
            ))
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
        Ok(Expr::Case {
            operand,
            whens,
            else_expr,
        })
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
            Some(Token::Arrow) => BinaryOp::JsonGet,
            Some(Token::ArrowText) => BinaryOp::JsonGetText,
            Some(Token::ArrayContains) => BinaryOp::ArrayContains,
            Some(Token::TextSearchMatch) => BinaryOp::TextSearchMatch,
            Some(Token::ArrayContainedBy) => BinaryOp::ArrayContainedBy,
            Some(Token::ArrayOverlap) => BinaryOp::ArrayOverlap,
            Some(Token::NetworkContainedBy) => BinaryOp::NetworkContainedBy,
            Some(Token::NetworkContainedByEq) => BinaryOp::NetworkContainedByEq,
            Some(Token::NetworkContains) => BinaryOp::NetworkContains,
            Some(Token::NetworkContainsEq) => BinaryOp::NetworkContainsEq,
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
            Token::Arrow => Some(BinaryOp::JsonGet),
            Token::ArrowText => Some(BinaryOp::JsonGetText),
            Token::ArrayContains => Some(BinaryOp::ArrayContains),
            Token::TextSearchMatch => Some(BinaryOp::TextSearchMatch),
            Token::ArrayContainedBy => Some(BinaryOp::ArrayContainedBy),
            Token::ArrayOverlap => Some(BinaryOp::ArrayOverlap),
            Token::NetworkContainedBy => Some(BinaryOp::NetworkContainedBy),
            Token::NetworkContainedByEq => Some(BinaryOp::NetworkContainedByEq),
            Token::NetworkContains => Some(BinaryOp::NetworkContains),
            Token::NetworkContainsEq => Some(BinaryOp::NetworkContainsEq),
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

/// Map a privilege keyword to a [`Privilege`], erroring on unknown words.
/// Expand `ROLLUP(a, b, c)` into the grouping sets
/// `[(a,b,c), (a,b), (a), ()]` (every leading prefix, then the grand total).
fn rollup_sets(cols: &[Expr]) -> Vec<Vec<Expr>> {
    let mut sets = Vec::with_capacity(cols.len() + 1);
    for len in (0..=cols.len()).rev() {
        sets.push(cols[..len].to_vec());
    }
    sets
}

/// Expand `CUBE(a, b)` into all `2^n` subsets, ordered most-to-least specific
/// to match PostgreSQL (full set first, grand total last).
fn cube_sets(cols: &[Expr]) -> Vec<Vec<Expr>> {
    let n = cols.len();
    let mut sets = Vec::with_capacity(1 << n);
    // Iterate masks so that the all-ones mask (full set) comes first and the
    // zero mask (grand total) comes last.
    for mask in (0..(1u32 << n)).rev() {
        let mut set = Vec::new();
        for (i, col) in cols.iter().enumerate() {
            if mask & (1 << i) != 0 {
                set.push(col.clone());
            }
        }
        sets.push(set);
    }
    sets
}

fn privilege_from_keyword(w: &str) -> Result<Privilege, String> {
    match w.to_ascii_lowercase().as_str() {
        "select" => Ok(Privilege::Select),
        "insert" => Ok(Privilege::Insert),
        "update" => Ok(Privilege::Update),
        "delete" => Ok(Privilege::Delete),
        "truncate" => Ok(Privilege::Truncate),
        "references" => Ok(Privilege::References),
        "trigger" => Ok(Privilege::Trigger),
        other => Err(format!("unsupported privilege: `{other}`")),
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
        | BinaryOp::RegexNotMatch { .. }
        | BinaryOp::ArrayContains
        | BinaryOp::TextSearchMatch
        | BinaryOp::ArrayContainedBy
        | BinaryOp::ArrayOverlap
        | BinaryOp::NetworkContainedBy
        | BinaryOp::NetworkContainedByEq
        | BinaryOp::NetworkContains
        | BinaryOp::NetworkContainsEq => (5, 6),
        BinaryOp::Concat | BinaryOp::JsonGet | BinaryOp::JsonGetText => (8, 9),
        BinaryOp::Add | BinaryOp::Sub => (10, 11),
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => (12, 13),
    }
}

/// Keywords that begin a clause after the SELECT projection, so a bare word
/// here is not an alias.
fn is_select_clause_keyword(w: &str) -> bool {
    const KW: &[&str] = &[
        "from", "where", "order", "limit", "offset", "group", "having", "as", "and", "or",
        "union", "intersect", "except", "for", "fetch",
    ];
    KW.iter().any(|k| w.eq_ignore_ascii_case(k))
}

/// Keywords that continue a MERGE statement, so a bare word here is a clause
/// keyword rather than a target/source alias.
fn is_merge_keyword(w: &str) -> bool {
    const KW: &[&str] = &["using", "on", "when"];
    KW.iter().any(|k| w.eq_ignore_ascii_case(k))
}

/// Keywords that may follow a table reference, so a bare word here is a clause
/// rather than a table alias.
fn is_table_ref_keyword(w: &str) -> bool {
    const KW: &[&str] = &[
        "where",
        "order",
        "limit",
        "offset",
        "group",
        "having",
        "join",
        "inner",
        "left",
        "right",
        "full",
        "cross",
        "outer",
        "on",
        "union",
        "intersect",
        "except",
        "for",
        "as",
    ];
    KW.iter().any(|k| w.eq_ignore_ascii_case(k))
}

/// Render a single token back to its source-text form. Used to capture the
/// verbatim tail of a `CREATE RULE` definition for round-tripping.
fn token_text(tok: &Token) -> String {
    match tok {
        Token::Word(s) | Token::QuotedIdent(s) | Token::Number(s) => s.clone(),
        Token::StringLit(s) => format!("'{}'", s.replace('\'', "''")),
        Token::Param(n) => format!("${n}"),
        Token::Comma => ",".into(),
        Token::Dot => ".".into(),
        Token::Semicolon => ";".into(),
        Token::LParen => "(".into(),
        Token::RParen => ")".into(),
        Token::LBracket => "[".into(),
        Token::RBracket => "]".into(),
        Token::Star => "*".into(),
        Token::Plus => "+".into(),
        Token::Minus => "-".into(),
        Token::Slash => "/".into(),
        Token::Percent => "%".into(),
        Token::Eq => "=".into(),
        Token::NotEq => "<>".into(),
        Token::Lt => "<".into(),
        Token::LtEq => "<=".into(),
        Token::Gt => ">".into(),
        Token::GtEq => ">=".into(),
        Token::Concat => "||".into(),
        Token::DoubleColon => "::".into(),
        Token::Arrow => "->".into(),
        Token::ArrowText => "->>".into(),
        Token::ArrayContains => "@>".into(),
        Token::TextSearchMatch => "@@".into(),
        Token::ArrayContainedBy => "<@".into(),
        Token::ArrayOverlap => "&&".into(),
        Token::NetworkContainedBy => "<<".into(),
        Token::NetworkContainedByEq => "<<=".into(),
        Token::NetworkContains => ">>".into(),
        Token::NetworkContainsEq => ">>=".into(),
        Token::Match => "~".into(),
        Token::MatchCi => "~*".into(),
        Token::NotMatch => "!~".into(),
        Token::NotMatchCi => "!~*".into(),
    }
}
