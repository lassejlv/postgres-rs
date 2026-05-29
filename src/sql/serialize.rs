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
        Statement::CreateExtension(c) => {
            let exists = if c.if_not_exists {
                "IF NOT EXISTS "
            } else {
                ""
            };
            let version = c
                .version
                .as_ref()
                .map(|v| format!(" WITH VERSION {}", quote_string(v)))
                .unwrap_or_default();
            format!("CREATE EXTENSION {exists}{}{version}", ident(&c.name))
        }
        Statement::CreateRole(c) => {
            let kind = if c.login { "USER" } else { "ROLE" };
            let options = role_options_sql(&c.options);
            if options.is_empty() {
                format!("CREATE {kind} {}", ident(&c.name))
            } else {
                format!(
                    "CREATE {kind} {} WITH {}",
                    ident(&c.name),
                    options.join(" ")
                )
            }
        }
        Statement::CreateSequence(c) => {
            let exists = if c.if_not_exists {
                "IF NOT EXISTS "
            } else {
                ""
            };
            format!(
                "CREATE SEQUENCE {exists}{} START WITH {} INCREMENT BY {}",
                ident(&c.name),
                c.start,
                c.increment
            )
        }
        Statement::CreateSchema(c) => {
            let exists = if c.if_not_exists {
                "IF NOT EXISTS "
            } else {
                ""
            };
            format!("CREATE SCHEMA {exists}{}", ident(&c.name))
        }
        Statement::CreateDatabase(c) => format!("CREATE DATABASE {}", ident(&c.name)),
        Statement::CreateTablespace(c) => {
            format!(
                "CREATE TABLESPACE {} LOCATION '{}'",
                ident(&c.name),
                c.location.replace('\'', "''")
            )
        }
        Statement::CreateCollation(c) => {
            let exists = if c.if_not_exists {
                "IF NOT EXISTS "
            } else {
                ""
            };
            format!(
                "CREATE COLLATION {exists}{} (LOCALE = '{}')",
                ident(&c.name),
                c.locale.replace('\'', "''")
            )
        }
        Statement::CreateType(c) => create_type_sql(c),
        Statement::CreateDomain(c) => create_domain_sql(c),
        Statement::CreateView(c) => {
            let replace = if c.or_replace { "OR REPLACE " } else { "" };
            format!(
                "CREATE {replace}VIEW {} AS {}",
                ident(&c.name),
                select_to_sql(&c.select)
            )
        }
        Statement::CreateMaterializedView(c) => {
            let exists = if c.if_not_exists {
                "IF NOT EXISTS "
            } else {
                ""
            };
            format!(
                "CREATE MATERIALIZED VIEW {exists}{} AS {}",
                ident(&c.name),
                select_to_sql(&c.select)
            )
        }
        Statement::CreateFunction(c) => create_function_sql(c),
        Statement::CreateTrigger(c) => create_trigger_sql(c),
        Statement::CreateRule(c) => create_rule_sql(c),
        Statement::CreateAggregate(c) => create_aggregate_sql(c),
        Statement::DropFunction(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            let sig = match &d.arg_types {
                Some(types) => format!("({})", types.join(", ")),
                None => String::new(),
            };
            format!("DROP FUNCTION {exists}{}{sig}", ident(&d.name))
        }
        Statement::DropTrigger(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!(
                "DROP TRIGGER {exists}{} ON {}",
                ident(&d.name),
                ident(&d.table)
            )
        }
        Statement::DropRule(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!(
                "DROP RULE {exists}{} ON {}",
                ident(&d.name),
                ident(&d.table)
            )
        }
        Statement::DropAggregate(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!(
                "DROP AGGREGATE {exists}{}({})",
                ident(&d.name),
                d.arg_types.join(", ")
            )
        }
        Statement::DropTable(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!("DROP TABLE {exists}{}", ident(&d.name))
        }
        Statement::DropExtension(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!("DROP EXTENSION {exists}{}", ident(&d.name))
        }
        Statement::DropRole(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!("DROP ROLE {exists}{}", ident(&d.name))
        }
        Statement::DropSequence(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!("DROP SEQUENCE {exists}{}", ident(&d.name))
        }
        Statement::DropSchema(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!("DROP SCHEMA {exists}{}", ident(&d.name))
        }
        Statement::DropDatabase(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!("DROP DATABASE {exists}{}", ident(&d.name))
        }
        Statement::DropTablespace(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!("DROP TABLESPACE {exists}{}", ident(&d.name))
        }
        Statement::DropCollation(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!("DROP COLLATION {exists}{}", ident(&d.name))
        }
        Statement::DropType(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!("DROP TYPE {exists}{}", ident(&d.name))
        }
        Statement::DropDomain(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!("DROP DOMAIN {exists}{}", ident(&d.name))
        }
        Statement::DropView(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!("DROP VIEW {exists}{}", ident(&d.name))
        }
        Statement::DropMaterializedView(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!("DROP MATERIALIZED VIEW {exists}{}", ident(&d.name))
        }
        Statement::AlterTable(a) => alter_table_sql(a),
        Statement::AlterRole(a) => {
            let options = role_options_sql(&a.options);
            if options.is_empty() {
                format!("ALTER ROLE {}", ident(&a.name))
            } else {
                format!("ALTER ROLE {} WITH {}", ident(&a.name), options.join(" "))
            }
        }
        Statement::AlterSequence(a) => {
            let mut parts = Vec::new();
            if let Some(restart) = a.restart {
                parts.push(format!("RESTART WITH {restart}"));
            }
            if let Some(increment) = a.increment {
                parts.push(format!("INCREMENT BY {increment}"));
            }
            format!("ALTER SEQUENCE {} {}", ident(&a.name), parts.join(" "))
        }
        Statement::CreateIndex(c) => create_index_sql(c),
        Statement::DropIndex(d) => {
            let exists = if d.if_exists { "IF EXISTS " } else { "" };
            format!("DROP INDEX {exists}{}", ident(&d.name))
        }
        Statement::Insert(i) => insert_sql(i),
        Statement::Copy(c) => copy_sql(c),
        Statement::Truncate(t) => truncate_sql(t),
        Statement::DeclareCursor(d) => {
            format!(
                "DECLARE {} CURSOR FOR {}",
                ident(&d.name),
                select_to_sql(&d.select)
            )
        }
        Statement::Fetch(f) => {
            let count = match f.count {
                FetchCount::Next => "NEXT".to_string(),
                FetchCount::All => "ALL".to_string(),
                FetchCount::Count(n) => n.to_string(),
            };
            format!("FETCH {count} FROM {}", ident(&f.cursor))
        }
        Statement::Update(u) => update_sql(u),
        Statement::AlterDatabase(a) => alter_database_sql(a),
        Statement::Delete(d) => delete_sql(d),
        Statement::Merge(m) => merge_sql(m),
        Statement::Explain(e) => {
            let analyze = if e.analyze { "ANALYZE " } else { "" };
            format!("EXPLAIN {analyze}{}", statement_to_sql(&e.statement))
        }
        Statement::Analyze(a) => match &a.table {
            Some(table) => format!("ANALYZE {}", ident(table)),
            None => "ANALYZE".into(),
        },
        Statement::Comment(c) => comment_sql(c),
        Statement::SecurityLabel(s) => security_label_sql(s),
        Statement::Grant(g) => grant_sql(&g.object, &g.grantees, false),
        Statement::Revoke(r) => grant_sql(&r.object, &r.grantees, true),
        Statement::AlterSystem(a) => alter_system_sql(a),
        Statement::Vacuum(v) => match &v.table {
            Some(table) => format!("VACUUM {}", ident(table)),
            None => "VACUUM".into(),
        },
        Statement::Reindex(r) => reindex_sql(r),
        Statement::Cluster(c) => match (&c.table, &c.index) {
            (Some(table), Some(index)) => {
                format!("CLUSTER {} USING {}", ident(table), ident(index))
            }
            (Some(table), None) => format!("CLUSTER {}", ident(table)),
            (None, _) => "CLUSTER".into(),
        },
        Statement::Checkpoint => "CHECKPOINT".into(),
        Statement::Discard(d) => discard_sql(d).into(),
        Statement::Listen { channel } => format!("LISTEN {}", ident(channel)),
        Statement::Notify { channel, payload } => match payload {
            Some(payload) => format!("NOTIFY {}, {}", ident(channel), quote_string(payload)),
            None => format!("NOTIFY {}", ident(channel)),
        },
        Statement::Unlisten { channel } => match channel {
            Some(channel) => format!("UNLISTEN {}", ident(channel)),
            None => "UNLISTEN *".into(),
        },
        Statement::LockTable(l) => {
            let tables: Vec<String> = l.tables.iter().map(|table| ident(table)).collect();
            format!("LOCK TABLE {}", tables.join(", "))
        }
        Statement::RefreshMaterializedView(r) => {
            format!("REFRESH MATERIALIZED VIEW {}", ident(&r.name))
        }
        Statement::Select(s) => select_to_sql(s),
        Statement::Begin => "BEGIN".into(),
        Statement::Commit => "COMMIT".into(),
        Statement::Rollback => "ROLLBACK".into(),
        Statement::Savepoint { name } => format!("SAVEPOINT {}", ident(name)),
        Statement::ReleaseSavepoint { name } => format!("RELEASE SAVEPOINT {}", ident(name)),
        Statement::RollbackToSavepoint { name } => format!("ROLLBACK TO SAVEPOINT {}", ident(name)),
        Statement::Set { name, .. } => format!("SET {name}"),
        Statement::Show { name } => format!("SHOW {name}"),
        Statement::Empty => String::new(),
    }
}

fn truncate_sql(t: &Truncate) -> String {
    let tables: Vec<String> = t.tables.iter().map(|t| ident(t)).collect();
    format!("TRUNCATE TABLE {}", tables.join(", "))
}

fn copy_sql(c: &Copy) -> String {
    let cols = match &c.columns {
        Some(names) => {
            let list: Vec<String> = names.iter().map(|n| ident(n)).collect();
            format!(" ({})", list.join(", "))
        }
        None => String::new(),
    };
    let (dir, endpoint) = match c.direction {
        CopyDirection::From => ("FROM", "STDIN"),
        CopyDirection::To => ("TO", "STDOUT"),
    };
    let mut opts = Vec::new();
    if c.format == CopyFormat::Csv {
        opts.push("FORMAT csv".to_string());
    }
    if let Some(d) = c.delimiter {
        opts.push(format!("DELIMITER '{d}'"));
    }
    if c.header {
        opts.push("HEADER".to_string());
    }
    if let Some(n) = &c.null {
        opts.push(format!("NULL '{n}'"));
    }
    let with = if opts.is_empty() {
        String::new()
    } else {
        format!(" WITH ({})", opts.join(", "))
    };
    format!("COPY {}{} {} {}{}", ident(&c.table), cols, dir, endpoint, with)
}

fn alter_database_sql(a: &AlterDatabase) -> String {
    match &a.action {
        AlterDatabaseAction::Rename { to } => {
            format!("ALTER DATABASE {} RENAME TO {}", ident(&a.name), ident(to))
        }
        AlterDatabaseAction::SetConnectionLimit { limit } => {
            format!(
                "ALTER DATABASE {} WITH CONNECTION LIMIT {limit}",
                ident(&a.name)
            )
        }
    }
}

fn role_options_sql(options: &RoleOptions) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(value) = options.superuser {
        out.push(if value { "SUPERUSER" } else { "NOSUPERUSER" }.into());
    }
    if let Some(value) = options.inherit {
        out.push(if value { "INHERIT" } else { "NOINHERIT" }.into());
    }
    if let Some(value) = options.create_role {
        out.push(if value { "CREATEROLE" } else { "NOCREATEROLE" }.into());
    }
    if let Some(value) = options.create_db {
        out.push(if value { "CREATEDB" } else { "NOCREATEDB" }.into());
    }
    if let Some(value) = options.login {
        out.push(if value { "LOGIN" } else { "NOLOGIN" }.into());
    }
    if let Some(value) = options.replication {
        out.push(
            if value {
                "REPLICATION"
            } else {
                "NOREPLICATION"
            }
            .into(),
        );
    }
    if let Some(value) = options.bypass_rls {
        out.push(if value { "BYPASSRLS" } else { "NOBYPASSRLS" }.into());
    }
    if let Some(value) = options.connection_limit {
        out.push(format!("CONNECTION LIMIT {value}"));
    }
    if let Some(value) = &options.password {
        out.push(match value {
            Some(password) => format!("PASSWORD {}", quote_string(password)),
            None => "PASSWORD NULL".into(),
        });
    }
    if let Some(value) = &options.valid_until {
        out.push(match value {
            Some(valid_until) => format!("VALID UNTIL {}", quote_string(valid_until)),
            None => "VALID UNTIL 'infinity'".into(),
        });
    }
    if !options.in_role.is_empty() {
        let roles: Vec<String> = options.in_role.iter().map(|r| ident(r)).collect();
        out.push(format!("IN ROLE {}", roles.join(", ")));
    }
    if !options.role_members.is_empty() {
        let roles: Vec<String> = options.role_members.iter().map(|r| ident(r)).collect();
        out.push(format!("ROLE {}", roles.join(", ")));
    }
    if !options.admin_members.is_empty() {
        let roles: Vec<String> = options.admin_members.iter().map(|r| ident(r)).collect();
        out.push(format!("ADMIN {}", roles.join(", ")));
    }
    out
}

fn grant_sql(object: &GrantObject, grantees: &[Grantee], revoke: bool) -> String {
    let grantee_list = grantees
        .iter()
        .map(|g| match g {
            Grantee::Role(name) => ident(name),
            Grantee::Public => "PUBLIC".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    match object {
        GrantObject::Table { privileges, table } => {
            let privs = match privileges {
                Privileges::All => "ALL".to_string(),
                Privileges::List(list) => list
                    .iter()
                    .map(|p| p.as_str().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            };
            if revoke {
                format!(
                    "REVOKE {privs} ON {} FROM {grantee_list}",
                    ident(table)
                )
            } else {
                format!("GRANT {privs} ON {} TO {grantee_list}", ident(table))
            }
        }
        GrantObject::Roles { roles } => {
            let role_list = roles
                .iter()
                .map(|r| ident(r))
                .collect::<Vec<_>>()
                .join(", ");
            if revoke {
                format!("REVOKE {role_list} FROM {grantee_list}")
            } else {
                format!("GRANT {role_list} TO {grantee_list}")
            }
        }
    }
}

fn comment_sql(c: &Comment) -> String {
    let target = match &c.object {
        CommentObject::Relation { name } => format!("TABLE {}", ident(name)),
        CommentObject::Column { table, column } => {
            format!("COLUMN {}.{}", ident(table), ident(column))
        }
    };
    let comment = c
        .comment
        .as_ref()
        .map(|s| quote_string(s))
        .unwrap_or_else(|| "NULL".into());
    format!("COMMENT ON {target} IS {comment}")
}

fn security_label_sql(s: &SecurityLabel) -> String {
    let target = match &s.object {
        CommentObject::Relation { name } => format!("TABLE {}", ident(name)),
        CommentObject::Column { table, column } => {
            format!("COLUMN {}.{}", ident(table), ident(column))
        }
    };
    let label = s
        .label
        .as_ref()
        .map(|label| quote_string(label))
        .unwrap_or_else(|| "NULL".into());
    format!(
        "SECURITY LABEL FOR {} ON {target} IS {label}",
        ident(&s.provider)
    )
}

fn alter_system_sql(a: &AlterSystem) -> String {
    match &a.action {
        AlterSystemAction::Set { name, value } => {
            format!("ALTER SYSTEM SET {} = {}", ident(name), quote_string(value))
        }
        AlterSystemAction::Reset { name: Some(name) } => {
            format!("ALTER SYSTEM RESET {}", ident(name))
        }
        AlterSystemAction::Reset { name: None } => "ALTER SYSTEM RESET ALL".into(),
    }
}

fn reindex_sql(r: &Reindex) -> String {
    match &r.target {
        ReindexTarget::Table(table) => format!("REINDEX TABLE {}", ident(table)),
        ReindexTarget::Index(index) => format!("REINDEX INDEX {}", ident(index)),
        ReindexTarget::Database(database) => format!("REINDEX DATABASE {}", ident(database)),
        ReindexTarget::System(Some(database)) => format!("REINDEX SYSTEM {}", ident(database)),
        ReindexTarget::System(None) => "REINDEX SYSTEM".into(),
    }
}

fn discard_sql(d: &Discard) -> &'static str {
    match d.target {
        DiscardTarget::All => "DISCARD ALL",
        DiscardTarget::Plans => "DISCARD PLANS",
        DiscardTarget::Sequences => "DISCARD SEQUENCES",
        DiscardTarget::Temp => "DISCARD TEMP",
    }
}

fn create_type_sql(c: &CreateType) -> String {
    match &c.kind {
        CreateTypeKind::Enum { labels } => {
            let labels: Vec<String> = labels.iter().map(|l| quote_string(l)).collect();
            format!("CREATE TYPE {} AS ENUM ({})", ident(&c.name), labels.join(", "))
        }
        CreateTypeKind::Composite { attributes } => {
            let attrs: Vec<String> = attributes
                .iter()
                .map(|(name, ty)| format!("{} {}", ident(name), ty.sql_name()))
                .collect();
            format!("CREATE TYPE {} AS ({})", ident(&c.name), attrs.join(", "))
        }
        CreateTypeKind::Range { subtype } => {
            format!(
                "CREATE TYPE {} AS RANGE (subtype = {})",
                ident(&c.name),
                subtype.sql_name()
            )
        }
    }
}

fn create_domain_sql(c: &CreateDomain) -> String {
    let mut s = format!("CREATE DOMAIN {} AS {}", ident(&c.name), c.base.sql_name());
    if c.not_null {
        s.push_str(" NOT NULL");
    }
    if let Some(check) = &c.check {
        s.push_str(&format!(" CHECK ({})", expr_to_sql(check)));
    }
    s
}

/// Wrap a function body in a dollar-quote delimiter that does not collide with
/// the body's contents, so it round-trips through the lexer unchanged.
fn dollar_quote(body: &str) -> String {
    if !body.contains("$$") {
        return format!("$${body}$$");
    }
    for n in 0.. {
        let tag = format!("$body{n}$");
        if !body.contains(&tag) {
            return format!("{tag}{body}{tag}");
        }
    }
    unreachable!("a free dollar-quote tag always exists")
}

fn create_function_sql(c: &CreateFunction) -> String {
    let replace = if c.or_replace { "OR REPLACE " } else { "" };
    let args: Vec<String> = c
        .args
        .iter()
        .map(|a| match &a.name {
            Some(name) => format!("{} {}", ident(name), a.type_name),
            None => a.type_name.clone(),
        })
        .collect();
    let mut s = format!(
        "CREATE {replace}FUNCTION {}({})",
        ident(&c.name),
        args.join(", ")
    );
    if let Some(rt) = &c.return_type_name {
        s.push_str(&format!(" RETURNS {rt}"));
    }
    s.push_str(&format!(
        " AS {} LANGUAGE {}",
        dollar_quote(&c.body),
        c.language
    ));
    s
}

fn create_trigger_sql(c: &CreateTrigger) -> String {
    let timing = match c.timing {
        TriggerTiming::Before => "BEFORE",
        TriggerTiming::After => "AFTER",
    };
    let events: Vec<&str> = c
        .events
        .iter()
        .map(|e| match e {
            TriggerEvent::Insert => "INSERT",
            TriggerEvent::Update => "UPDATE",
            TriggerEvent::Delete => "DELETE",
        })
        .collect();
    let level = if c.for_each_row {
        "FOR EACH ROW"
    } else {
        "FOR EACH STATEMENT"
    };
    format!(
        "CREATE TRIGGER {} {timing} {} ON {} {level} EXECUTE FUNCTION {}()",
        ident(&c.name),
        events.join(" OR "),
        ident(&c.table),
        ident(&c.function)
    )
}

fn create_rule_sql(c: &CreateRule) -> String {
    let replace = if c.or_replace { "OR REPLACE " } else { "" };
    let event = match c.event {
        TriggerEvent::Insert => "INSERT",
        TriggerEvent::Update => "UPDATE",
        TriggerEvent::Delete => "DELETE",
    };
    let def = if c.definition.is_empty() {
        String::new()
    } else {
        format!(" {}", c.definition)
    };
    format!(
        "CREATE {replace}RULE {} AS ON {event} TO {}{def}",
        ident(&c.name),
        ident(&c.table)
    )
}

fn create_aggregate_sql(c: &CreateAggregate) -> String {
    let replace = if c.or_replace { "OR REPLACE " } else { "" };
    let opts: Vec<String> = c
        .options
        .iter()
        .map(|(k, v)| format!("{} = {}", k.to_uppercase(), v))
        .collect();
    format!(
        "CREATE {replace}AGGREGATE {}({}) ({})",
        ident(&c.name),
        c.arg_types.join(", "),
        opts.join(", ")
    )
}

fn create_table_sql(c: &CreateTable) -> String {
    let exists = if c.if_not_exists {
        "IF NOT EXISTS "
    } else {
        ""
    };
    let persistence = match c.persistence {
        TablePersistence::Permanent => "",
        TablePersistence::Unlogged => "UNLOGGED ",
        TablePersistence::Temporary => "TEMPORARY ",
    };
    let mut cols: Vec<String> = c.columns.iter().map(column_def_to_sql).collect();
    cols.extend(c.constraints.iter().map(table_constraint_sql));
    format!(
        "CREATE {persistence}TABLE {exists}{} ({})",
        ident(&c.name),
        cols.join(", ")
    )
}

/// Serialize a single column definition (shared by CREATE TABLE / ALTER ADD).
fn column_def_to_sql(col: &ColumnDef) -> String {
    // `serial` columns re-emit as serial so replay rebuilds the sequence; their
    // NOT NULL and default are implicit.
    let type_name = if col.serial {
        match col.data_type {
            DataType::Int2 => "smallserial",
            DataType::Int8 => "bigserial",
            _ => "serial",
        }
    } else if let Some(name) = &col.type_name {
        // A user-defined type/domain: re-emit the declared name so WAL replay
        // resolves it against the type catalogs again.
        name.as_str()
    } else {
        col.data_type.sql_name()
    };
    let mut s = format!("{} {}", ident(&col.name), type_name);
    if col.primary_key {
        s.push_str(" PRIMARY KEY");
    } else if col.not_null && !col.serial {
        s.push_str(" NOT NULL");
    }
    if col.identity {
        if col.identity_always {
            s.push_str(" GENERATED ALWAYS AS IDENTITY");
        } else {
            s.push_str(" GENERATED BY DEFAULT AS IDENTITY");
        }
    }
    if let Some(expr) = &col.generated {
        s.push_str(&format!(
            " GENERATED ALWAYS AS ({}) STORED",
            expr_to_sql(expr)
        ));
    }
    if !col.serial {
        if let Some(default) = &col.default {
            s.push_str(&format!(" DEFAULT {}", expr_to_sql(default)));
        }
    }
    s
}

fn table_constraint_sql(constraint: &TableConstraint) -> String {
    match constraint {
        TableConstraint::Unique {
            name,
            columns,
            primary_key,
        } => {
            let kind = if *primary_key {
                "PRIMARY KEY"
            } else {
                "UNIQUE"
            };
            let columns = columns
                .iter()
                .map(|column| ident(column))
                .collect::<Vec<_>>();
            format!("CONSTRAINT {} {kind} ({})", ident(name), columns.join(", "))
        }
        TableConstraint::Check {
            name,
            expr,
            validated,
        } => {
            let valid = if *validated { "" } else { " NOT VALID" };
            format!(
                "CONSTRAINT {} CHECK ({}){valid}",
                ident(name),
                expr_to_sql(expr)
            )
        }
        TableConstraint::ForeignKey {
            name,
            column,
            ref_table,
            ref_column,
            validated,
        } => {
            let valid = if *validated { "" } else { " NOT VALID" };
            format!(
                "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {}({}){valid}",
                ident(name),
                ident(column),
                ident(ref_table),
                ident(ref_column)
            )
        }
    }
}

fn alter_table_sql(a: &AlterTable) -> String {
    let t = ident(&a.table);
    match &a.action {
        AlterAction::AddColumn {
            column,
            if_not_exists,
        } => {
            let exists = if *if_not_exists { "IF NOT EXISTS " } else { "" };
            format!(
                "ALTER TABLE {t} ADD COLUMN {exists}{}",
                column_def_to_sql(column)
            )
        }
        AlterAction::DropColumn { name, if_exists } => {
            let exists = if *if_exists { "IF EXISTS " } else { "" };
            format!("ALTER TABLE {t} DROP COLUMN {exists}{}", ident(name))
        }
        AlterAction::AddConstraint { constraint } => match constraint {
            TableConstraint::Unique {
                name,
                columns,
                primary_key,
            } => {
                let kind = if *primary_key {
                    "PRIMARY KEY"
                } else {
                    "UNIQUE"
                };
                format!(
                    "ALTER TABLE {t} ADD CONSTRAINT {} {kind} ({})",
                    ident(name),
                    columns
                        .iter()
                        .map(|column| ident(column))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            TableConstraint::Check {
                name,
                expr,
                validated,
            } => {
                let valid = if *validated { "" } else { " NOT VALID" };
                format!(
                    "ALTER TABLE {t} ADD CONSTRAINT {} CHECK ({}){valid}",
                    ident(name),
                    expr_to_sql(expr)
                )
            }
            TableConstraint::ForeignKey {
                name,
                column,
                ref_table,
                ref_column,
                validated,
            } => {
                let valid = if *validated { "" } else { " NOT VALID" };
                format!(
                    "ALTER TABLE {t} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {}({}){valid}",
                    ident(name),
                    ident(column),
                    ident(ref_table),
                    ident(ref_column)
                )
            }
        },
        AlterAction::DropConstraint { name, if_exists } => {
            let exists = if *if_exists { "IF EXISTS " } else { "" };
            format!("ALTER TABLE {t} DROP CONSTRAINT {exists}{}", ident(name))
        }
        AlterAction::RenameColumn { from, to } => {
            format!(
                "ALTER TABLE {t} RENAME COLUMN {} TO {}",
                ident(from),
                ident(to)
            )
        }
        AlterAction::RenameTable { to } => format!("ALTER TABLE {t} RENAME TO {}", ident(to)),
    }
}

fn create_index_sql(c: &CreateIndex) -> String {
    let unique = if c.unique { "UNIQUE " } else { "" };
    let exists = if c.if_not_exists {
        "IF NOT EXISTS "
    } else {
        ""
    };
    let name = match &c.name {
        // A name is required by our parser unless `ON` follows immediately, so
        // re-emit the (possibly auto-generated) name to keep replay stable.
        Some(n) => format!("{} ", ident(n)),
        None => String::new(),
    };
    let using = match c.method {
        IndexMethod::Hash => " USING hash",
        IndexMethod::Btree => "",
    };
    let keys: Vec<String> = c
        .keys
        .iter()
        .map(|k| match k {
            IndexKeyExpr::Column(name) => ident(name),
            IndexKeyExpr::Expr(e) => format!("({})", expr_to_sql(e)),
        })
        .collect();
    let include = if c.include.is_empty() {
        String::new()
    } else {
        let cols: Vec<String> = c.include.iter().map(|n| ident(n)).collect();
        format!(" INCLUDE ({})", cols.join(", "))
    };
    let predicate = match &c.predicate {
        Some(p) => format!(" WHERE {}", expr_to_sql(p)),
        None => String::new(),
    };
    format!(
        "CREATE {unique}INDEX {exists}{name}ON {}{using} ({}){include}{predicate}",
        ident(&c.table),
        keys.join(", ")
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
    let overriding = if i.overriding_system_value {
        " OVERRIDING SYSTEM VALUE"
    } else {
        ""
    };
    let conflict = on_conflict_sql(&i.on_conflict);
    if i.default_values {
        return format!(
            "INSERT INTO {}{}{} DEFAULT VALUES{}",
            ident(&i.table),
            cols,
            overriding,
            conflict
        );
    }
    if let Some(sel) = &i.select {
        return format!(
            "INSERT INTO {}{}{} {}{}",
            ident(&i.table),
            cols,
            overriding,
            select_to_sql(sel),
            conflict
        );
    }
    let tuples: Vec<String> = i
        .rows
        .iter()
        .map(|tuple| {
            let vals: Vec<String> = tuple.iter().map(expr_to_sql).collect();
            format!("({})", vals.join(", "))
        })
        .collect();
    format!(
        "INSERT INTO {}{}{} VALUES {}{}",
        ident(&i.table),
        cols,
        overriding,
        tuples.join(", "),
        conflict
    )
}

fn on_conflict_sql(c: &Option<OnConflict>) -> String {
    match c {
        None => String::new(),
        Some(OnConflict::DoNothing { target }) => {
            let target = if target.is_empty() {
                String::new()
            } else {
                let cols: Vec<String> = target.iter().map(|c| ident(c)).collect();
                format!(" ({})", cols.join(", "))
            };
            format!(" ON CONFLICT{target} DO NOTHING")
        }
        Some(OnConflict::DoUpdate {
            target,
            assignments,
            filter,
        }) => {
            let target = if target.is_empty() {
                String::new()
            } else {
                let cols: Vec<String> = target.iter().map(|c| ident(c)).collect();
                format!(" ({})", cols.join(", "))
            };
            let assignments: Vec<String> = assignments
                .iter()
                .map(|(name, expr)| format!("{} = {}", ident(name), expr_to_sql(expr)))
                .collect();
            let filter = filter
                .as_ref()
                .map(|expr| format!(" WHERE {}", expr_to_sql(expr)))
                .unwrap_or_default();
            format!(
                " ON CONFLICT{target} DO UPDATE SET {}{filter}",
                assignments.join(", ")
            )
        }
    }
}

fn update_sql(u: &Update) -> String {
    let sets: Vec<String> = u
        .assignments
        .iter()
        .map(|(c, e)| format!("{} = {}", ident(c), expr_to_sql(e)))
        .collect();
    let mut s = format!("UPDATE {} SET {}", ident(&u.table), sets.join(", "));
    if let Some(from) = &u.from {
        s.push_str(" FROM ");
        s.push_str(&from_clause_to_sql(from));
    }
    if let Some(f) = &u.filter {
        s.push_str(&format!(" WHERE {}", expr_to_sql(f)));
    }
    s
}

fn delete_sql(d: &Delete) -> String {
    let mut s = format!("DELETE FROM {}", ident(&d.table));
    if let Some(using) = &d.using {
        s.push_str(" USING ");
        s.push_str(&from_clause_to_sql(using));
    }
    if let Some(f) = &d.filter {
        s.push_str(&format!(" WHERE {}", expr_to_sql(f)));
    }
    s
}

fn merge_sql(m: &Merge) -> String {
    let mut s = format!("MERGE INTO {}", ident(&m.target));
    if let Some(alias) = &m.target_alias {
        s.push_str(&format!(" AS {}", ident(alias)));
    }
    s.push_str(" USING ");
    s.push_str(&merge_source_sql(&m.source));
    s.push_str(&format!(" ON {}", expr_to_sql(&m.on)));
    for when in &m.clauses {
        s.push_str(&merge_when_sql(when));
    }
    s
}

fn merge_source_sql(source: &MergeSource) -> String {
    match source {
        MergeSource::Table { name, alias } => match alias {
            Some(alias) => format!("{} AS {}", ident(name), ident(alias)),
            None => ident(name),
        },
        MergeSource::Subquery { select, alias } => {
            format!("({}) AS {}", select_to_sql(select), ident(alias))
        }
        MergeSource::Values {
            rows,
            alias,
            columns,
        } => {
            let tuples: Vec<String> = rows
                .iter()
                .map(|tuple| {
                    let vals: Vec<String> = tuple.iter().map(expr_to_sql).collect();
                    format!("({})", vals.join(", "))
                })
                .collect();
            let cols = if columns.is_empty() {
                String::new()
            } else {
                let list: Vec<String> = columns.iter().map(|c| ident(c)).collect();
                format!(" ({})", list.join(", "))
            };
            format!(
                "(VALUES {}) AS {}{}",
                tuples.join(", "),
                ident(alias),
                cols
            )
        }
    }
}

fn merge_when_sql(when: &MergeWhen) -> String {
    let head = if when.matched {
        " WHEN MATCHED"
    } else {
        " WHEN NOT MATCHED"
    };
    let cond = when
        .condition
        .as_ref()
        .map(|c| format!(" AND {}", expr_to_sql(c)))
        .unwrap_or_default();
    let action = match &when.action {
        MergeAction::Update { assignments } => {
            let sets: Vec<String> = assignments
                .iter()
                .map(|(c, e)| format!("{} = {}", ident(c), expr_to_sql(e)))
                .collect();
            format!("UPDATE SET {}", sets.join(", "))
        }
        MergeAction::Delete => "DELETE".into(),
        MergeAction::DoNothing => "DO NOTHING".into(),
        MergeAction::Insert {
            columns,
            values,
            default_values,
        } => {
            if *default_values {
                "INSERT DEFAULT VALUES".into()
            } else {
                let cols = match columns {
                    Some(names) => {
                        let list: Vec<String> = names.iter().map(|n| ident(n)).collect();
                        format!(" ({})", list.join(", "))
                    }
                    None => String::new(),
                };
                let vals: Vec<String> = values.iter().map(expr_to_sql).collect();
                format!("INSERT{} VALUES ({})", cols, vals.join(", "))
            }
        }
    };
    format!("{head}{cond} THEN {action}")
}

/// Serialize a `SELECT` back to SQL. Used for subqueries embedded in logged
/// DML and (harmlessly) for any standalone SELECT.
pub fn select_to_sql(sel: &Select) -> String {
    let mut s = String::new();
    if !sel.ctes.is_empty() {
        let recursive = sel.ctes.iter().any(|c| c.recursive);
        let ctes: Vec<String> = sel
            .ctes
            .iter()
            .map(|cte| {
                let columns = if cte.columns.is_empty() {
                    String::new()
                } else {
                    let columns: Vec<String> = cte.columns.iter().map(|name| ident(name)).collect();
                    format!(" ({})", columns.join(", "))
                };
                let body = match &cte.dml {
                    Some(stmt) => statement_to_sql(stmt),
                    None => select_to_sql(&cte.select),
                };
                format!("{}{} AS ({})", ident(&cte.name), columns, body)
            })
            .collect();
        let kw = if recursive { "WITH RECURSIVE " } else { "WITH " };
        s.push_str(&format!("{kw}{} ", ctes.join(", ")));
    }
    s.push_str("SELECT ");
    if sel.distinct {
        if sel.distinct_on.is_empty() {
            s.push_str("DISTINCT ");
        } else {
            let keys: Vec<String> = sel.distinct_on.iter().map(expr_to_sql).collect();
            s.push_str(&format!("DISTINCT ON ({}) ", keys.join(", ")));
        }
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
    } else if !sel.grouping_sets.is_empty() {
        let sets: Vec<String> = sel
            .grouping_sets
            .iter()
            .map(|set| {
                let cols: Vec<String> = set.iter().map(expr_to_sql).collect();
                format!("({})", cols.join(", "))
            })
            .collect();
        s.push_str(&format!(" GROUP BY GROUPING SETS ({})", sets.join(", ")));
    }
    if let Some(h) = &sel.having {
        s.push_str(&format!(" HAVING {}", expr_to_sql(h)));
    }
    for set_op in &sel.set_ops {
        let op = match set_op.op {
            SetOperator::Union => "UNION",
            SetOperator::Intersect => "INTERSECT",
            SetOperator::Except => "EXCEPT",
        };
        let all = if set_op.all { " ALL" } else { "" };
        s.push_str(&format!(" {op}{all} {}", select_to_sql(&set_op.select)));
    }
    if !sel.order_by.is_empty() {
        let o: Vec<String> = sel
            .order_by
            .iter()
            .map(|ob| {
                format!(
                    "{}{}",
                    expr_to_sql(&ob.expr),
                    if ob.asc { "" } else { " DESC" }
                )
            })
            .collect();
        s.push_str(&format!(" ORDER BY {}", o.join(", ")));
    }
    if let Some(l) = &sel.limit {
        s.push_str(&format!(" LIMIT {}", expr_to_sql(l)));
    }
    if let Some(o) = &sel.offset {
        s.push_str(&format!(" OFFSET {}", expr_to_sql(o)));
    }
    for locking in &sel.locking {
        s.push_str(&row_locking_clause_to_sql(locking));
    }
    s
}

fn row_locking_clause_to_sql(locking: &RowLockingClause) -> String {
    let mode = match locking.mode {
        RowLockingMode::Update => "FOR UPDATE",
        RowLockingMode::NoKeyUpdate => "FOR NO KEY UPDATE",
        RowLockingMode::Share => "FOR SHARE",
        RowLockingMode::KeyShare => "FOR KEY SHARE",
    };
    let mut out = format!(" {mode}");
    if !locking.tables.is_empty() {
        let tables: Vec<String> = locking.tables.iter().map(|name| ident(name)).collect();
        out.push_str(&format!(" OF {}", tables.join(", ")));
    }
    match locking.wait_policy {
        Some(RowLockingWaitPolicy::NoWait) => out.push_str(" NOWAIT"),
        Some(RowLockingWaitPolicy::SkipLocked) => out.push_str(" SKIP LOCKED"),
        None => {}
    }
    out
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
    if t.lateral {
        s.push_str("LATERAL ");
    }
    if let Some(sub) = &t.subquery {
        s.push_str(&format!("({})", select_to_sql(sub)));
        if let Some(a) = &t.alias {
            s.push_str(&format!(" AS {}", ident(a)));
        }
        return s;
    }
    if let Some(schema) = &t.schema {
        s.push_str(&ident(schema));
        s.push('.');
    }
    s.push_str(&ident(&t.name));
    if !t.args.is_empty() {
        let args: Vec<String> = t.args.iter().map(expr_to_sql).collect();
        s.push_str(&format!("({})", args.join(", ")));
    }
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
        Expr::QualifiedColumn { qualifier, name } => {
            format!("{}.{}", ident(qualifier), ident(name))
        }
        Expr::Unary { op, expr } => {
            let inner = expr_to_sql(expr);
            match op {
                UnaryOp::Neg => format!("(-{inner})"),
                UnaryOp::Not => format!("(NOT {inner})"),
            }
        }
        Expr::Binary { op, left, right } => {
            format!(
                "({} {} {})",
                expr_to_sql(left),
                binop_sql(*op),
                expr_to_sql(right)
            )
        }
        Expr::QuantifiedCompare {
            left,
            op,
            quantifier,
            list,
        } => {
            let items: Vec<String> = list.iter().map(expr_to_sql).collect();
            format!(
                "({} {} {} ({}))",
                expr_to_sql(left),
                binop_sql(*op),
                quantifier_sql(*quantifier),
                items.join(", ")
            )
        }
        Expr::Row(items) => {
            let items: Vec<String> = items.iter().map(expr_to_sql).collect();
            format!("ROW({})", items.join(", "))
        }
        Expr::Array(items) => {
            let items: Vec<String> = items.iter().map(expr_to_sql).collect();
            format!("ARRAY[{}]", items.join(", "))
        }
        Expr::IsNull { expr, negated } => {
            let kw = if *negated { "IS NOT NULL" } else { "IS NULL" };
            format!("({} {kw})", expr_to_sql(expr))
        }
        Expr::IsDistinctFrom {
            left,
            right,
            negated,
        } => {
            let kw = if *negated {
                "IS NOT DISTINCT FROM"
            } else {
                "IS DISTINCT FROM"
            };
            format!("({} {kw} {})", expr_to_sql(left), expr_to_sql(right))
        }
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => {
            let op = match (*negated, *case_insensitive) {
                (false, false) => "LIKE",
                (true, false) => "NOT LIKE",
                (false, true) => "ILIKE",
                (true, true) => "NOT ILIKE",
            };
            format!("({} {op} {})", expr_to_sql(expr), expr_to_sql(pattern))
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let items: Vec<String> = list.iter().map(expr_to_sql).collect();
            let op = if *negated { "NOT IN" } else { "IN" };
            format!("({} {op} ({}))", expr_to_sql(expr), items.join(", "))
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let op = if *negated { "NOT BETWEEN" } else { "BETWEEN" };
            format!(
                "({} {op} {} AND {})",
                expr_to_sql(expr),
                expr_to_sql(low),
                expr_to_sql(high)
            )
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
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
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => {
            let op = if *negated { "NOT IN" } else { "IN" };
            format!("({} {op} ({}))", expr_to_sql(expr), select_to_sql(subquery))
        }
        Expr::Function {
            name,
            args,
            star,
            distinct,
            filter,
            over,
        } => {
            let mut call = if *star {
                format!("{name}(*)")
            } else {
                let a: Vec<String> = args.iter().map(expr_to_sql).collect();
                let d = if *distinct { "DISTINCT " } else { "" };
                format!("{name}({d}{})", a.join(", "))
            };
            if let Some(filter) = filter {
                call.push_str(&format!(" FILTER (WHERE {})", expr_to_sql(filter)));
            }
            if let Some(spec) = over {
                call.push_str(&format!(" OVER ({})", window_spec_sql(spec)));
            }
            call
        }
    }
}

fn window_spec_sql(spec: &WindowSpec) -> String {
    let mut parts = Vec::new();
    if !spec.partition_by.is_empty() {
        let p: Vec<String> = spec.partition_by.iter().map(expr_to_sql).collect();
        parts.push(format!("PARTITION BY {}", p.join(", ")));
    }
    if !spec.order_by.is_empty() {
        let o: Vec<String> = spec
            .order_by
            .iter()
            .map(|item| {
                let dir = if item.asc { "" } else { " DESC" };
                format!("{}{}", expr_to_sql(&item.expr), dir)
            })
            .collect();
        parts.push(format!("ORDER BY {}", o.join(", ")));
    }
    parts.join(" ")
}

fn quantifier_sql(q: Quantifier) -> &'static str {
    match q {
        Quantifier::Any => "ANY",
        Quantifier::Some => "SOME",
        Quantifier::All => "ALL",
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
        BinaryOp::JsonGet => "->",
        BinaryOp::JsonGetText => "->>",
        BinaryOp::ArrayContains => "@>",
        BinaryOp::TextSearchMatch => "@@",
        BinaryOp::ArrayContainedBy => "<@",
        BinaryOp::ArrayOverlap => "&&",
        BinaryOp::NetworkContainedBy => "<<",
        BinaryOp::NetworkContainedByEq => "<<=",
        BinaryOp::NetworkContains => ">>",
        BinaryOp::NetworkContainsEq => ">>=",
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
        && name
            .bytes()
            .next()
            .is_some_and(|b| b == b'_' || b.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b == b'_' || b.is_ascii_lowercase() || b.is_ascii_digit());
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
