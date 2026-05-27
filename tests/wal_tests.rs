//! Tests that mutating statements survive a serialize → reparse → replay
//! round-trip, which is exactly what the WAL relies on for durability.

use postgres_rs::executor::{self, ExecResult};
use postgres_rs::sql::Parser;
use postgres_rs::sql::serialize::statement_to_sql;
use postgres_rs::storage::Database;
use postgres_rs::types::Value;

/// Apply SQL to `db`, logging the canonical SQL of each mutating statement
/// into `log` (mimicking the WAL).
fn apply(db: &mut Database, log: &mut String, sql: &str) {
    for stmt in Parser::parse_sql(sql).expect("parse") {
        let serialized = statement_to_sql(&stmt);
        let res = executor::execute(db, stmt).expect("execute");
        // Only DDL/DML produce a non-empty serialization worth logging.
        if !serialized.is_empty() && !matches!(res, ExecResult::Rows { .. }) {
            log.push_str(&serialized);
            log.push_str(";\n");
        }
    }
}

fn select_all(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    let stmt = Parser::parse_sql(sql).unwrap().into_iter().next().unwrap();
    match executor::execute(db, stmt).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows"),
    }
}

#[test]
fn replay_reproduces_state() {
    let mut original = Database::new();
    let mut log = String::new();
    apply(&mut original, &mut log, "CREATE TABLE t (id integer PRIMARY KEY, name text NOT NULL, v double precision)");
    apply(&mut original, &mut log, "INSERT INTO t VALUES (1, 'a', 1.5), (2, 'b''c', 2.0)");
    apply(&mut original, &mut log, "UPDATE t SET v = v * 2 WHERE id = 1");
    apply(&mut original, &mut log, "DELETE FROM t WHERE id = 2");
    apply(&mut original, &mut log, "INSERT INTO t VALUES (3, 'qu\"ote', 9)");

    // Rebuild a fresh database purely from the log.
    let mut recovered = Database::new();
    for stmt in Parser::parse_sql(&log).expect("reparse WAL") {
        executor::execute(&mut recovered, stmt).expect("replay");
    }

    let q = "SELECT id, name, v FROM t ORDER BY id";
    assert_eq!(select_all(&mut original, q), select_all(&mut recovered, q));
}

#[test]
fn serial_replays_identically() {
    let mut db = Database::new();
    let mut log = String::new();
    apply(&mut db, &mut log, "CREATE TABLE u (id serial PRIMARY KEY, name text)");
    apply(&mut db, &mut log, "INSERT INTO u (name) VALUES ('a'), ('b')");
    apply(&mut db, &mut log, "INSERT INTO u (id, name) VALUES (50, 'm')");
    apply(&mut db, &mut log, "INSERT INTO u (name) VALUES ('c')");

    let mut recovered = Database::new();
    for stmt in Parser::parse_sql(&log).unwrap() {
        executor::execute(&mut recovered, stmt).unwrap();
    }
    let q = "SELECT id, name FROM u ORDER BY id";
    assert_eq!(select_all(&mut db, q), select_all(&mut recovered, q));

    // The sequence continues correctly after recovery.
    let stmt = Parser::parse_sql("INSERT INTO u (name) VALUES ('d') RETURNING id").unwrap().remove(0);
    match executor::execute(&mut recovered, stmt).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(52)),
        _ => panic!("expected RETURNING rows"),
    }
}

#[test]
fn string_escaping_round_trips() {
    let mut db = Database::new();
    let mut log = String::new();
    apply(&mut db, &mut log, "CREATE TABLE t (s text)");
    apply(&mut db, &mut log, "INSERT INTO t VALUES ('O''Brien'), ('a;b'), ('line1\nline2')");

    let mut recovered = Database::new();
    for stmt in Parser::parse_sql(&log).unwrap() {
        executor::execute(&mut recovered, stmt).unwrap();
    }
    let q = "SELECT s FROM t";
    assert_eq!(select_all(&mut db, q), select_all(&mut recovered, q));
}
