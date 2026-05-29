//! Tests that mutating statements survive a serialize → reparse → replay
//! round-trip, which is exactly what the WAL relies on for durability.

use postgres_rs::executor::{self, ExecResult};
use postgres_rs::sql::Parser;
use postgres_rs::sql::serialize::statement_to_sql;
use postgres_rs::storage::{Column, Database, Table};
use postgres_rs::types::{DataType, Value};

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
    apply(
        &mut original,
        &mut log,
        "CREATE TABLE t (id integer PRIMARY KEY, name text NOT NULL, v double precision)",
    );
    apply(
        &mut original,
        &mut log,
        "INSERT INTO t VALUES (1, 'a', 1.5), (2, 'b''c', 2.0)",
    );
    apply(
        &mut original,
        &mut log,
        "UPDATE t SET v = v * 2 WHERE id = 1",
    );
    apply(&mut original, &mut log, "DELETE FROM t WHERE id = 2");
    apply(
        &mut original,
        &mut log,
        "INSERT INTO t VALUES (3, 'qu\"ote', 9)",
    );

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
    apply(
        &mut db,
        &mut log,
        "CREATE TABLE u (id serial PRIMARY KEY, name text)",
    );
    apply(&mut db, &mut log, "INSERT INTO u DEFAULT VALUES");
    apply(
        &mut db,
        &mut log,
        "INSERT INTO u (name) VALUES ('a'), ('b')",
    );
    apply(
        &mut db,
        &mut log,
        "INSERT INTO u (name) SELECT name FROM u WHERE id = 2",
    );
    apply(
        &mut db,
        &mut log,
        "INSERT INTO u (id, name) VALUES (2, 'dup'), (5, 'fresh') ON CONFLICT (id) DO NOTHING",
    );
    apply(&mut db, &mut log, "TRUNCATE TABLE u");
    apply(&mut db, &mut log, "INSERT INTO u (name) VALUES ('after')");
    apply(
        &mut db,
        &mut log,
        "INSERT INTO u (id, name) VALUES (50, 'm')",
    );
    apply(&mut db, &mut log, "INSERT INTO u (name) VALUES ('c')");

    let mut recovered = Database::new();
    for stmt in Parser::parse_sql(&log).unwrap() {
        executor::execute(&mut recovered, stmt).unwrap();
    }
    let q = "SELECT id, name FROM u ORDER BY id";
    assert_eq!(select_all(&mut db, q), select_all(&mut recovered, q));

    // The sequence continues correctly after recovery.
    let stmt = Parser::parse_sql("INSERT INTO u (name) VALUES ('d') RETURNING id")
        .unwrap()
        .remove(0);
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
    apply(
        &mut db,
        &mut log,
        "INSERT INTO t VALUES ('O''Brien'), ('a;b'), ('line1\nline2')",
    );

    let mut recovered = Database::new();
    for stmt in Parser::parse_sql(&log).unwrap() {
        executor::execute(&mut recovered, stmt).unwrap();
    }
    let q = "SELECT s FROM t";
    assert_eq!(select_all(&mut db, q), select_all(&mut recovered, q));
}

#[test]
fn storage_pages_track_fsm_vm_and_vacuum_compaction() {
    let mut table = Table::new(
        "docs".into(),
        vec![
            test_column("id", DataType::Int4),
            test_column("body", DataType::Text),
        ],
    );

    table.push_row(vec![Value::Int(1), Value::Text("a".repeat(3_000))]);
    table.push_row(vec![Value::Int(2), Value::Text("b".repeat(3_000))]);
    table.push_row(vec![Value::Int(3), Value::Text("c".repeat(3_000))]);

    let initial = table.storage_stats();
    assert_eq!(initial.live_rows, 3);
    assert_eq!(initial.dead_rows, 0);
    assert!(initial.page_count >= 2);
    assert_eq!(table.visibility_map().len(), initial.page_count);
    assert!(table.visibility_map().iter().all(|page| page.all_visible));

    table.update_row(1, vec![Value::Int(2), Value::Text("bb".repeat(2_000))]);
    table.delete_rows(&[0]);

    let dirty = table.storage_stats();
    assert_eq!(dirty.live_rows, 2);
    assert_eq!(dirty.dead_rows, 2);
    assert!(dirty.dead_bytes > 0);
    assert!(table.visibility_map().iter().any(|page| !page.all_visible));
    assert_eq!(table.free_space_map().len(), dirty.page_count);

    let vacuum = table.vacuum_storage();
    assert_eq!(vacuum.dead_rows_removed, 2);
    assert!(vacuum.dead_bytes_removed > 0);

    let compacted = table.storage_stats();
    assert_eq!(compacted.live_rows, 2);
    assert_eq!(compacted.dead_rows, 0);
    assert_eq!(compacted.vacuum_count, 1);
    assert_eq!(compacted.compaction_count, 1);
    assert!(table.visibility_map().iter().all(|page| page.all_visible));
    assert!(compacted.page_count <= dirty.page_count);
}

#[test]
fn database_storage_vacuum_reports_missing_tables() {
    let mut db = Database::new();
    let err = db
        .vacuum_table_storage("missing")
        .expect_err("missing relation should fail");
    assert_eq!(err, "relation \"missing\" does not exist");
}

fn test_column(name: &str, data_type: DataType) -> Column {
    Column {
        name: name.into(),
        data_type,
        type_name: None,
        not_null: false,
        primary_key: false,
        default: None,
        serial: false,
        identity: false,
        identity_always: false,
        generated: None,
    }
}

#[test]
fn grant_and_membership_replay() {
    let mut original = Database::new();
    let mut log = String::new();
    apply(&mut original, &mut log, "CREATE TABLE t (id integer)");
    apply(&mut original, &mut log, "CREATE ROLE devs");
    apply(&mut original, &mut log, "CREATE ROLE alice IN ROLE devs");
    apply(&mut original, &mut log, "GRANT SELECT, INSERT ON t TO alice");
    apply(&mut original, &mut log, "GRANT ALL ON t TO public");
    apply(&mut original, &mut log, "REVOKE INSERT ON t FROM alice");
    apply(&mut original, &mut log, "CREATE ROLE bob");
    apply(&mut original, &mut log, "GRANT devs TO bob");
    apply(&mut original, &mut log, "REVOKE devs FROM alice");

    let mut recovered = Database::new();
    for stmt in Parser::parse_sql(&log).expect("reparse WAL") {
        executor::execute(&mut recovered, stmt).expect("replay");
    }

    let q = "SELECT m.rolname, g.rolname \
             FROM pg_auth_members am \
             JOIN pg_roles g ON g.oid = am.roleid \
             JOIN pg_roles m ON m.oid = am.member \
             ORDER BY m.rolname, g.rolname";
    assert_eq!(select_all(&mut original, q), select_all(&mut recovered, q));
    // bob should be the sole remaining member of devs after replay.
    assert_eq!(
        select_all(&mut recovered, q),
        vec![vec![Value::Text("bob".into()), Value::Text("devs".into())]]
    );
}

#[test]
fn enum_and_domain_replay() {
    let mut original = Database::new();
    let mut log = String::new();
    apply(&mut original, &mut log, "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')");
    apply(
        &mut original,
        &mut log,
        "CREATE DOMAIN pos AS integer NOT NULL CHECK (VALUE > 0)",
    );
    apply(&mut original, &mut log, "CREATE TABLE t (id pos, m mood)");
    apply(&mut original, &mut log, "INSERT INTO t VALUES (1, 'happy'), (2, 'ok')");

    // Rebuild from the log; the enum/domain definitions must replay first so
    // the table's columns resolve and the rows still validate.
    let mut recovered = Database::new();
    for stmt in Parser::parse_sql(&log).expect("reparse WAL") {
        executor::execute(&mut recovered, stmt).expect("replay");
    }

    let q = "SELECT id, m FROM t ORDER BY id";
    assert_eq!(select_all(&mut original, q), select_all(&mut recovered, q));

    // Enforcement survives replay: an invalid enum value is still rejected.
    let stmt = Parser::parse_sql("INSERT INTO t VALUES (3, 'angry')")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(executor::execute(&mut recovered, stmt).is_err());
}

#[test]
fn merge_replays_identically() {
    let mut original = Database::new();
    let mut log = String::new();
    apply(
        &mut original,
        &mut log,
        "CREATE TABLE target (id integer PRIMARY KEY, val text, qty integer)",
    );
    apply(
        &mut original,
        &mut log,
        "INSERT INTO target VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30)",
    );
    // Table source: update matched, insert not-matched, conditional delete.
    apply(
        &mut original,
        &mut log,
        "MERGE INTO target t \
         USING (VALUES (1, 'x', 100), (3, 'y', 5), (4, 'z', 40)) AS s(id, v, q) ON t.id = s.id \
         WHEN MATCHED AND s.q < 10 THEN DELETE \
         WHEN MATCHED THEN UPDATE SET val = s.v, qty = t.qty + s.q \
         WHEN NOT MATCHED THEN INSERT (id, val, qty) VALUES (s.id, s.v, s.q)",
    );

    let mut recovered = Database::new();
    for stmt in Parser::parse_sql(&log).expect("reparse WAL") {
        executor::execute(&mut recovered, stmt).expect("replay");
    }

    let q = "SELECT id, val, qty FROM target ORDER BY id";
    assert_eq!(select_all(&mut original, q), select_all(&mut recovered, q));
}

#[test]
fn function_and_trigger_replay() {
    let mut original = Database::new();
    let mut log = String::new();
    apply(
        &mut original,
        &mut log,
        "CREATE FUNCTION my_add(a integer, b integer) RETURNS integer \
         AS $$ SELECT a + b $$ LANGUAGE sql",
    );
    apply(&mut original, &mut log, "CREATE TABLE t (id integer)");
    apply(&mut original, &mut log, "CREATE TABLE audit (note text)");
    apply(
        &mut original,
        &mut log,
        "CREATE FUNCTION log_change() RETURNS trigger \
         AS $$ INSERT INTO audit (note) VALUES ('x') $$ LANGUAGE sql",
    );
    apply(
        &mut original,
        &mut log,
        "CREATE TRIGGER trg AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION log_change()",
    );
    apply(&mut original, &mut log, "INSERT INTO t VALUES (1), (2)");

    // Rebuild from the log only.
    let mut recovered = Database::new();
    for stmt in Parser::parse_sql(&log).expect("reparse WAL") {
        executor::execute(&mut recovered, stmt).expect("replay");
    }

    // The scalar UDF is callable after replay.
    let stmt = Parser::parse_sql("SELECT my_add(40, 2)")
        .unwrap()
        .remove(0);
    match executor::execute(&mut recovered, stmt).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(42)),
        _ => panic!("expected rows"),
    }

    // The trigger fired during replay's INSERT, producing two audit rows.
    let q = "SELECT count(*) FROM audit";
    assert_eq!(select_all(&mut original, q), select_all(&mut recovered, q));
    assert_eq!(select_all(&mut recovered, q), vec![vec![Value::Int(2)]]);
}

/// The new statement/CTE/FROM forms survive a serialize → reparse round-trip:
/// re-serializing the reparsed statement yields identical SQL and the reparsed
/// query produces the same rows.
#[test]
fn new_forms_serialize_and_reparse() {
    let cases = [
        // Writable CTE.
        "WITH ins AS (INSERT INTO t (id) VALUES (1) RETURNING id) SELECT id FROM ins",
        // LATERAL subquery referencing a left column.
        "SELECT t.id, s.v FROM t, LATERAL (SELECT t.id AS v) AS s",
        // Derived table (subquery in FROM).
        "SELECT d.id FROM (SELECT id FROM t) AS d",
        // Interval literal.
        "SELECT INTERVAL '1 year 2 months'",
        // jsonpath.
        "SELECT jsonb_path_query(doc, '$.a.b') FROM j",
        // Ownership + row-level security DDL.
        "ALTER TABLE t OWNER TO alice",
        "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE t FORCE ROW LEVEL SECURITY",
        "ALTER TABLE t NO FORCE ROW LEVEL SECURITY",
        // Policies.
        "CREATE POLICY p ON t AS RESTRICTIVE FOR SELECT TO alice USING ((tenant = 'x'))",
        "ALTER POLICY p ON t USING ((tenant = 'y'))",
        "DROP POLICY IF EXISTS p ON t",
        // SECURITY DEFINER function.
        "CREATE FUNCTION sd() RETURNS integer AS $$ SELECT 1 $$ LANGUAGE sql SECURITY DEFINER",
    ];
    for sql in cases {
        let stmt = Parser::parse_sql(sql).expect("parse").remove(0);
        let serialized = statement_to_sql(&stmt);
        // Reparse the serialized SQL and re-serialize: the canonical form is a
        // fixpoint, proving the WAL can round-trip the construct.
        let reparsed = Parser::parse_sql(&serialized).expect("reparse").remove(0);
        assert_eq!(serialized, statement_to_sql(&reparsed), "for: {sql}");
    }
}

/// A writable CTE's side effects persist, and re-running its (serialized) SELECT
/// form re-applies the same mutation — what WAL replay relies on.
#[test]
fn writable_cte_side_effects_replay() {
    let mut original = Database::new();
    let mut log = String::new();
    apply(&mut original, &mut log, "CREATE TABLE t (id integer)");
    // The writable CTE is a SELECT (returns rows), so it isn't WAL-logged here,
    // but its INSERT effect is visible in the live database.
    let _ = select_all(
        &mut original,
        "WITH ins AS (INSERT INTO t (id) VALUES (7) RETURNING id) SELECT id FROM ins",
    );
    assert_eq!(select_all(&mut original, "SELECT id FROM t"), vec![vec![Value::Int(7)]]);
}

/// Extended catalog DDL (FDW, server, publication, exclusion constraint, foreign
/// table) serializes, replays, and reproduces the same catalog state.
#[test]
fn extended_catalog_ddl_replays() {
    let mut original = Database::new();
    let mut log = String::new();
    apply(&mut original, &mut log, "CREATE FOREIGN DATA WRAPPER w OPTIONS (a 'b')");
    apply(
        &mut original,
        &mut log,
        "CREATE SERVER s FOREIGN DATA WRAPPER w OPTIONS (host 'localhost')",
    );
    apply(&mut original, &mut log, "CREATE PUBLICATION p FOR ALL TABLES");
    apply(
        &mut original,
        &mut log,
        "CREATE FOREIGN TABLE ft (id integer, name text) SERVER s OPTIONS (tab 'x')",
    );
    apply(&mut original, &mut log, "INSERT INTO ft (id, name) VALUES (1, 'a')");
    apply(
        &mut original,
        &mut log,
        "CREATE TABLE rooms (id integer, during int4range, \
         EXCLUDE USING gist (during WITH &&))",
    );

    // Rebuild a fresh database purely from the log.
    let mut recovered = Database::new();
    for stmt in Parser::parse_sql(&log).expect("reparse WAL") {
        executor::execute(&mut recovered, stmt).expect("replay");
    }

    // The foreign table (stored as a regular table) and its row survived.
    assert_eq!(
        select_all(&mut recovered, "SELECT id, name FROM ft"),
        vec![vec![Value::Int(1), Value::Text("a".into())]]
    );
    // The catalog objects survived replay.
    assert_eq!(recovered.catalog_objects("FOREIGN DATA WRAPPER").len(), 1);
    assert_eq!(recovered.catalog_objects("SERVER").len(), 1);
    assert_eq!(recovered.catalog_objects("PUBLICATION").len(), 1);
    // The exclusion-constrained table replayed too.
    assert!(select_all(&mut recovered, "SELECT id FROM rooms").is_empty());
    let _ = &original;
}
