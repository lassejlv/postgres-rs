//! ORM-compatibility fixtures: load representative ORM-style schema + query SQL
//! through the executor and assert it runs and introspects as expected.
//!
//! These are hand-written fixtures (not a real ORM), but they mirror what
//! Prisma/Drizzle emit: serial PKs, UNIQUE, timestamps, foreign keys, and the
//! information_schema introspection queries clients run to discover the schema.

use std::path::PathBuf;

use postgres_rs::executor::{self, ExecResult};
use postgres_rs::sql::Parser;
use postgres_rs::storage::Database;
use postgres_rs::types::Value;

fn fixture(name: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

/// Run every statement in `sql`, asserting none error. Returns the last result.
fn run_script(db: &mut Database, sql: &str) -> ExecResult {
    let mut last = ExecResult::Empty;
    for stmt in Parser::parse_sql(sql).expect("parse fixture SQL") {
        last = executor::execute(db, stmt).expect("execute fixture statement");
    }
    last
}

/// Run a single query and return its rows.
fn query(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    let stmt = Parser::parse_sql(sql).unwrap().into_iter().next().unwrap();
    match executor::execute(db, stmt).expect("execute query") {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {}", tag_of(&other)),
    }
}

fn tag_of(res: &ExecResult) -> String {
    match res {
        ExecResult::Rows { tag, .. } => tag.clone(),
        ExecResult::Command(c) => c.clone(),
        ExecResult::Empty => "<empty>".into(),
    }
}

fn cell(row: &[Value], i: usize) -> String {
    row[i].to_text().unwrap_or_else(|| "NULL".into())
}

/// The whole schema fixture loads without error and seeds the expected data.
#[test]
fn orm_schema_loads() {
    let mut db = Database::new();
    run_script(&mut db, &fixture("orm_schema.sql"));

    let users = query(&mut db, "SELECT count(*) FROM \"User\"");
    assert_eq!(cell(&users[0], 0), "2");

    let posts = query(&mut db, "SELECT count(*) FROM \"Post\"");
    assert_eq!(cell(&posts[0], 0), "3");

    // serial PK auto-assigned sequential ids.
    let ids = query(&mut db, "SELECT id FROM \"User\" ORDER BY id");
    assert_eq!(cell(&ids[0], 0), "1");
    assert_eq!(cell(&ids[1], 0), "2");
}

/// The serial PRIMARY KEY rejects a duplicate key (a constraint ORMs rely on
/// when they upsert by id).
#[test]
fn orm_primary_key_enforced() {
    let mut db = Database::new();
    run_script(&mut db, &fixture("orm_schema.sql"));
    let dup = Parser::parse_sql(
        "INSERT INTO \"User\" (id, email, created_at) VALUES (1, 'new@example.com', '2024-02-01 00:00:00')",
    )
    .unwrap()
    .into_iter()
    .next()
    .unwrap();
    assert!(
        executor::execute(&mut db, dup).is_err(),
        "duplicate primary key id=1 should be rejected"
    );
}

/// A join across the foreign key returns the joined rows.
#[test]
fn orm_join_over_fk() {
    let mut db = Database::new();
    run_script(&mut db, &fixture("orm_schema.sql"));
    let rows = query(
        &mut db,
        "SELECT u.name, p.title FROM \"Post\" p JOIN \"User\" u ON p.author_id = u.id ORDER BY p.id",
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(cell(&rows[0], 0), "Alice");
    assert_eq!(cell(&rows[0], 1), "Hello World");
    assert_eq!(cell(&rows[2], 0), "Bob");
}

/// The introspection queries return the expected metadata rows.
#[test]
fn orm_introspection() {
    let mut db = Database::new();
    run_script(&mut db, &fixture("orm_schema.sql"));

    // Split the introspection fixture on `;` and run each query, asserting the
    // shape of the well-known ones.
    let tables = query(
        &mut db,
        "SELECT table_name FROM information_schema.tables WHERE table_schema='public' AND table_type='BASE TABLE' ORDER BY table_name",
    );
    let names: Vec<String> = tables.iter().map(|r| cell(r, 0)).collect();
    assert_eq!(names, vec!["Post".to_string(), "User".to_string()]);

    let cols = query(
        &mut db,
        "SELECT column_name, data_type, is_nullable FROM information_schema.columns WHERE table_name='User' ORDER BY ordinal_position",
    );
    let col_names: Vec<String> = cols.iter().map(|r| cell(r, 0)).collect();
    assert_eq!(
        col_names,
        vec!["id", "email", "name", "created_at"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
    );
    // email is NOT NULL, name is nullable.
    assert_eq!(cell(&cols[1], 2), "NO");
    assert_eq!(cell(&cols[2], 2), "YES");

    let constraints = query(
        &mut db,
        "SELECT constraint_type FROM information_schema.table_constraints WHERE table_name='Post' ORDER BY constraint_type",
    );
    let types: Vec<String> = constraints.iter().map(|r| cell(r, 0)).collect();
    assert!(types.contains(&"FOREIGN KEY".to_string()), "got {types:?}");
    assert!(types.contains(&"PRIMARY KEY".to_string()), "got {types:?}");
}

/// The introspection fixture file itself parses and every statement executes.
#[test]
fn orm_introspect_fixture_runs() {
    let mut db = Database::new();
    run_script(&mut db, &fixture("orm_schema.sql"));
    let sql = fixture("orm_introspect.sql");
    for stmt in Parser::parse_sql(&sql).expect("parse introspection fixture") {
        executor::execute(&mut db, stmt).expect("introspection query runs");
    }
}
