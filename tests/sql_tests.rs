//! End-to-end tests exercising the parser + executor against the in-memory
//! engine, independent of the wire protocol.

use postgres_rs::executor::{self, ExecResult};
use postgres_rs::sql::Parser;
use postgres_rs::storage::Database;
use postgres_rs::types::Value;

/// Run one SQL string (which may contain several `;`-separated statements)
/// and return the result of the *last* statement.
fn run(db: &mut Database, sql: &str) -> ExecResult {
    let stmts = Parser::parse_sql(sql).expect("parse");
    let mut last = ExecResult::Empty;
    for s in stmts {
        last = executor::execute(db, s).expect("execute");
    }
    last
}

fn rows(res: ExecResult) -> Vec<Vec<Value>> {
    match res {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {}", tag_of(&other)),
    }
}

fn tag_of(res: &ExecResult) -> String {
    match res {
        ExecResult::Rows { .. } => "Rows".into(),
        ExecResult::Command(t) => format!("Command({t})"),
        ExecResult::Empty => "Empty".into(),
    }
}

#[test]
fn create_insert_select() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, name text)");
    let r = run(
        &mut db,
        "INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b')",
    );
    assert!(matches!(r, ExecResult::Command(ref t) if t == "INSERT 0 2"));

    let r = rows(run(&mut db, "SELECT id, name FROM t ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Text("a".into())],
            vec![Value::Int(2), Value::Text("b".into())],
        ]
    );
}

#[test]
fn where_and_ordering() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE n (x integer)");
    run(&mut db, "INSERT INTO n VALUES (5), (1), (3), (2), (4)");
    let r = rows(run(&mut db, "SELECT x FROM n WHERE x > 2 ORDER BY x DESC"));
    let got: Vec<i64> = r
        .iter()
        .map(|row| match row[0] {
            Value::Int(i) => i,
            _ => panic!(),
        })
        .collect();
    assert_eq!(got, vec![5, 4, 3]);
}

#[test]
fn large_select_filter_preserves_order_and_projection() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE events (id integer, category text, score integer)",
    );
    let values = (0..256)
        .map(|i| {
            let category = if i % 2 == 0 { "even" } else { "odd" };
            format!("({i}, '{category}', {})", i * 3)
        })
        .collect::<Vec<_>>()
        .join(", ");
    run(&mut db, &format!("INSERT INTO events VALUES {values}"));

    let r = rows(run(
        &mut db,
        "SELECT id, score + 1 FROM events \
         WHERE category = 'even' AND id >= 120 AND id < 132 \
         ORDER BY id DESC",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(130), Value::Int(391)],
            vec![Value::Int(128), Value::Int(385)],
            vec![Value::Int(126), Value::Int(379)],
            vec![Value::Int(124), Value::Int(373)],
            vec![Value::Int(122), Value::Int(367)],
            vec![Value::Int(120), Value::Int(361)],
        ]
    );
}

#[test]
fn set_operations_union_intersect_and_except() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE a (x integer)");
    run(&mut db, "CREATE TABLE b (x integer)");
    run(&mut db, "INSERT INTO a VALUES (1), (2), (2), (3)");
    run(&mut db, "INSERT INTO b VALUES (2), (3), (4)");

    let r = rows(run(
        &mut db,
        "SELECT x FROM a UNION SELECT x FROM b ORDER BY x",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)],
            vec![Value::Int(4)],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT x FROM a UNION ALL SELECT x FROM b ORDER BY x LIMIT 3",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(2)]
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT x FROM a INTERSECT SELECT x FROM b ORDER BY x",
    ));
    assert_eq!(r, vec![vec![Value::Int(2)], vec![Value::Int(3)]]);

    let r = rows(run(
        &mut db,
        "SELECT x FROM a EXCEPT SELECT x FROM b ORDER BY x",
    ));
    assert_eq!(r, vec![vec![Value::Int(1)]]);
}

#[test]
fn create_query_replace_and_drop_view() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE users (id integer, name text)");
    run(&mut db, "INSERT INTO users VALUES (1, 'ada'), (2, 'linus')");

    match run(
        &mut db,
        "CREATE VIEW active_users AS SELECT id, name FROM users WHERE id >= 2",
    ) {
        ExecResult::Command(tag) => assert_eq!(tag, "CREATE VIEW"),
        other => panic!("expected CREATE VIEW command, got {}", tag_of(&other)),
    }

    let r = rows(run(&mut db, "SELECT name FROM active_users"));
    assert_eq!(r, vec![vec![Value::Text("linus".into())]]);

    run(&mut db, "INSERT INTO users VALUES (3, 'grace')");
    let r = rows(run(&mut db, "SELECT id FROM active_users ORDER BY id"));
    assert_eq!(r, vec![vec![Value::Int(2)], vec![Value::Int(3)]]);

    let r = rows(run(
        &mut db,
        "SELECT relname, relkind FROM pg_class WHERE relname = 'active_users'",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("active_users".into()),
            Value::Text("v".into())
        ]]
    );

    let r = rows(run(
        &mut db,
        "SELECT table_name, table_type FROM information_schema.tables \
         WHERE table_name = 'active_users'",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("active_users".into()),
            Value::Text("VIEW".into())
        ]]
    );

    match run(
        &mut db,
        "CREATE OR REPLACE VIEW active_users AS SELECT name FROM users WHERE id = 1",
    ) {
        ExecResult::Command(tag) => assert_eq!(tag, "CREATE VIEW"),
        other => panic!("expected CREATE VIEW command, got {}", tag_of(&other)),
    }
    let r = rows(run(&mut db, "SELECT name FROM active_users"));
    assert_eq!(r, vec![vec![Value::Text("ada".into())]]);

    match run(&mut db, "DROP VIEW active_users") {
        ExecResult::Command(tag) => assert_eq!(tag, "DROP VIEW"),
        other => panic!("expected DROP VIEW command, got {}", tag_of(&other)),
    }
}

#[test]
fn create_refresh_and_drop_materialized_view() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE events (id integer, kind text)");
    run(&mut db, "INSERT INTO events VALUES (1, 'a'), (2, 'b')");

    match run(
        &mut db,
        "CREATE MATERIALIZED VIEW event_counts AS \
         SELECT kind, count(*) AS n FROM events GROUP BY kind",
    ) {
        ExecResult::Command(tag) => assert_eq!(tag, "CREATE MATERIALIZED VIEW"),
        other => panic!(
            "expected CREATE MATERIALIZED VIEW command, got {}",
            tag_of(&other)
        ),
    }

    let r = rows(run(
        &mut db,
        "SELECT kind, n FROM event_counts ORDER BY kind",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("a".into()), Value::Int(1)],
            vec![Value::Text("b".into()), Value::Int(1)],
        ]
    );

    run(&mut db, "INSERT INTO events VALUES (3, 'a')");
    let r = rows(run(
        &mut db,
        "SELECT kind, n FROM event_counts ORDER BY kind",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("a".into()), Value::Int(1)],
            vec![Value::Text("b".into()), Value::Int(1)],
        ]
    );

    match run(&mut db, "REFRESH MATERIALIZED VIEW event_counts") {
        ExecResult::Command(tag) => assert_eq!(tag, "REFRESH MATERIALIZED VIEW"),
        other => panic!(
            "expected REFRESH MATERIALIZED VIEW command, got {}",
            tag_of(&other)
        ),
    }
    let r = rows(run(
        &mut db,
        "SELECT kind, n FROM event_counts ORDER BY kind",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("a".into()), Value::Int(2)],
            vec![Value::Text("b".into()), Value::Int(1)],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT relname, relkind FROM pg_class WHERE relname = 'event_counts'",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("event_counts".into()),
            Value::Text("m".into())
        ]]
    );

    let r = rows(run(
        &mut db,
        "SELECT table_name, table_type FROM information_schema.tables \
         WHERE table_name = 'event_counts'",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("event_counts".into()),
            Value::Text("MATERIALIZED VIEW".into())
        ]]
    );

    match run(&mut db, "DROP MATERIALIZED VIEW event_counts") {
        ExecResult::Command(tag) => assert_eq!(tag, "DROP MATERIALIZED VIEW"),
        other => panic!(
            "expected DROP MATERIALIZED VIEW command, got {}",
            tag_of(&other)
        ),
    }
}

#[test]
fn aggregates() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE s (v integer)");
    run(&mut db, "INSERT INTO s VALUES (10), (20), (30)");
    let r = rows(run(
        &mut db,
        "SELECT count(*), sum(v), min(v), max(v) FROM s",
    ));
    assert_eq!(
        r[0],
        vec![
            Value::Int(3),
            Value::Int(60),
            Value::Int(10),
            Value::Int(30),
        ]
    );

    let r = rows(run(&mut db, "SELECT avg(v) FROM s"));
    assert_eq!(r[0], vec![Value::Float(20.0)]);
}

#[test]
fn row_constructors_project_and_compare() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, name text)");
    run(&mut db, "INSERT INTO t VALUES (1, 'ada'), (2, 'linus')");

    let r = rows(run(&mut db, "SELECT ROW(id, name) FROM t ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("(1,ada)".into())],
            vec![Value::Text("(2,linus)".into())],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT name FROM t WHERE (id, name) = ROW(2, 'linus')",
    ));
    assert_eq!(r, vec![vec![Value::Text("linus".into())]]);

    let r = rows(run(&mut db, "SELECT ROW(1, NULL, 'a,b')"));
    assert_eq!(r, vec![vec![Value::Text("(1,,\"a,b\")".into())]]);
}

#[test]
fn array_constructors_project_and_compare() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, name text)");
    run(&mut db, "INSERT INTO t VALUES (1, 'ada'), (2, 'linus')");

    let r = rows(run(&mut db, "SELECT ARRAY[id, name] FROM t ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("{1,ada}".into())],
            vec![Value::Text("{2,linus}".into())],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT name FROM t WHERE ARRAY[id, name] = ARRAY[2, 'linus']",
    ));
    assert_eq!(r, vec![vec![Value::Text("linus".into())]]);

    let r = rows(run(&mut db, "SELECT ARRAY[1, NULL, 'a,b', '']"));
    assert_eq!(r, vec![vec![Value::Text("{1,NULL,\"a,b\",\"\"}".into())]]);
}

#[test]
fn array_operators_compare_constructed_arrays() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, tags text)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, 'red'), (2, 'blue'), (3, 'green')",
    );

    let r = rows(run(
        &mut db,
        "SELECT ARRAY[1, 2, 3] @> ARRAY[2, 3], \
         ARRAY[2, 3] <@ ARRAY[1, 2, 3], \
         ARRAY['red', 'blue'] && ARRAY['blue', 'yellow']",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(true),
        ]]
    );

    let r = rows(run(
        &mut db,
        "SELECT id FROM t WHERE ARRAY[tags, 'common'] && ARRAY['blue']",
    ));
    assert_eq!(r, vec![vec![Value::Int(2)]]);

    let r = rows(run(
        &mut db,
        "SELECT ARRAY[1, NULL] @> ARRAY[NULL], ARRAY[] @> ARRAY[]",
    ));
    assert_eq!(r, vec![vec![Value::Bool(true), Value::Bool(true)]]);
}

#[test]
fn array_functions_operate_on_constructed_arrays() {
    let mut db = Database::new();

    let r = rows(run(
        &mut db,
        "SELECT array_length(ARRAY[1, 2, 3], 1), \
         array_length(ARRAY[], 1), \
         cardinality(ARRAY['a', 'b', NULL]), \
         array_position(ARRAY['a', NULL, 'b'], NULL)",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Int(3),
            Value::Null,
            Value::Int(3),
            Value::Int(2),
        ]]
    );

    let r = rows(run(
        &mut db,
        "SELECT array_append(ARRAY['a', 'b'], 'c'), \
         array_prepend('z', ARRAY['a', 'b']), \
         array_cat(ARRAY['a'], ARRAY['b', 'c'])",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("{a,b,c}".into()),
            Value::Text("{z,a,b}".into()),
            Value::Text("{a,b,c}".into()),
        ]]
    );

    let r = rows(run(
        &mut db,
        "SELECT proname FROM pg_proc \
         WHERE proname IN ('array_length', 'cardinality', 'array_position', 'array_append', 'array_prepend', 'array_cat') \
         ORDER BY proname",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("array_append".into())],
            vec![Value::Text("array_cat".into())],
            vec![Value::Text("array_length".into())],
            vec![Value::Text("array_position".into())],
            vec![Value::Text("array_prepend".into())],
            vec![Value::Text("cardinality".into())],
        ]
    );
}

#[test]
fn update_and_delete() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, v integer)");
    run(&mut db, "INSERT INTO t VALUES (1, 100), (2, 200), (3, 300)");

    let r = run(&mut db, "UPDATE t SET v = v + 1 WHERE id = 2");
    assert!(matches!(r, ExecResult::Command(ref t) if t == "UPDATE 1"));
    let r = rows(run(&mut db, "SELECT v FROM t WHERE id = 2"));
    assert_eq!(r[0][0], Value::Int(201));

    let r = run(&mut db, "DELETE FROM t WHERE v > 150");
    assert!(matches!(r, ExecResult::Command(ref t) if t == "DELETE 2"));
    let r = rows(run(&mut db, "SELECT count(*) FROM t"));
    assert_eq!(r[0][0], Value::Int(1));
}

#[test]
fn update_from_and_delete_using_join_sources() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE inventory (id integer, qty integer)");
    run(
        &mut db,
        "CREATE TABLE adjustments (id integer, delta integer)",
    );
    run(
        &mut db,
        "INSERT INTO inventory VALUES (1, 10), (2, 20), (3, 30)",
    );
    run(&mut db, "INSERT INTO adjustments VALUES (1, 5), (3, -10)");

    match run(
        &mut db,
        "UPDATE inventory SET qty = inventory.qty + adjustments.delta \
         FROM adjustments WHERE inventory.id = adjustments.id",
    ) {
        ExecResult::Command(tag) => assert_eq!(tag, "UPDATE 2"),
        other => panic!("expected UPDATE command, got {}", tag_of(&other)),
    }

    let r = rows(run(&mut db, "SELECT id, qty FROM inventory ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Int(15)],
            vec![Value::Int(2), Value::Int(20)],
            vec![Value::Int(3), Value::Int(20)],
        ]
    );

    match run(
        &mut db,
        "DELETE FROM inventory USING adjustments \
         WHERE inventory.id = adjustments.id AND adjustments.delta < 0",
    ) {
        ExecResult::Command(tag) => assert_eq!(tag, "DELETE 1"),
        other => panic!("expected DELETE command, got {}", tag_of(&other)),
    }

    let r = rows(run(&mut db, "SELECT id FROM inventory ORDER BY id"));
    assert_eq!(r, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
}

#[test]
fn expressions_and_functions() {
    let mut db = Database::new();
    let r = rows(run(
        &mut db,
        "SELECT 1 + 2 * 3, upper('hi'), 'a' || 'b', 10 / 3, 10.0 / 4",
    ));
    assert_eq!(
        r[0],
        vec![
            Value::Int(7),
            Value::Text("HI".into()),
            Value::Text("ab".into()),
            Value::Int(3),
            Value::Float(2.5),
        ]
    );
}

#[test]
fn null_handling() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (a integer, b integer)");
    run(&mut db, "INSERT INTO t (a) VALUES (1)");
    let r = rows(run(&mut db, "SELECT a, b FROM t"));
    assert_eq!(r[0], vec![Value::Int(1), Value::Null]);

    let r = rows(run(&mut db, "SELECT a FROM t WHERE b IS NULL"));
    assert_eq!(r.len(), 1);
    let r = rows(run(&mut db, "SELECT a FROM t WHERE b IS NOT NULL"));
    assert_eq!(r.len(), 0);
}

#[test]
fn is_distinct_from_null_safe_comparison() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (a integer, b integer)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, 1), (1, 2), (NULL, NULL), (NULL, 1)",
    );

    let r = rows(run(
        &mut db,
        "SELECT count(*) FROM t WHERE a IS DISTINCT FROM b",
    ));
    assert_eq!(r, vec![vec![Value::Int(2)]]);

    let r = rows(run(
        &mut db,
        "SELECT count(*) FROM t WHERE a IS NOT DISTINCT FROM b",
    ));
    assert_eq!(r, vec![vec![Value::Int(2)]]);
}

#[test]
fn quantified_any_some_all_comparisons() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (x integer)");
    run(&mut db, "INSERT INTO t VALUES (1), (3), (NULL)");

    let r = rows(run(
        &mut db,
        "SELECT x FROM t WHERE x = ANY (1, 2) ORDER BY x",
    ));
    assert_eq!(r, vec![vec![Value::Int(1)]]);

    let r = rows(run(
        &mut db,
        "SELECT x FROM t WHERE x > SOME (2, 5) ORDER BY x",
    ));
    assert_eq!(r, vec![vec![Value::Int(3)]]);

    let r = rows(run(
        &mut db,
        "SELECT x FROM t WHERE x < ALL (2, 5) ORDER BY x",
    ));
    assert_eq!(r, vec![vec![Value::Int(1)]]);

    let r = rows(run(&mut db, "SELECT 7 = ANY (NULL, 7), 7 = ALL (7, NULL)"));
    assert_eq!(r[0], vec![Value::Bool(true), Value::Null]);
}

#[test]
fn not_null_violation_is_error() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer NOT NULL)");
    let stmts = Parser::parse_sql("INSERT INTO t (id) VALUES (NULL)").unwrap();
    let err = executor::execute(&mut db, stmts.into_iter().next().unwrap());
    assert!(err.is_err());
}

#[test]
fn limit_offset() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (x integer)");
    run(&mut db, "INSERT INTO t VALUES (1),(2),(3),(4),(5)");
    let r = rows(run(&mut db, "SELECT x FROM t ORDER BY x LIMIT 2 OFFSET 1"));
    let got: Vec<i64> = r
        .iter()
        .map(|row| match row[0] {
            Value::Int(i) => i,
            _ => panic!(),
        })
        .collect();
    assert_eq!(got, vec![2, 3]);
}

#[test]
fn row_locking_clauses_are_accepted_as_query_modifiers() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE jobs (id integer, status text)");
    run(
        &mut db,
        "INSERT INTO jobs VALUES (1, 'queued'), (2, 'running'), (3, 'queued')",
    );

    let r = rows(run(
        &mut db,
        "SELECT id FROM jobs WHERE status = 'queued' ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED",
    ));
    assert_eq!(r, vec![vec![Value::Int(1)]]);

    let r = rows(run(
        &mut db,
        "SELECT id FROM jobs ORDER BY id LIMIT 2 FOR SHARE OF jobs NOWAIT",
    ));
    assert_eq!(r, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);

    let r = rows(run(
        &mut db,
        "SELECT id FROM jobs FOR NO KEY UPDATE FOR KEY SHARE SKIP LOCKED",
    ));
    assert_eq!(r.len(), 3);
}

#[test]
fn cursor_declarations_and_fetch_materialize_select_results() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE events (id integer, name text)");
    run(
        &mut db,
        "INSERT INTO events VALUES (1, 'one'), (2, 'two'), (3, 'three')",
    );

    match run(
        &mut db,
        "DECLARE event_cursor CURSOR FOR SELECT id, name FROM events ORDER BY id",
    ) {
        ExecResult::Command(tag) => assert_eq!(tag, "DECLARE CURSOR"),
        other => panic!("expected DECLARE CURSOR command, got {}", tag_of(&other)),
    }

    let r = run(&mut db, "FETCH NEXT FROM event_cursor");
    match r {
        ExecResult::Rows { fields, rows, tag } => {
            assert_eq!(tag, "FETCH 1");
            assert_eq!(fields[0].name, "id");
            assert_eq!(fields[1].name, "name");
            assert_eq!(rows, vec![vec![Value::Int(1), Value::Text("one".into())]]);
        }
        other => panic!("expected FETCH rows, got {}", tag_of(&other)),
    }

    let r = rows(run(&mut db, "FETCH 2 FROM event_cursor"));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(2), Value::Text("two".into())],
            vec![Value::Int(3), Value::Text("three".into())],
        ]
    );

    let r = run(&mut db, "FETCH ALL FROM event_cursor");
    match r {
        ExecResult::Rows { rows, tag, .. } => {
            assert_eq!(tag, "FETCH 0");
            assert!(rows.is_empty());
        }
        other => panic!("expected FETCH rows, got {}", tag_of(&other)),
    }
}

#[test]
fn group_by_and_having() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE sales (region text, amount integer)");
    run(
        &mut db,
        "INSERT INTO sales VALUES ('w', 100), ('w', 200), ('e', 50), ('e', 25)",
    );

    // GROUP BY with ORDER BY on an aggregate alias.
    let r = rows(run(
        &mut db,
        "SELECT region, sum(amount) AS total FROM sales GROUP BY region ORDER BY total DESC",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("w".into()), Value::Int(300)],
            vec![Value::Text("e".into()), Value::Int(75)],
        ]
    );

    // HAVING filters out the 'e' group (sum 75 <= 100).
    let r = rows(run(
        &mut db,
        "SELECT region FROM sales GROUP BY region HAVING sum(amount) > 100",
    ));
    assert_eq!(r, vec![vec![Value::Text("w".into())]]);
}

#[test]
fn aggregate_over_empty_set() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (x integer)");
    // count over an empty set is 0 (one row); sum is NULL.
    let r = rows(run(&mut db, "SELECT count(*), sum(x) FROM t"));
    assert_eq!(r[0], vec![Value::Int(0), Value::Null]);
}

#[test]
fn order_by_output_alias() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (x integer)");
    run(&mut db, "INSERT INTO t VALUES (1), (2), (3)");
    let r = rows(run(&mut db, "SELECT x * 10 AS d FROM t ORDER BY d DESC"));
    let got: Vec<i64> = r
        .iter()
        .map(|row| match row[0] {
            Value::Int(i) => i,
            _ => panic!(),
        })
        .collect();
    assert_eq!(got, vec![30, 20, 10]);
}

#[test]
fn inner_and_left_join() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE users (id integer, name text)");
    run(
        &mut db,
        "CREATE TABLE orders (id integer, user_id integer, amount integer)",
    );
    run(
        &mut db,
        "INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')",
    );
    run(
        &mut db,
        "INSERT INTO orders VALUES (10, 1, 100), (11, 1, 50), (12, 2, 200)",
    );

    // INNER JOIN excludes Carol (no orders).
    let r = rows(run(
        &mut db,
        "SELECT u.name, o.amount FROM users u INNER JOIN orders o ON u.id = o.user_id ORDER BY o.amount",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("Alice".into()), Value::Int(50)],
            vec![Value::Text("Alice".into()), Value::Int(100)],
            vec![Value::Text("Bob".into()), Value::Int(200)],
        ]
    );

    // LEFT JOIN keeps Carol with a NULL amount.
    let r = rows(run(
        &mut db,
        "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.name = 'Carol'",
    ));
    assert_eq!(r, vec![vec![Value::Text("Carol".into()), Value::Null]]);

    // Aggregate over a LEFT JOIN: Carol has 0 orders, NULL sum.
    let r = rows(run(
        &mut db,
        "SELECT u.name, count(o.id) AS n, sum(o.amount) AS total FROM users u LEFT JOIN orders o ON u.id = o.user_id GROUP BY u.name ORDER BY u.name",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("Alice".into()), Value::Int(2), Value::Int(150)],
            vec![Value::Text("Bob".into()), Value::Int(1), Value::Int(200)],
            vec![Value::Text("Carol".into()), Value::Int(0), Value::Null],
        ]
    );
}

#[test]
fn select_distinct() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (region text, n integer)");
    run(
        &mut db,
        "INSERT INTO t VALUES ('w',1),('w',2),('e',3),('w',1)",
    );

    let r = rows(run(
        &mut db,
        "SELECT DISTINCT region FROM t ORDER BY region",
    ));
    assert_eq!(
        r,
        vec![vec![Value::Text("e".into())], vec![Value::Text("w".into())],]
    );

    // The duplicate ('w', 1) collapses to a single row.
    let r = rows(run(&mut db, "SELECT DISTINCT region, n FROM t"));
    assert_eq!(r.len(), 3);
}

#[test]
fn distinct_on_keeps_first_row_per_key_after_ordering() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE events (category text, name text, score integer)",
    );
    run(
        &mut db,
        "INSERT INTO events VALUES \
         ('a','old',10),('a','new',20),('b','low',5),('b','high',30)",
    );

    let r = rows(run(
        &mut db,
        "SELECT DISTINCT ON (category) category, name, score \
         FROM events ORDER BY category, score DESC",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("a".into()),
                Value::Text("new".into()),
                Value::Int(20),
            ],
            vec![
                Value::Text("b".into()),
                Value::Text("high".into()),
                Value::Int(30),
            ],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT DISTINCT ON (category) category AS c, name \
         FROM events ORDER BY c, name",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("a".into()), Value::Text("new".into())],
            vec![Value::Text("b".into()), Value::Text("high".into())],
        ]
    );
}

#[test]
fn like_in_between_case() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE p (id integer, name text, price integer)",
    );
    run(
        &mut db,
        "INSERT INTO p VALUES (1,'Apple',100),(2,'Apricot',150),(3,'Banana',80),(4,'Cherry',200)",
    );

    let names = |r: Vec<Vec<Value>>| -> Vec<String> {
        r.into_iter()
            .map(|row| match &row[0] {
                Value::Text(s) => s.clone(),
                _ => panic!(),
            })
            .collect()
    };

    assert_eq!(
        names(rows(run(
            &mut db,
            "SELECT name FROM p WHERE name LIKE 'Ap%' ORDER BY name"
        ))),
        vec!["Apple", "Apricot"]
    );
    assert_eq!(
        names(rows(run(
            &mut db,
            "SELECT name FROM p WHERE name ILIKE 'b%'"
        ))),
        vec!["Banana"]
    );
    assert_eq!(
        names(rows(run(
            &mut db,
            "SELECT name FROM p WHERE name LIKE '_pple'"
        ))),
        vec!["Apple"]
    );
    assert_eq!(
        names(rows(run(
            &mut db,
            "SELECT name FROM p WHERE price BETWEEN 90 AND 160 ORDER BY id"
        ))),
        vec!["Apple", "Apricot"]
    );
    assert_eq!(
        names(rows(run(
            &mut db,
            "SELECT name FROM p WHERE id IN (1, 3) ORDER BY id"
        ))),
        vec!["Apple", "Banana"]
    );
    assert_eq!(
        names(rows(run(
            &mut db,
            "SELECT name FROM p WHERE id NOT IN (1, 3) ORDER BY id"
        ))),
        vec!["Apricot", "Cherry"]
    );

    // Searched CASE.
    let r = rows(run(
        &mut db,
        "SELECT CASE WHEN price >= 150 THEN 'hi' WHEN price >= 90 THEN 'mid' ELSE 'lo' END FROM p ORDER BY id",
    ));
    assert_eq!(names(r), vec!["mid", "hi", "lo", "hi"]);

    // Simple CASE.
    let r = rows(run(
        &mut db,
        "SELECT CASE id WHEN 1 THEN 'one' ELSE 'other' END FROM p ORDER BY id",
    ));
    assert_eq!(names(r), vec!["one", "other", "other", "other"]);
}

#[test]
fn right_full_cross_joins() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE a (id integer, label text)");
    run(&mut db, "CREATE TABLE b (id integer, note text)");
    run(&mut db, "INSERT INTO a VALUES (1,'a1'),(2,'a2'),(3,'a3')");
    run(&mut db, "INSERT INTO b VALUES (2,'b2'),(3,'b3'),(4,'b4')");

    // RIGHT JOIN keeps unmatched b4 with a NULL left side.
    let r = rows(run(
        &mut db,
        "SELECT a.label, b.note FROM a RIGHT JOIN b ON a.id = b.id ORDER BY b.id",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("a2".into()), Value::Text("b2".into())],
            vec![Value::Text("a3".into()), Value::Text("b3".into())],
            vec![Value::Null, Value::Text("b4".into())],
        ]
    );

    // FULL JOIN keeps unmatched rows from both sides.
    let r = rows(run(
        &mut db,
        "SELECT count(*) FROM a FULL JOIN b ON a.id = b.id",
    ));
    assert_eq!(r[0][0], Value::Int(4));

    // CROSS JOIN is the cartesian product.
    let r = rows(run(&mut db, "SELECT count(*) FROM a CROSS JOIN b"));
    assert_eq!(r[0][0], Value::Int(9));
}

#[test]
fn returning_clause() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, name text, v integer)");

    let r = rows(run(
        &mut db,
        "INSERT INTO t VALUES (1,'a',10),(2,'b',20) RETURNING id, name",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Text("a".into())],
            vec![Value::Int(2), Value::Text("b".into())],
        ]
    );

    let r = rows(run(
        &mut db,
        "UPDATE t SET v = v * 10 WHERE id = 1 RETURNING v",
    ));
    assert_eq!(r, vec![vec![Value::Int(100)]]);

    let r = rows(run(&mut db, "DELETE FROM t WHERE id = 2 RETURNING *"));
    assert_eq!(
        r,
        vec![vec![Value::Int(2), Value::Text("b".into()), Value::Int(20)]]
    );
}

#[test]
fn default_column_values() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t (id integer, name text DEFAULT 'anon', score integer DEFAULT 0)",
    );
    run(&mut db, "INSERT INTO t (id) VALUES (1)");
    run(&mut db, "INSERT INTO t (id, score) VALUES (2, 99)");
    run(&mut db, "INSERT INTO t DEFAULT VALUES");
    let r = rows(run(&mut db, "SELECT id, name, score FROM t ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Text("anon".into()), Value::Int(0)],
            vec![Value::Int(2), Value::Text("anon".into()), Value::Int(99)],
            vec![Value::Null, Value::Text("anon".into()), Value::Int(0)],
        ]
    );
}

#[test]
fn generated_columns_are_stored_and_recomputed() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE metrics (raw int, doubled int GENERATED ALWAYS AS (raw * 2) STORED)",
    );
    run(&mut db, "INSERT INTO metrics (raw) VALUES (5), (7)");

    let r = rows(run(
        &mut db,
        "SELECT raw, doubled FROM metrics ORDER BY raw",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(5), Value::Int(10)],
            vec![Value::Int(7), Value::Int(14)],
        ]
    );

    run(&mut db, "UPDATE metrics SET raw = 6 WHERE raw = 5");
    let r = rows(run(
        &mut db,
        "SELECT raw, doubled FROM metrics ORDER BY raw",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(6), Value::Int(12)],
            vec![Value::Int(7), Value::Int(14)],
        ]
    );

    let err = Parser::parse_sql("INSERT INTO metrics (raw, doubled) VALUES (8, 99)")
        .and_then(|mut stmts| executor::execute(&mut db, stmts.remove(0)).map(|_| ()))
        .expect_err("explicit generated column value should fail");
    assert_eq!(
        err,
        "cannot insert a non-DEFAULT value into column \"doubled\" because it is a generated column"
    );

    let r = rows(run(
        &mut db,
        "SELECT attgenerated FROM pg_attribute WHERE attname = 'doubled'",
    ));
    assert_eq!(r, vec![vec![Value::Text("s".into())]]);
    let r = rows(run(
        &mut db,
        "SELECT adbin FROM pg_attrdef WHERE adbin = '(raw * 2)'",
    ));
    assert_eq!(r, vec![vec![Value::Text("(raw * 2)".into())]]);

    run(
        &mut db,
        "ALTER TABLE metrics ADD COLUMN plus_one int GENERATED ALWAYS AS (raw + 1) STORED",
    );
    let r = rows(run(
        &mut db,
        "SELECT raw, plus_one FROM metrics ORDER BY raw",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(6), Value::Int(7)],
            vec![Value::Int(7), Value::Int(8)],
        ]
    );
}

#[test]
fn identity_columns_use_sequences_and_catalog_flags() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE identities (id int GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, name text)",
    );
    run(&mut db, "INSERT INTO identities (name) VALUES ('a'), ('b')");
    run(
        &mut db,
        "INSERT INTO identities (id, name) VALUES (10, 'explicit')",
    );
    run(&mut db, "INSERT INTO identities (name) VALUES ('after')");

    let r = rows(run(&mut db, "SELECT id, name FROM identities ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Text("a".into())],
            vec![Value::Int(2), Value::Text("b".into())],
            vec![Value::Int(10), Value::Text("explicit".into())],
            vec![Value::Int(11), Value::Text("after".into())],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT attidentity FROM pg_attribute WHERE attname = 'id'",
    ));
    assert_eq!(r, vec![vec![Value::Text("d".into())]]);
}

#[test]
fn overriding_system_value_allows_generated_always_identity_inserts() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE always_ids (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name text)",
    );
    run(&mut db, "INSERT INTO always_ids (name) VALUES ('auto')");

    let err = Parser::parse_sql("INSERT INTO always_ids (id, name) VALUES (10, 'blocked')")
        .and_then(|mut stmts| executor::execute(&mut db, stmts.remove(0)).map(|_| ()))
        .expect_err("explicit GENERATED ALWAYS identity value should fail");
    assert_eq!(
        err,
        "cannot insert a non-DEFAULT value into column \"id\" because it is an identity column defined as GENERATED ALWAYS"
    );

    run(
        &mut db,
        "INSERT INTO always_ids (id, name) OVERRIDING SYSTEM VALUE VALUES (10, 'manual')",
    );
    run(&mut db, "INSERT INTO always_ids (name) VALUES ('after')");

    let r = rows(run(&mut db, "SELECT id, name FROM always_ids ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Text("auto".into())],
            vec![Value::Int(10), Value::Text("manual".into())],
            vec![Value::Int(11), Value::Text("after".into())],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT attidentity FROM pg_attribute WHERE attname = 'id'",
    ));
    assert_eq!(r, vec![vec![Value::Text("a".into())]]);
}

#[test]
fn insert_select() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE src (id integer, name text)");
    run(
        &mut db,
        "CREATE TABLE dst (id serial PRIMARY KEY, name text DEFAULT 'anon')",
    );
    run(&mut db, "INSERT INTO src VALUES (10, 'a'), (20, 'b')");

    let r = run(
        &mut db,
        "INSERT INTO dst (id, name) SELECT id, upper(name) FROM src ORDER BY id RETURNING id, name",
    );
    assert!(
        matches!(r, ExecResult::Rows { ref rows, ref tag, .. } if tag == "INSERT 0 2" && rows.len() == 2)
    );

    let r = rows(run(&mut db, "SELECT id, name FROM dst ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(10), Value::Text("A".into())],
            vec![Value::Int(20), Value::Text("B".into())],
        ]
    );

    run(
        &mut db,
        "INSERT INTO dst (name) SELECT name FROM src WHERE id = 10",
    );
    let r = rows(run(&mut db, "SELECT id, name FROM dst WHERE name = 'a'"));
    assert_eq!(r, vec![vec![Value::Int(21), Value::Text("a".into())]]);
}

#[test]
fn serial_auto_increment() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE u (id serial PRIMARY KEY, name text)");
    let r = rows(run(
        &mut db,
        "INSERT INTO u DEFAULT VALUES RETURNING id, name",
    ));
    assert_eq!(r, vec![vec![Value::Int(1), Value::Null]]);
    run(&mut db, "INSERT INTO u (name) VALUES ('a'), ('b')");
    // Explicit value advances the sequence past it.
    run(&mut db, "INSERT INTO u (id, name) VALUES (50, 'm')");
    run(&mut db, "INSERT INTO u (name) VALUES ('c')");

    let r = rows(run(&mut db, "SELECT id, name FROM u ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Null],
            vec![Value::Int(2), Value::Text("a".into())],
            vec![Value::Int(3), Value::Text("b".into())],
            vec![Value::Int(50), Value::Text("m".into())],
            vec![Value::Int(51), Value::Text("c".into())],
        ]
    );
}

#[test]
fn casts_and_functions() {
    let mut db = Database::new();
    let r = rows(run(
        &mut db,
        "SELECT '42'::integer + 8, CAST(3.7 AS integer), 100::text || '!'",
    ));
    assert_eq!(
        r[0],
        vec![Value::Int(50), Value::Int(4), Value::Text("100!".into())]
    );

    let r = rows(run(
        &mut db,
        "SELECT round(3.14159, 2), greatest(3,7,2), least(3,7,2), nullif(5,5)",
    ));
    assert_eq!(
        r[0],
        vec![
            Value::Float(3.14),
            Value::Int(7),
            Value::Int(2),
            Value::Null
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT trim('  hi  '), substr('postgres',1,4), replace('a-b-c','-','_')",
    ));
    assert_eq!(
        r[0],
        vec![
            Value::Text("hi".into()),
            Value::Text("post".into()),
            Value::Text("a_b_c".into()),
        ]
    );
}

#[test]
fn math_functions() {
    let mut db = Database::new();

    // Float input → float output (matching PostgreSQL's double-precision path),
    // while integer input to sign/abs stays integral.
    let r = rows(run(
        &mut db,
        "SELECT ceil(4.2), floor(4.8), sign(-3), abs(-7), trunc(3.78, 1)",
    ));
    assert_eq!(
        r[0],
        vec![
            Value::Float(5.0),
            Value::Float(4.0),
            Value::Int(-1),
            Value::Int(7),
            Value::Float(3.7),
        ]
    );

    let r = rows(run(&mut db, "SELECT sqrt(16.0), power(2, 10), log(100.0)"));
    assert_eq!(
        r[0],
        vec![Value::Float(4.0), Value::Float(1024.0), Value::Float(2.0)]
    );

    let r = rows(run(&mut db, "SELECT mod(17, 5), div(17, 5), gcd(12, 18), lcm(4, 6)"));
    assert_eq!(
        r[0],
        vec![Value::Int(2), Value::Int(3), Value::Int(6), Value::Int(12)]
    );

    // log(base, x) and trig sanity.
    let r = rows(run(&mut db, "SELECT log(2.0, 8.0), round(degrees(pi()))"));
    assert_eq!(r[0][0], Value::Float(3.0));
    assert_eq!(r[0][1], Value::Float(180.0));

    // NULL passthrough.
    let r = rows(run(&mut db, "SELECT sqrt(NULL), mod(NULL, 2)"));
    assert_eq!(r[0], vec![Value::Null, Value::Null]);
}

#[test]
fn string_functions() {
    let mut db = Database::new();

    let r = rows(run(
        &mut db,
        "SELECT lpad('7', 3, '0'), rpad('ab', 5, '.'), left('postgres', 4), right('postgres', 3)",
    ));
    assert_eq!(
        r[0],
        vec![
            Value::Text("007".into()),
            Value::Text("ab...".into()),
            Value::Text("post".into()),
            Value::Text("res".into()),
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT repeat('ab', 3), reverse('abc'), initcap('hello world'), ascii('A'), chr(66)",
    ));
    assert_eq!(
        r[0],
        vec![
            Value::Text("ababab".into()),
            Value::Text("cba".into()),
            Value::Text("Hello World".into()),
            Value::Int(65),
            Value::Text("B".into()),
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT strpos('postgres', 'gr'), split_part('a,b,c', ',', 2), split_part('a,b,c', ',', -1)",
    ));
    assert_eq!(
        r[0],
        vec![
            Value::Int(5),
            Value::Text("b".into()),
            Value::Text("c".into()),
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT starts_with('postgres', 'post'), concat_ws('-', 'a', NULL, 'c'), translate('hello', 'el', 'ip'), to_hex(255)",
    ));
    assert_eq!(
        r[0],
        vec![
            Value::Bool(true),
            Value::Text("a-c".into()),
            Value::Text("hippo".into()),
            Value::Text("ff".into()),
        ]
    );
}

#[test]
fn information_schema_introspection() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE users (id serial PRIMARY KEY, name text NOT NULL, email text)",
    );
    run(&mut db, "CREATE TABLE orders (id integer)");

    let r = rows(run(
        &mut db,
        "SELECT table_name FROM information_schema.tables ORDER BY table_name",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("orders".into())],
            vec![Value::Text("users".into())],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT column_name, is_nullable FROM information_schema.columns WHERE table_name = 'users' ORDER BY ordinal_position",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("id".into()), Value::Text("NO".into())],
            vec![Value::Text("name".into()), Value::Text("NO".into())],
            vec![Value::Text("email".into()), Value::Text("YES".into())],
        ]
    );
}

#[test]
fn temporary_and_unlogged_tables_expose_persistence_metadata() {
    let mut db = Database::new();

    match run(&mut db, "CREATE TEMPORARY TABLE scratch (id integer)") {
        ExecResult::Command(tag) => assert_eq!(tag, "CREATE TABLE"),
        other => panic!("expected CREATE TABLE command, got {}", tag_of(&other)),
    }
    match run(&mut db, "CREATE UNLOGGED TABLE cache (id integer)") {
        ExecResult::Command(tag) => assert_eq!(tag, "CREATE TABLE"),
        other => panic!("expected CREATE TABLE command, got {}", tag_of(&other)),
    }

    let r = rows(run(
        &mut db,
        "SELECT relname, relpersistence FROM pg_class \
         WHERE relname IN ('cache', 'scratch') ORDER BY relname",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("cache".into()), Value::Text("u".into())],
            vec![Value::Text("scratch".into()), Value::Text("t".into())],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT table_name, table_type FROM information_schema.tables \
         WHERE table_name IN ('cache', 'scratch') ORDER BY table_name",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("cache".into()),
                Value::Text("BASE TABLE".into())
            ],
            vec![
                Value::Text("scratch".into()),
                Value::Text("LOCAL TEMPORARY".into())
            ],
        ]
    );
}

#[test]
fn pg_catalog_and_regex_and_positional() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE alpha (id integer)");
    run(&mut db, "CREATE TABLE beta (id integer)");

    // The shape of psql's \dt query against pg_catalog.
    let r = rows(run(
        &mut db,
        "SELECT n.nspname, c.relname FROM pg_catalog.pg_class c \
         LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r','p','') AND n.nspname <> 'pg_catalog' \
         AND n.nspname !~ '^pg_toast' AND n.nspname <> 'information_schema' \
         AND pg_catalog.pg_table_is_visible(c.oid) ORDER BY 1, 2",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("public".into()), Value::Text("alpha".into())],
            vec![Value::Text("public".into()), Value::Text("beta".into())],
        ]
    );

    // Regex operators directly.
    let r = rows(run(
        &mut db,
        "SELECT 'public' !~ '^pg_toast', 'pg_toast_x' ~ '^pg_toast', 'ABC' ~* '^abc'",
    ));
    assert_eq!(
        r[0],
        vec![Value::Bool(true), Value::Bool(true), Value::Bool(true)]
    );
}

#[test]
fn pg_type_catalog_lists_builtin_types() {
    let mut db = Database::new();

    let r = rows(run(
        &mut db,
        "SELECT typname, oid, typlen, typcategory FROM pg_catalog.pg_type \
         WHERE typname IN ('bool', 'int4', 'money', 'text') ORDER BY typname",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("bool".into()),
                Value::Int(16),
                Value::Int(1),
                Value::Text("B".into()),
            ],
            vec![
                Value::Text("int4".into()),
                Value::Int(23),
                Value::Int(4),
                Value::Text("N".into()),
            ],
            vec![
                Value::Text("money".into()),
                Value::Int(790),
                Value::Int(8),
                Value::Text("N".into()),
            ],
            vec![
                Value::Text("text".into()),
                Value::Int(25),
                Value::Int(-1),
                Value::Text("S".into()),
            ],
        ]
    );

    let r = rows(run(&mut db, "SELECT typname FROM pg_type WHERE oid = 1700"));
    assert_eq!(r, vec![vec![Value::Text("numeric".into())]]);
}

#[test]
fn pg_attribute_catalog_lists_table_columns() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE users (id serial PRIMARY KEY, email text NOT NULL, balance money)",
    );

    let r = rows(run(
        &mut db,
        "SELECT a.attname, t.typname, a.attnum, a.attnotnull \
         FROM pg_catalog.pg_attribute a \
         JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
         JOIN pg_catalog.pg_type t ON t.oid = a.atttypid \
         WHERE c.relname = 'users' AND NOT a.attisdropped \
         ORDER BY a.attnum",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("id".into()),
                Value::Text("int4".into()),
                Value::Int(1),
                Value::Bool(true),
            ],
            vec![
                Value::Text("email".into()),
                Value::Text("text".into()),
                Value::Int(2),
                Value::Bool(true),
            ],
            vec![
                Value::Text("balance".into()),
                Value::Text("money".into()),
                Value::Int(3),
                Value::Bool(false),
            ],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT attname FROM pg_attribute WHERE attrelid = 16384 ORDER BY attnum",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("id".into())],
            vec![Value::Text("email".into())],
            vec![Value::Text("balance".into())],
        ]
    );
}

#[test]
fn pg_index_catalog_lists_table_indexes() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE users (id serial PRIMARY KEY, email text, balance money)",
    );
    run(
        &mut db,
        "CREATE UNIQUE INDEX users_email_idx ON users (email)",
    );

    let r = rows(run(
        &mut db,
        "SELECT ic.relname, tc.relname, i.indisunique, i.indisprimary, i.indkey \
         FROM pg_catalog.pg_index i \
         JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid \
         JOIN pg_catalog.pg_class tc ON tc.oid = i.indrelid \
         WHERE tc.relname = 'users' ORDER BY ic.relname",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("users_email_idx".into()),
                Value::Text("users".into()),
                Value::Bool(true),
                Value::Bool(false),
                Value::Text("2".into()),
            ],
            vec![
                Value::Text("users_id_pkey".into()),
                Value::Text("users".into()),
                Value::Bool(true),
                Value::Bool(true),
                Value::Text("1".into()),
            ],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT relname, relkind FROM pg_class WHERE relkind = 'i' ORDER BY relname",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("users_email_idx".into()),
                Value::Text("i".into())
            ],
            vec![Value::Text("users_id_pkey".into()), Value::Text("i".into())],
        ]
    );
}

#[test]
fn pg_constraint_catalog_lists_primary_and_unique_constraints() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE users (id serial PRIMARY KEY, email text, balance money)",
    );
    run(
        &mut db,
        "CREATE UNIQUE INDEX users_email_idx ON users (email)",
    );
    run(&mut db, "CREATE INDEX users_balance_idx ON users (balance)");
    run(
        &mut db,
        "ALTER TABLE users ADD CONSTRAINT users_balance_key UNIQUE (balance)",
    );

    let r = rows(run(
        &mut db,
        "SELECT conname, contype, conkey, c.relname, i.relname \
         FROM pg_catalog.pg_constraint co \
         JOIN pg_catalog.pg_class c ON c.oid = co.conrelid \
         JOIN pg_catalog.pg_class i ON i.oid = co.conindid \
         WHERE c.relname = 'users' ORDER BY conname",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("users_balance_key".into()),
                Value::Text("u".into()),
                Value::Text("3".into()),
                Value::Text("users".into()),
                Value::Text("users_balance_key".into()),
            ],
            vec![
                Value::Text("users_email_idx".into()),
                Value::Text("u".into()),
                Value::Text("2".into()),
                Value::Text("users".into()),
                Value::Text("users_email_idx".into()),
            ],
            vec![
                Value::Text("users_id_pkey".into()),
                Value::Text("p".into()),
                Value::Text("1".into()),
                Value::Text("users".into()),
                Value::Text("users_id_pkey".into()),
            ],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT convalidated FROM pg_constraint WHERE conname = 'users_id_pkey'",
    ));
    assert_eq!(r, vec![vec![Value::Bool(true)]]);

    run(
        &mut db,
        "ALTER TABLE users DROP CONSTRAINT users_balance_key",
    );
    let r = rows(run(
        &mut db,
        "SELECT conname FROM pg_constraint WHERE conname = 'users_balance_key'",
    ));
    assert!(r.is_empty());

    run(&mut db, "CREATE TABLE dupes (email text)");
    run(
        &mut db,
        "INSERT INTO dupes VALUES ('a@example.com'), ('a@example.com')",
    );
    let err = Parser::parse_sql("ALTER TABLE dupes ADD CONSTRAINT dupes_email_key UNIQUE (email)")
        .and_then(|mut stmts| executor::execute(&mut db, stmts.remove(0)).map(|_| ()))
        .expect_err("duplicate existing rows should fail");
    assert_eq!(
        err,
        "could not create unique constraint \"dupes_email_key\": key contains duplicate values"
    );

    run(
        &mut db,
        "CREATE TABLE accounts (balance int, CONSTRAINT accounts_balance_nonnegative CHECK (balance >= 0))",
    );
    let err = Parser::parse_sql("INSERT INTO accounts VALUES (-1)")
        .and_then(|mut stmts| executor::execute(&mut db, stmts.remove(0)).map(|_| ()))
        .expect_err("check violation should fail");
    assert_eq!(
        err,
        "new row for relation \"accounts\" violates check constraint \"accounts_balance_nonnegative\""
    );

    run(&mut db, "CREATE TABLE legacy_scores (score int)");
    run(&mut db, "INSERT INTO legacy_scores VALUES (-10)");
    run(
        &mut db,
        "ALTER TABLE legacy_scores ADD CONSTRAINT legacy_score_positive CHECK (score > 0) NOT VALID",
    );
    let r = rows(run(
        &mut db,
        "SELECT contype, convalidated FROM pg_constraint WHERE conname = 'legacy_score_positive'",
    ));
    assert_eq!(r, vec![vec![Value::Text("c".into()), Value::Bool(false)]]);

    let err = Parser::parse_sql("INSERT INTO legacy_scores VALUES (-1)")
        .and_then(|mut stmts| executor::execute(&mut db, stmts.remove(0)).map(|_| ()))
        .expect_err("not valid check still applies to new rows");
    assert_eq!(
        err,
        "new row for relation \"legacy_scores\" violates check constraint \"legacy_score_positive\""
    );
}

#[test]
fn foreign_key_constraints_are_enforced_and_listed() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE users (id int PRIMARY KEY, name text)",
    );
    run(
        &mut db,
        "CREATE TABLE orders (id int PRIMARY KEY, user_id int, CONSTRAINT orders_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id))",
    );
    run(&mut db, "INSERT INTO users VALUES (1, 'Ada')");
    run(&mut db, "INSERT INTO orders VALUES (10, 1)");

    let r = rows(run(
        &mut db,
        "SELECT contype, conkey, convalidated FROM pg_constraint WHERE conname = 'orders_user_id_fkey'",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("f".into()),
            Value::Text("2".into()),
            Value::Bool(true),
        ]]
    );

    let err = Parser::parse_sql("INSERT INTO orders VALUES (11, 999)")
        .and_then(|mut stmts| executor::execute(&mut db, stmts.remove(0)).map(|_| ()))
        .expect_err("missing parent row should fail");
    assert_eq!(
        err,
        "insert or update on table \"orders\" violates foreign key constraint \"orders_user_id_fkey\""
    );

    let err = Parser::parse_sql("UPDATE users SET id = 2 WHERE id = 1")
        .and_then(|mut stmts| executor::execute(&mut db, stmts.remove(0)).map(|_| ()))
        .expect_err("referenced parent update should fail");
    assert_eq!(
        err,
        "update or delete on table \"users\" violates foreign key constraint \"orders_user_id_fkey\" on table \"orders\""
    );

    let err = Parser::parse_sql("DELETE FROM users WHERE id = 1")
        .and_then(|mut stmts| executor::execute(&mut db, stmts.remove(0)).map(|_| ()))
        .expect_err("referenced parent delete should fail");
    assert_eq!(
        err,
        "update or delete on table \"users\" violates foreign key constraint \"orders_user_id_fkey\" on table \"orders\""
    );

    let err = Parser::parse_sql("DROP TABLE users")
        .and_then(|mut stmts| executor::execute(&mut db, stmts.remove(0)).map(|_| ()))
        .expect_err("referenced parent drop should fail");
    assert_eq!(
        err,
        "cannot drop table \"users\" because other objects depend on it: constraint \"orders_user_id_fkey\" on table \"orders\""
    );

    run(
        &mut db,
        "ALTER TABLE orders DROP CONSTRAINT orders_user_id_fkey",
    );
    run(&mut db, "DELETE FROM users WHERE id = 1");
    run(&mut db, "DROP TABLE users");
}

#[test]
fn not_valid_foreign_key_skips_existing_rows_but_checks_new_rows() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE events (id int PRIMARY KEY)");
    run(&mut db, "CREATE TABLE event_logs (event_id int)");
    run(&mut db, "INSERT INTO event_logs VALUES (999)");
    run(
        &mut db,
        "ALTER TABLE event_logs ADD CONSTRAINT event_logs_event_id_fkey FOREIGN KEY (event_id) REFERENCES events(id) NOT VALID",
    );

    let r = rows(run(
        &mut db,
        "SELECT contype, convalidated FROM pg_constraint WHERE conname = 'event_logs_event_id_fkey'",
    ));
    assert_eq!(r, vec![vec![Value::Text("f".into()), Value::Bool(false)]]);

    let err = Parser::parse_sql("INSERT INTO event_logs VALUES (1000)")
        .and_then(|mut stmts| executor::execute(&mut db, stmts.remove(0)).map(|_| ()))
        .expect_err("new rows are still checked");
    assert_eq!(
        err,
        "insert or update on table \"event_logs\" violates foreign key constraint \"event_logs_event_id_fkey\""
    );

    run(&mut db, "INSERT INTO events VALUES (1000)");
    run(&mut db, "INSERT INTO event_logs VALUES (1000)");
}

#[test]
fn pg_attrdef_catalog_lists_column_defaults() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE users (id serial PRIMARY KEY, name text DEFAULT 'anon', score integer DEFAULT 0)",
    );

    let r = rows(run(
        &mut db,
        "SELECT a.attname, pg_get_expr(d.adbin, d.adrelid) \
         FROM pg_catalog.pg_attrdef d \
         JOIN pg_catalog.pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
         JOIN pg_catalog.pg_class c ON c.oid = d.adrelid \
         WHERE c.relname = 'users' ORDER BY a.attnum",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("name".into()), Value::Text("'anon'".into())],
            vec![Value::Text("score".into()), Value::Text("0".into())],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT adnum, adbin FROM pg_attrdef WHERE adrelid = 16384 ORDER BY adnum",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(2), Value::Text("'anon'".into())],
            vec![Value::Int(3), Value::Text("0".into())],
        ]
    );
}

#[test]
fn pg_description_and_depend_catalogs_are_queryable() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE users (id serial PRIMARY KEY)");

    let r = rows(run(
        &mut db,
        "SELECT description FROM pg_catalog.pg_description WHERE objoid = 16384",
    ));
    assert!(r.is_empty());

    let r = rows(run(
        &mut db,
        "SELECT deptype FROM pg_depend WHERE objid = 16384",
    ));
    assert!(r.is_empty());

    let r = rows(run(
        &mut db,
        "SELECT d.description, dep.deptype \
         FROM pg_catalog.pg_class c \
         LEFT JOIN pg_catalog.pg_description d ON d.objoid = c.oid \
         LEFT JOIN pg_catalog.pg_depend dep ON dep.objid = c.oid \
         WHERE c.relname = 'users'",
    ));
    assert_eq!(r, vec![vec![Value::Null, Value::Null]]);

    run(&mut db, "COMMENT ON TABLE users IS 'application users'");
    run(&mut db, "COMMENT ON COLUMN users.id IS 'synthetic key'");

    let r = rows(run(
        &mut db,
        "SELECT objoid, objsubid, description FROM pg_catalog.pg_description WHERE objoid = 16384 ORDER BY objsubid",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Int(16384),
                Value::Int(0),
                Value::Text("application users".into())
            ],
            vec![
                Value::Int(16384),
                Value::Int(1),
                Value::Text("synthetic key".into())
            ],
        ]
    );

    run(&mut db, "COMMENT ON COLUMN users.id IS NULL");
    let r = rows(run(
        &mut db,
        "SELECT objsubid, description FROM pg_description WHERE objoid = 16384",
    ));
    assert_eq!(
        r,
        vec![vec![Value::Int(0), Value::Text("application users".into())]]
    );
}

#[test]
fn role_database_and_settings_catalogs_are_queryable() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE ROLE app_reader WITH LOGIN CREATEDB CONNECTION LIMIT 3 PASSWORD 'secret' VALID UNTIL '2030-01-01'",
    );
    run(
        &mut db,
        "CREATE USER deploy WITH SUPERUSER REPLICATION BYPASSRLS",
    );
    run(&mut db, "ALTER ROLE app_reader WITH NOCREATEDB");

    let r = rows(run(
        &mut db,
        "SELECT rolname, rolsuper, rolcreatedb, rolcanlogin, rolconnlimit FROM pg_catalog.pg_roles ORDER BY oid",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("postgres".into()),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
                Value::Int(-1),
            ],
            vec![
                Value::Text("app_reader".into()),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true),
                Value::Int(3),
            ],
            vec![
                Value::Text("deploy".into()),
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(true),
                Value::Int(-1),
            ],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT usename, usesuper, userepl, usebypassrls FROM pg_user ORDER BY usesysid",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("postgres".into()),
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(true),
            ],
            vec![
                Value::Text("app_reader".into()),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
            ],
            vec![
                Value::Text("deploy".into()),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
            ],
        ]
    );

    run(&mut db, "DROP USER deploy");
    let r = rows(run(
        &mut db,
        "SELECT rolname FROM pg_roles WHERE rolname = 'deploy'",
    ));
    assert!(r.is_empty());

    let r = rows(run(
        &mut db,
        "SELECT datname, pg_get_userbyid(datdba), encoding FROM pg_database",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("postgres".into()),
            Value::Text("postgres".into()),
            Value::Int(6),
        ]]
    );

    let r = rows(run(
        &mut db,
        "SELECT setting FROM pg_settings WHERE name = 'server_encoding'",
    ));
    assert_eq!(r, vec![vec![Value::Text("UTF8".into())]]);
}

#[test]
fn function_operator_and_extension_catalogs_are_queryable() {
    let mut db = Database::new();

    let r = rows(run(
        &mut db,
        "SELECT proname, prokind, t.typname \
         FROM pg_catalog.pg_proc p \
         JOIN pg_catalog.pg_type t ON t.oid = p.prorettype \
         WHERE proname IN ('count', 'upper', 'pg_get_expr') ORDER BY proname",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("count".into()),
                Value::Text("a".into()),
                Value::Text("int8".into()),
            ],
            vec![
                Value::Text("pg_get_expr".into()),
                Value::Text("f".into()),
                Value::Text("text".into()),
            ],
            vec![
                Value::Text("upper".into()),
                Value::Text("f".into()),
                Value::Text("text".into()),
            ],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT oprname, oprcanhash, t.typname \
         FROM pg_operator o \
         JOIN pg_type t ON t.oid = o.oprresult \
         WHERE oprname IN ('=', '||') ORDER BY oprname",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("=".into()),
                Value::Bool(true),
                Value::Text("bool".into()),
            ],
            vec![
                Value::Text("||".into()),
                Value::Bool(false),
                Value::Text("text".into()),
            ],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT extname, extversion FROM pg_extension ORDER BY extname",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("plpgsql".into()),
            Value::Text("1.0".into())
        ]]
    );
}

#[test]
fn advisory_lock_functions_update_pg_locks() {
    let mut db = Database::new();

    let r = rows(run(&mut db, "SELECT pg_try_advisory_lock(42)"));
    assert_eq!(r, vec![vec![Value::Bool(true)]]);

    let r = rows(run(
        &mut db,
        "SELECT locktype, classid, objid, mode, granted FROM pg_locks",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("advisory".into()),
            Value::Int(0),
            Value::Int(42),
            Value::Text("ExclusiveLock".into()),
            Value::Bool(true),
        ]]
    );

    let r = rows(run(
        &mut db,
        "SELECT pg_advisory_lock(7, 9), pg_advisory_unlock(42)",
    ));
    assert_eq!(r, vec![vec![Value::Null, Value::Bool(true)]]);

    let r = rows(run(
        &mut db,
        "SELECT classid, objid FROM pg_locks ORDER BY classid, objid",
    ));
    assert_eq!(r, vec![vec![Value::Int(7), Value::Int(9)]]);

    let r = rows(run(&mut db, "SELECT pg_advisory_unlock(42)"));
    assert_eq!(r, vec![vec![Value::Bool(false)]]);

    run(&mut db, "SELECT pg_advisory_unlock_all()");
    let r = rows(run(&mut db, "SELECT count(*) FROM pg_locks"));
    assert_eq!(r, vec![vec![Value::Int(0)]]);

    let r = rows(run(
        &mut db,
        "SELECT proname FROM pg_proc WHERE proname LIKE 'pg_advisory_%' ORDER BY proname",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("pg_advisory_lock".into())],
            vec![Value::Text("pg_advisory_unlock".into())],
            vec![Value::Text("pg_advisory_unlock_all".into())],
        ]
    );
}

#[test]
fn create_and_drop_extension_updates_pg_extension() {
    let mut db = Database::new();

    let r = rows(run(
        &mut db,
        "SELECT extname, extversion FROM pg_extension ORDER BY extname",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("plpgsql".into()),
            Value::Text("1.0".into())
        ]]
    );

    match run(
        &mut db,
        "CREATE EXTENSION IF NOT EXISTS hstore WITH VERSION '1.8'",
    ) {
        ExecResult::Command(tag) => assert_eq!(tag, "CREATE EXTENSION"),
        other => panic!("expected CREATE EXTENSION command, got {}", tag_of(&other)),
    }
    match run(
        &mut db,
        "CREATE EXTENSION IF NOT EXISTS hstore WITH VERSION '1.8'",
    ) {
        ExecResult::Command(tag) => assert_eq!(tag, "CREATE EXTENSION"),
        other => panic!("expected CREATE EXTENSION command, got {}", tag_of(&other)),
    }

    let r = rows(run(
        &mut db,
        "SELECT extname, extversion FROM pg_extension ORDER BY extname",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("hstore".into()), Value::Text("1.8".into())],
            vec![Value::Text("plpgsql".into()), Value::Text("1.0".into())],
        ]
    );

    match run(&mut db, "DROP EXTENSION hstore") {
        ExecResult::Command(tag) => assert_eq!(tag, "DROP EXTENSION"),
        other => panic!("expected DROP EXTENSION command, got {}", tag_of(&other)),
    }
    match run(&mut db, "DROP EXTENSION IF EXISTS missing") {
        ExecResult::Command(tag) => assert_eq!(tag, "DROP EXTENSION"),
        other => panic!("expected DROP EXTENSION command, got {}", tag_of(&other)),
    }

    let r = rows(run(
        &mut db,
        "SELECT extname FROM pg_extension ORDER BY extname",
    ));
    assert_eq!(r, vec![vec![Value::Text("plpgsql".into())]]);
}

#[test]
fn create_drop_schema_and_search_path_update_catalogs() {
    let mut db = Database::new();

    match run(&mut db, "CREATE SCHEMA IF NOT EXISTS app") {
        ExecResult::Command(tag) => assert_eq!(tag, "CREATE SCHEMA"),
        other => panic!("expected CREATE SCHEMA command, got {}", tag_of(&other)),
    }
    match run(&mut db, "SET search_path TO app, public") {
        ExecResult::Command(tag) => assert_eq!(tag, "SET"),
        other => panic!("expected SET command, got {}", tag_of(&other)),
    }

    let r = rows(run(
        &mut db,
        "SELECT nspname FROM pg_namespace ORDER BY nspname",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("app".into())],
            vec![Value::Text("information_schema".into())],
            vec![Value::Text("pg_catalog".into())],
            vec![Value::Text("public".into())],
        ]
    );

    let r = rows(run(&mut db, "SHOW search_path"));
    assert_eq!(r, vec![vec![Value::Text("app, public".into())]]);

    let r = rows(run(
        &mut db,
        "SELECT setting FROM pg_settings WHERE name = 'search_path'",
    ));
    assert_eq!(r, vec![vec![Value::Text("app, public".into())]]);

    match run(&mut db, "DROP SCHEMA app") {
        ExecResult::Command(tag) => assert_eq!(tag, "DROP SCHEMA"),
        other => panic!("expected DROP SCHEMA command, got {}", tag_of(&other)),
    }
    match run(&mut db, "DROP SCHEMA IF EXISTS missing") {
        ExecResult::Command(tag) => assert_eq!(tag, "DROP SCHEMA"),
        other => panic!("expected DROP SCHEMA command, got {}", tag_of(&other)),
    }

    let r = rows(run(
        &mut db,
        "SELECT nspname FROM pg_namespace WHERE nspname = 'app'",
    ));
    assert_eq!(r, Vec::<Vec<Value>>::new());

    let r = rows(run(&mut db, "SHOW current_schema"));
    assert_eq!(r, vec![vec![Value::Text("public".into())]]);
}

#[test]
fn create_and_drop_database_update_pg_database() {
    let mut db = Database::new();

    match run(&mut db, "CREATE DATABASE appdb WITH ENCODING 'UTF8'") {
        ExecResult::Command(tag) => assert_eq!(tag, "CREATE DATABASE"),
        other => panic!("expected CREATE DATABASE command, got {}", tag_of(&other)),
    }

    let r = rows(run(
        &mut db,
        "SELECT datname FROM pg_database ORDER BY datname",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("appdb".into())],
            vec![Value::Text("postgres".into())],
        ]
    );

    match run(&mut db, "ALTER DATABASE appdb WITH CONNECTION LIMIT 7") {
        ExecResult::Command(tag) => assert_eq!(tag, "ALTER DATABASE"),
        other => panic!("expected ALTER DATABASE command, got {}", tag_of(&other)),
    }
    match run(&mut db, "ALTER DATABASE appdb RENAME TO renameddb") {
        ExecResult::Command(tag) => assert_eq!(tag, "ALTER DATABASE"),
        other => panic!("expected ALTER DATABASE command, got {}", tag_of(&other)),
    }

    let r = rows(run(
        &mut db,
        "SELECT datname, datconnlimit FROM pg_database ORDER BY datname",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("postgres".into()), Value::Int(-1)],
            vec![Value::Text("renameddb".into()), Value::Int(7)],
        ]
    );

    match run(&mut db, "DROP DATABASE renameddb WITH (FORCE)") {
        ExecResult::Command(tag) => assert_eq!(tag, "DROP DATABASE"),
        other => panic!("expected DROP DATABASE command, got {}", tag_of(&other)),
    }
    match run(&mut db, "DROP DATABASE IF EXISTS missing") {
        ExecResult::Command(tag) => assert_eq!(tag, "DROP DATABASE"),
        other => panic!("expected DROP DATABASE command, got {}", tag_of(&other)),
    }

    let r = rows(run(
        &mut db,
        "SELECT datname FROM pg_database ORDER BY datname",
    ));
    assert_eq!(r, vec![vec![Value::Text("postgres".into())]]);
}

#[test]
fn create_and_drop_tablespace_update_pg_tablespace() {
    let mut db = Database::new();

    match run(
        &mut db,
        "CREATE TABLESPACE fastspace LOCATION '/tmp/postgres-rs-fast'",
    ) {
        ExecResult::Command(tag) => assert_eq!(tag, "CREATE TABLESPACE"),
        other => panic!("expected CREATE TABLESPACE command, got {}", tag_of(&other)),
    }

    let r = rows(run(
        &mut db,
        "SELECT spcname, spclocation FROM pg_tablespace ORDER BY oid",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("pg_default".into()), Value::Text("".into())],
            vec![Value::Text("pg_global".into()), Value::Text("".into())],
            vec![
                Value::Text("fastspace".into()),
                Value::Text("/tmp/postgres-rs-fast".into()),
            ],
        ]
    );

    match run(&mut db, "DROP TABLESPACE fastspace") {
        ExecResult::Command(tag) => assert_eq!(tag, "DROP TABLESPACE"),
        other => panic!("expected DROP TABLESPACE command, got {}", tag_of(&other)),
    }
    match run(&mut db, "DROP TABLESPACE IF EXISTS missing_space") {
        ExecResult::Command(tag) => assert_eq!(tag, "DROP TABLESPACE"),
        other => panic!("expected DROP TABLESPACE command, got {}", tag_of(&other)),
    }

    let r = rows(run(
        &mut db,
        "SELECT spcname FROM pg_tablespace ORDER BY oid",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("pg_default".into())],
            vec![Value::Text("pg_global".into())],
        ]
    );
}

#[test]
fn create_and_drop_collation_update_pg_collation() {
    let mut db = Database::new();

    match run(&mut db, "CREATE COLLATION da_dk (LOCALE = 'da_DK.UTF-8')") {
        ExecResult::Command(tag) => assert_eq!(tag, "CREATE COLLATION"),
        other => panic!("expected CREATE COLLATION command, got {}", tag_of(&other)),
    }
    match run(
        &mut db,
        "CREATE COLLATION IF NOT EXISTS da_dk (LOCALE = 'ignored')",
    ) {
        ExecResult::Command(tag) => assert_eq!(tag, "CREATE COLLATION"),
        other => panic!("expected CREATE COLLATION command, got {}", tag_of(&other)),
    }

    let r = rows(run(
        &mut db,
        "SELECT collname, collcollate, collctype FROM pg_collation \
         WHERE collname IN ('default', 'C', 'POSIX', 'da_dk') ORDER BY oid",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("default".into()),
                Value::Text("C".into()),
                Value::Text("C".into()),
            ],
            vec![
                Value::Text("C".into()),
                Value::Text("C".into()),
                Value::Text("C".into()),
            ],
            vec![
                Value::Text("POSIX".into()),
                Value::Text("POSIX".into()),
                Value::Text("POSIX".into()),
            ],
            vec![
                Value::Text("da_dk".into()),
                Value::Text("da_DK.UTF-8".into()),
                Value::Text("da_DK.UTF-8".into()),
            ],
        ]
    );

    run(&mut db, "CREATE TABLE words (name text COLLATE da_dk)");
    run(&mut db, "INSERT INTO words VALUES ('a')");
    let r = rows(run(
        &mut db,
        "SELECT name COLLATE da_dk FROM words WHERE name COLLATE da_dk = 'a'",
    ));
    assert_eq!(r, vec![vec![Value::Text("a".into())]]);

    match run(&mut db, "DROP COLLATION da_dk") {
        ExecResult::Command(tag) => assert_eq!(tag, "DROP COLLATION"),
        other => panic!("expected DROP COLLATION command, got {}", tag_of(&other)),
    }
    match run(&mut db, "DROP COLLATION IF EXISTS missing_collation") {
        ExecResult::Command(tag) => assert_eq!(tag, "DROP COLLATION"),
        other => panic!("expected DROP COLLATION command, got {}", tag_of(&other)),
    }

    let r = rows(run(
        &mut db,
        "SELECT collname FROM pg_collation WHERE collname = 'da_dk'",
    ));
    assert!(r.is_empty());
}

#[test]
fn generate_series_returns_integer_series() {
    let mut db = Database::new();

    let r = rows(run(&mut db, "SELECT generate_series(1, 4)"));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)],
            vec![Value::Int(4)],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT generate_series FROM generate_series(5, 1, -2) gs ORDER BY generate_series",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(3)],
            vec![Value::Int(5)],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT proname, proretset FROM pg_proc WHERE proname = 'generate_series'",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("generate_series".into()),
            Value::Bool(true)
        ]]
    );
}

#[test]
fn explain_returns_query_plan_rows() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, name text)");
    run(&mut db, "INSERT INTO t VALUES (1, 'a'), (2, 'b')");

    let r = rows(run(
        &mut db,
        "EXPLAIN SELECT name FROM t WHERE id = 1 ORDER BY name LIMIT 1",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("Seq Scan on t (rows=1)".into())],
            vec![Value::Text("  Filter".into())],
            vec![Value::Text("  Sort".into())],
            vec![Value::Text("  Limit".into())],
        ]
    );

    let r = rows(run(
        &mut db,
        "EXPLAIN SELECT generate_series FROM generate_series(1, 2)",
    ));
    assert_eq!(
        r,
        vec![vec![Value::Text("Function Scan on generate_series".into())]]
    );
}

#[test]
fn explain_analyze_executes_and_reports_observed_rows() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer)");
    run(&mut db, "INSERT INTO t VALUES (1), (2), (3)");

    let r = rows(run(
        &mut db,
        "EXPLAIN ANALYZE SELECT id FROM t WHERE id > 1 ORDER BY id",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("Seq Scan on t (rows=1) (actual rows=2)".into())],
            vec![Value::Text("  Filter".into())],
            vec![Value::Text("  Sort".into())],
        ]
    );
}

/// Collect an EXPLAIN result's QUERY PLAN column into one newline-joined string.
fn plan_text(res: ExecResult) -> String {
    rows(res)
        .into_iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.clone(),
            other => panic!("expected text plan line, got {other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn analyze_populates_planner_statistics() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, grp integer)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, 10), (2, 10), (3, 20), (4, 20), (5, 20)",
    );

    // Before ANALYZE, pg_class.reltuples falls back to the live row count.
    let r = rows(run(
        &mut db,
        "SELECT reltuples FROM pg_class WHERE relname = 't'",
    ));
    assert_eq!(r, vec![vec![Value::Float(5.0)]]);

    run(&mut db, "ANALYZE t");

    // reltuples reflects the analyzed estimate.
    let r = rows(run(
        &mut db,
        "SELECT reltuples FROM pg_class WHERE relname = 't'",
    ));
    assert_eq!(r, vec![vec![Value::Float(5.0)]]);

    // The collected ndistinct drives selectivity: grp has 2 distinct values, so
    // an equality on grp is estimated at 5/2 ≈ 3 rows (rounded, clamped).
    let p = plan_text(run(&mut db, "EXPLAIN SELECT * FROM t WHERE grp = 10"));
    assert!(p.contains("(rows=3)"), "plan was:\n{p}");
}

#[test]
fn explain_chooses_index_scan_when_selective() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, v integer)");
    let values: String = (0..100)
        .map(|i| format!("({i}, {})", i % 4))
        .collect::<Vec<_>>()
        .join(", ");
    run(&mut db, &format!("INSERT INTO t VALUES {values}"));
    run(&mut db, "CREATE INDEX t_id_idx ON t (id)");
    run(&mut db, "CREATE INDEX t_v_idx ON t (v)");
    run(&mut db, "ANALYZE t");

    // Equality on the unique-ish `id` (ndistinct=100) is highly selective: the
    // planner should pick the index and estimate ~1 row.
    let p = plan_text(run(&mut db, "EXPLAIN SELECT * FROM t WHERE id = 42"));
    assert!(
        p.contains("Index Scan using t_id_idx") && p.contains("(rows=1)"),
        "plan was:\n{p}"
    );

    // Equality on `v` (ndistinct=4) matches ~25 rows; an index still beats a
    // 100-row seq scan, so an index scan is chosen with a ~25-row estimate.
    let p = plan_text(run(&mut db, "EXPLAIN SELECT * FROM t WHERE v = 1"));
    assert!(p.contains("Index Scan using t_v_idx"), "plan was:\n{p}");
    assert!(p.contains("(rows=25)"), "plan was:\n{p}");

    // No predicate → a plain seq scan over all rows.
    let p = plan_text(run(&mut db, "EXPLAIN SELECT * FROM t"));
    assert!(p.contains("Seq Scan on t (rows=100)"), "plan was:\n{p}");
}

#[test]
fn explain_three_table_join_drives_from_smallest() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE big (id integer, m integer)");
    run(&mut db, "CREATE TABLE mid (id integer, b integer)");
    run(&mut db, "CREATE TABLE small (id integer)");
    // big: 50 rows, mid: 10 rows, small: 2 rows.
    let big: String = (0..50)
        .map(|i| format!("({i}, {})", i % 10))
        .collect::<Vec<_>>()
        .join(", ");
    run(&mut db, &format!("INSERT INTO big VALUES {big}"));
    let mid: String = (0..10)
        .map(|i| format!("({i}, {i})"))
        .collect::<Vec<_>>()
        .join(", ");
    run(&mut db, &format!("INSERT INTO mid VALUES {mid}"));
    run(&mut db, "INSERT INTO small VALUES (0), (1)");
    run(&mut db, "ANALYZE");

    // Written big -> mid -> small, but the planner should drive from `small`.
    let p = plan_text(run(
        &mut db,
        "EXPLAIN SELECT * FROM big \
         JOIN mid ON big.m = mid.id \
         JOIN small ON mid.b = small.id",
    ));
    let lines: Vec<&str> = p.lines().collect();
    // First scan node (the drive relation) must be `small`.
    let first_scan = lines
        .iter()
        .find(|l| l.contains("-> "))
        .expect("a scan node");
    assert!(
        first_scan.contains("on small"),
        "expected smallest table to drive, plan was:\n{p}"
    );
}

#[test]
fn join_reordering_preserves_results() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE a (id integer, b_id integer)");
    run(&mut db, "CREATE TABLE b (id integer, c_id integer)");
    run(&mut db, "CREATE TABLE c (id integer, label text)");
    run(
        &mut db,
        "INSERT INTO a VALUES (1, 10), (2, 10), (3, 20), (4, 30)",
    );
    run(&mut db, "INSERT INTO b VALUES (10, 100), (20, 200), (30, 300)");
    run(
        &mut db,
        "INSERT INTO c VALUES (100, 'x'), (200, 'y'), (300, 'z')",
    );

    let query = "SELECT a.id, c.label FROM a \
         JOIN b ON a.b_id = b.id \
         JOIN c ON b.c_id = c.id \
         ORDER BY a.id";

    // Result without statistics (no reordering applied).
    let before = rows(run(&mut db, query));

    // Same query after ANALYZE populates stats and enables reordering.
    run(&mut db, "ANALYZE");
    let after = rows(run(&mut db, query));

    assert_eq!(before, after);
    assert_eq!(
        after,
        vec![
            vec![Value::Int(1), Value::Text("x".into())],
            vec![Value::Int(2), Value::Text("x".into())],
            vec![Value::Int(3), Value::Text("y".into())],
            vec![Value::Int(4), Value::Text("z".into())],
        ]
    );
}

#[test]
fn analyze_accepts_database_and_table_targets() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer)");

    match run(&mut db, "ANALYZE") {
        ExecResult::Command(tag) => assert_eq!(tag, "ANALYZE"),
        other => panic!("expected ANALYZE command, got {}", tag_of(&other)),
    }

    match run(&mut db, "ANALYZE VERBOSE t") {
        ExecResult::Command(tag) => assert_eq!(tag, "ANALYZE"),
        other => panic!("expected ANALYZE command, got {}", tag_of(&other)),
    }

    let err = Parser::parse_sql("ANALYZE missing")
        .and_then(|mut stmts| executor::execute(&mut db, stmts.remove(0)).map(|_| ()))
        .expect_err("missing table should fail");
    assert_eq!(err, "relation \"missing\" does not exist");
}

#[test]
fn administration_commands_parse_and_acknowledge() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer PRIMARY KEY)");

    for (sql, expected) in [
        ("VACUUM", "VACUUM"),
        ("VACUUM (VERBOSE, ANALYZE) t", "VACUUM"),
        ("REINDEX TABLE t", "REINDEX"),
        ("REINDEX INDEX t_id_pkey", "REINDEX"),
        ("REINDEX DATABASE postgres", "REINDEX"),
        ("REINDEX SYSTEM postgres", "REINDEX"),
        ("CLUSTER", "CLUSTER"),
        ("CLUSTER t USING t_id_pkey", "CLUSTER"),
        ("CHECKPOINT", "CHECKPOINT"),
        ("DISCARD PLANS", "DISCARD PLANS"),
        ("DISCARD ALL", "DISCARD ALL"),
        ("LISTEN changes", "LISTEN"),
        ("NOTIFY changes, 'payload'", "NOTIFY"),
        ("UNLISTEN changes", "UNLISTEN"),
        ("UNLISTEN *", "UNLISTEN"),
        ("LOCK TABLE t IN ACCESS SHARE MODE", "LOCK TABLE"),
    ] {
        match run(&mut db, sql) {
            ExecResult::Command(tag) => assert_eq!(tag, expected, "{sql}"),
            other => panic!("expected command for {sql}, got {}", tag_of(&other)),
        }
    }

    let err = Parser::parse_sql("VACUUM missing")
        .and_then(|mut stmts| executor::execute(&mut db, stmts.remove(0)).map(|_| ()))
        .expect_err("missing table should fail");
    assert_eq!(err, "relation \"missing\" does not exist");

    let err = Parser::parse_sql("CLUSTER t USING missing_idx")
        .and_then(|mut stmts| executor::execute(&mut db, stmts.remove(0)).map(|_| ()))
        .expect_err("missing index should fail");
    assert_eq!(err, "index \"missing_idx\" does not exist");

    let err = Parser::parse_sql("LOCK TABLE missing")
        .and_then(|mut stmts| executor::execute(&mut db, stmts.remove(0)).map(|_| ()))
        .expect_err("missing table should fail");
    assert_eq!(err, "relation \"missing\" does not exist");
}

#[test]
fn vacuum_compacts_storage_metadata_for_sql_tables() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE docs (id integer, body text)");
    run(
        &mut db,
        "INSERT INTO docs VALUES (1, 'aaaa'), (2, 'bbbb'), (3, 'cccc')",
    );
    run(&mut db, "UPDATE docs SET body = 'changed' WHERE id = 2");
    run(&mut db, "DELETE FROM docs WHERE id = 1");

    let dirty = db.table("docs").expect("table exists").storage_stats();
    assert_eq!(dirty.live_rows, 2);
    assert_eq!(dirty.dead_rows, 2);

    match run(&mut db, "VACUUM docs") {
        ExecResult::Command(tag) => assert_eq!(tag, "VACUUM"),
        other => panic!("expected VACUUM command, got {}", tag_of(&other)),
    }

    let compacted = db.table("docs").expect("table exists").storage_stats();
    assert_eq!(compacted.live_rows, 2);
    assert_eq!(compacted.dead_rows, 0);
    assert_eq!(compacted.vacuum_count, dirty.vacuum_count + 1);
}

#[test]
fn alter_system_and_security_labels_update_catalogs() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer)");

    match run(&mut db, "ALTER SYSTEM SET work_mem = '8MB'") {
        ExecResult::Command(tag) => assert_eq!(tag, "ALTER SYSTEM"),
        other => panic!("expected ALTER SYSTEM command, got {}", tag_of(&other)),
    }
    let r = rows(run(
        &mut db,
        "SELECT setting FROM pg_settings WHERE name = 'work_mem'",
    ));
    assert_eq!(r, vec![vec![Value::Text("8MB".into())]]);

    run(&mut db, "ALTER SYSTEM RESET work_mem");
    let r = rows(run(
        &mut db,
        "SELECT setting FROM pg_settings WHERE name = 'work_mem'",
    ));
    assert!(r.is_empty());

    match run(
        &mut db,
        "SECURITY LABEL FOR selinux ON TABLE t IS 'system_u:object_r:postgresql_db_t:s0'",
    ) {
        ExecResult::Command(tag) => assert_eq!(tag, "SECURITY LABEL"),
        other => panic!("expected SECURITY LABEL command, got {}", tag_of(&other)),
    }
    let r = rows(run(
        &mut db,
        "SELECT provider, objsubid, label FROM pg_seclabel WHERE objoid = 16384",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("selinux".into()),
            Value::Int(0),
            Value::Text("system_u:object_r:postgresql_db_t:s0".into()),
        ]]
    );

    run(&mut db, "SECURITY LABEL FOR selinux ON TABLE t IS NULL");
    let r = rows(run(&mut db, "SELECT label FROM pg_seclabel"));
    assert!(r.is_empty());
}

#[test]
fn dollar_quoted_strings_parse_as_string_literals() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE notes (body text)");
    run(
        &mut db,
        "INSERT INTO notes VALUES ($$plain ' quote$$), ($tag$semi; colon and $nested$ text$tag$)",
    );

    let r = rows(run(&mut db, "SELECT body FROM notes ORDER BY body"));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("plain ' quote".into())],
            vec![Value::Text("semi; colon and $nested$ text".into())],
        ]
    );

    let r = rows(run(&mut db, "SELECT $q$line\nbreak$q$"));
    assert_eq!(r, vec![vec![Value::Text("line\nbreak".into())]]);
}

#[test]
fn extended_types() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE e (id serial, amount numeric(10,2), created timestamp, day date, tags jsonb)",
    );
    run(
        &mut db,
        "INSERT INTO e (amount, created, day, tags) VALUES (19.99, '2024-03-15 10:30:00', '2024-03-15', '{\"k\":1}'), (5.50, '2023-12-01 08:00:00', '2023-12-01', '{\"k\":2}')",
    );

    // Timestamp comparison via ISO text ordering.
    let r = rows(run(
        &mut db,
        "SELECT day FROM e WHERE created > '2024-01-01' ORDER BY day",
    ));
    assert_eq!(r, vec![vec![Value::Text("2024-03-15".into())]]);

    // numeric arithmetic.
    let r = rows(run(&mut db, "SELECT amount * 2 FROM e ORDER BY amount"));
    assert_eq!(r, vec![vec![Value::Float(11.0)], vec![Value::Float(39.98)]]);

    // Casts to the new types round-trip the text.
    let r = rows(run(
        &mut db,
        "SELECT '550e8400-e29b-41d4-a716-446655440000'::uuid",
    ));
    assert_eq!(
        r[0][0],
        Value::Text("550e8400-e29b-41d4-a716-446655440000".into())
    );
}

#[test]
fn json_extraction_operators() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE docs (id int, doc jsonb)");
    run(
        &mut db,
        "INSERT INTO docs VALUES (1, '{\"name\":\"Ada\",\"age\":37,\"active\":true,\"nested\":{\"score\":9},\"tags\":[\"rust\",\"sql\"],\"missing\":null}'), (2, '{\"name\":\"Linus\",\"tags\":[\"c\"]}')",
    );

    let r = rows(run(
        &mut db,
        "SELECT doc ->> 'name', doc -> 'nested', doc -> 'tags' ->> 1 FROM docs WHERE id = 1",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("Ada".into()),
            Value::Text("{\"score\":9}".into()),
            Value::Text("sql".into()),
        ]]
    );

    let r = rows(run(
        &mut db,
        "SELECT id FROM docs WHERE doc ->> 'name' = 'Ada'",
    ));
    assert_eq!(r, vec![vec![Value::Int(1)]]);

    let r = rows(run(
        &mut db,
        "SELECT doc ->> 'missing', doc -> 'tags' ->> 9 FROM docs WHERE id = 1",
    ));
    assert_eq!(r, vec![vec![Value::Null, Value::Null]]);
}

#[test]
fn json_functions_extract_type_and_array_length() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE docs (doc jsonb)");
    run(
        &mut db,
        "INSERT INTO docs VALUES ('{\"name\":\"Ada\",\"profile\":{\"score\":9},\"tags\":[\"rust\",\"sql\"],\"active\":true}')",
    );

    let r = rows(run(
        &mut db,
        "SELECT jsonb_typeof(doc), \
         jsonb_typeof(doc -> 'tags'), \
         jsonb_array_length(doc -> 'tags'), \
         jsonb_extract_path_text(doc, 'profile', 'score') \
         FROM docs",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("object".into()),
            Value::Text("array".into()),
            Value::Int(2),
            Value::Text("9".into()),
        ]]
    );

    let r = rows(run(
        &mut db,
        "SELECT json_typeof('true'::json), json_array_length('[1,{\"x\":2},3]'::json)",
    ));
    assert_eq!(r, vec![vec![Value::Text("boolean".into()), Value::Int(3)]]);

    let r = rows(run(
        &mut db,
        "SELECT proname FROM pg_proc \
         WHERE proname IN ('json_typeof', 'jsonb_typeof', 'json_array_length', 'jsonb_array_length', 'json_extract_path_text', 'jsonb_extract_path_text') \
         ORDER BY proname",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("json_array_length".into())],
            vec![Value::Text("json_extract_path_text".into())],
            vec![Value::Text("json_typeof".into())],
            vec![Value::Text("jsonb_array_length".into())],
            vec![Value::Text("jsonb_extract_path_text".into())],
            vec![Value::Text("jsonb_typeof".into())],
        ]
    );
}

#[test]
fn char_types_are_text_backed() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE c (a char(3), b character(5), c bpchar)",
    );
    run(&mut db, "INSERT INTO c VALUES ('xy', 'hello', 'z')");

    let r = rows(run(&mut db, "SELECT a, b, c FROM c"));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("xy".into()),
            Value::Text("hello".into()),
            Value::Text("z".into()),
        ]]
    );
}

#[test]
fn bytea_is_text_backed_with_bytea_type() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE b (payload bytea)");
    run(&mut db, "INSERT INTO b VALUES ('\\xDEADBEEF')");

    let r = rows(run(&mut db, "SELECT payload FROM b"));
    assert_eq!(r, vec![vec![Value::Text("\\xDEADBEEF".into())]]);

    let r = rows(run(
        &mut db,
        "SELECT data_type FROM information_schema.columns WHERE table_name = 'b'",
    ));
    assert_eq!(r, vec![vec![Value::Text("bytea".into())]]);
}

#[test]
fn interval_is_text_backed_with_interval_type() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE durations (span interval)");
    run(&mut db, "INSERT INTO durations VALUES ('2 days 03:04:05')");

    let r = rows(run(&mut db, "SELECT span FROM durations"));
    assert_eq!(r, vec![vec![Value::Text("2 days 03:04:05".into())]]);

    let r = rows(run(
        &mut db,
        "SELECT data_type FROM information_schema.columns WHERE table_name = 'durations'",
    ));
    assert_eq!(r, vec![vec![Value::Text("interval".into())]]);
}

#[test]
fn timetz_is_text_backed_with_timetz_type() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE schedule (starts_at timetz, ends_at time with time zone)",
    );
    run(
        &mut db,
        "INSERT INTO schedule VALUES ('08:30:00+02', '17:00:00Z')",
    );

    let r = rows(run(&mut db, "SELECT starts_at, ends_at FROM schedule"));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("08:30:00+02".into()),
            Value::Text("17:00:00Z".into()),
        ]]
    );

    let r = rows(run(
        &mut db,
        "SELECT column_name, data_type FROM information_schema.columns WHERE table_name = 'schedule' ORDER BY ordinal_position",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("starts_at".into()),
                Value::Text("time with time zone".into()),
            ],
            vec![
                Value::Text("ends_at".into()),
                Value::Text("time with time zone".into()),
            ],
        ]
    );
}

#[test]
fn money_is_numeric_backed_with_money_type() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE invoices (amount money)");
    run(&mut db, "INSERT INTO invoices VALUES (12.50), ('7.25')");

    let r = rows(run(&mut db, "SELECT amount FROM invoices ORDER BY amount"));
    assert_eq!(r, vec![vec![Value::Float(7.25)], vec![Value::Float(12.5)]]);

    let r = rows(run(
        &mut db,
        "SELECT data_type FROM information_schema.columns WHERE table_name = 'invoices'",
    ));
    assert_eq!(r, vec![vec![Value::Text("money".into())]]);
}

#[test]
fn network_address_types_are_text_backed_with_catalog_metadata() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE net (host inet, subnet cidr, mac macaddr, mac8 macaddr8)",
    );
    run(
        &mut db,
        "INSERT INTO net VALUES ('192.168.1.1', '192.168.0.0/24', '08:00:2b:01:02:03', '08:00:2b:ff:fe:01:02:03')",
    );

    let r = rows(run(&mut db, "SELECT host, subnet, mac, mac8 FROM net"));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("192.168.1.1".into()),
            Value::Text("192.168.0.0/24".into()),
            Value::Text("08:00:2b:01:02:03".into()),
            Value::Text("08:00:2b:ff:fe:01:02:03".into()),
        ]]
    );

    let r = rows(run(
        &mut db,
        "SELECT column_name, data_type FROM information_schema.columns \
         WHERE table_name = 'net' ORDER BY ordinal_position",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("host".into()), Value::Text("inet".into())],
            vec![Value::Text("subnet".into()), Value::Text("cidr".into())],
            vec![Value::Text("mac".into()), Value::Text("macaddr".into())],
            vec![Value::Text("mac8".into()), Value::Text("macaddr8".into())],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT typname, oid, typcategory FROM pg_type \
         WHERE typname IN ('inet', 'cidr', 'macaddr', 'macaddr8') ORDER BY typname",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("cidr".into()),
                Value::Int(650),
                Value::Text("I".into()),
            ],
            vec![
                Value::Text("inet".into()),
                Value::Int(869),
                Value::Text("I".into()),
            ],
            vec![
                Value::Text("macaddr".into()),
                Value::Int(829),
                Value::Text("I".into()),
            ],
            vec![
                Value::Text("macaddr8".into()),
                Value::Int(774),
                Value::Text("I".into()),
            ],
        ]
    );
}

#[test]
fn network_operators_compare_ipv4_inet_and_cidr_values() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE nets (host inet, subnet cidr)");
    run(
        &mut db,
        "INSERT INTO nets VALUES ('192.168.1.10', '192.168.1.0/24'), ('10.0.0.5', '10.0.0.0/8')",
    );

    let r = rows(run(
        &mut db,
        "SELECT host << subnet, host <<= subnet, subnet >> host, subnet >>= host, subnet && '192.168.1.128/25'::cidr \
         FROM nets ORDER BY host",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(false),
            ],
            vec![
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
            ],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT '192.168.1.0/24'::cidr <<= '192.168.1.0/24'::cidr, \
         '192.168.1.0/24'::cidr << '192.168.1.0/24'::cidr",
    ));
    assert_eq!(r, vec![vec![Value::Bool(true), Value::Bool(false)]]);

    let r = rows(run(
        &mut db,
        "SELECT oprname FROM pg_operator \
         WHERE oprname IN ('<<', '<<=', '>>', '>>=', '&&') ORDER BY oprname",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("&&".into())],
            vec![Value::Text("<<".into())],
            vec![Value::Text("<<=".into())],
            vec![Value::Text(">>".into())],
            vec![Value::Text(">>=".into())],
        ]
    );
}

#[test]
fn xml_and_full_text_types_are_text_backed_with_catalog_metadata() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE docs (body xml, lexemes tsvector, query tsquery)",
    );
    run(
        &mut db,
        "INSERT INTO docs VALUES ('<doc><title>Postgres</title></doc>', '''postgres'':1', '''postgres''')",
    );

    let r = rows(run(&mut db, "SELECT body, lexemes, query FROM docs"));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("<doc><title>Postgres</title></doc>".into()),
            Value::Text("'postgres':1".into()),
            Value::Text("'postgres'".into()),
        ]]
    );

    let r = rows(run(
        &mut db,
        "SELECT column_name, data_type FROM information_schema.columns \
         WHERE table_name = 'docs' ORDER BY ordinal_position",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("body".into()), Value::Text("xml".into())],
            vec![
                Value::Text("lexemes".into()),
                Value::Text("tsvector".into())
            ],
            vec![Value::Text("query".into()), Value::Text("tsquery".into())],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT typname, oid FROM pg_type \
         WHERE typname IN ('xml', 'tsvector', 'tsquery') ORDER BY typname",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("tsquery".into()), Value::Int(3615)],
            vec![Value::Text("tsvector".into()), Value::Int(3614)],
            vec![Value::Text("xml".into()), Value::Int(142)],
        ]
    );
}

#[test]
fn full_text_search_functions_and_match_operator() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE docs (id integer, body text, lexemes tsvector)",
    );
    run(
        &mut db,
        "INSERT INTO docs VALUES \
         (1, 'Rust makes Postgres searchable', to_tsvector('Rust makes Postgres searchable')), \
         (2, 'SQLite has tables', to_tsvector('SQLite has tables'))",
    );

    let r = rows(run(
        &mut db,
        "SELECT id FROM docs WHERE lexemes @@ plainto_tsquery('postgres rust') ORDER BY id",
    ));
    assert_eq!(r, vec![vec![Value::Int(1)]]);

    let r = rows(run(
        &mut db,
        "SELECT to_tsvector('Hello hello world'), \
         plainto_tsquery('Hello world'), \
         to_tsquery('hello & !missing')",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("'hello':1,2 'world':3".into()),
            Value::Text("'hello' & 'world'".into()),
            Value::Text("'hello' & ! 'missing'".into()),
        ]]
    );

    let r = rows(run(
        &mut db,
        "SELECT ts_rank(lexemes, plainto_tsquery('postgres rust missing')) \
         FROM docs WHERE id = 1",
    ));
    assert_eq!(r, vec![vec![Value::Float(2.0 / 3.0)]]);

    let r = rows(run(
        &mut db,
        "SELECT proname FROM pg_proc \
         WHERE proname IN ('to_tsvector', 'plainto_tsquery', 'to_tsquery', 'ts_rank') \
         ORDER BY proname",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("plainto_tsquery".into())],
            vec![Value::Text("to_tsquery".into())],
            vec![Value::Text("to_tsvector".into())],
            vec![Value::Text("ts_rank".into())],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT oprname FROM pg_operator WHERE oprname = '@@'",
    ));
    assert_eq!(r, vec![vec![Value::Text("@@".into())]]);
}

#[test]
fn loose_number_text_comparison() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer)");
    run(&mut db, "INSERT INTO t VALUES (16384)");
    // Integer column compared to a string literal (as catalog queries do).
    let r = rows(run(&mut db, "SELECT id FROM t WHERE id = '16384'"));
    assert_eq!(r, vec![vec![Value::Int(16384)]]);
}

#[test]
fn subqueries() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE users (id serial PRIMARY KEY, name text)",
    );
    run(
        &mut db,
        "CREATE TABLE orders (id serial, user_id integer, amount integer)",
    );
    run(
        &mut db,
        "INSERT INTO users (name) VALUES ('Alice'),('Bob'),('Carol')",
    );
    run(
        &mut db,
        "INSERT INTO orders (user_id, amount) VALUES (1,100),(1,50),(2,200)",
    );

    // IN (subquery) — duplicate user_ids must not duplicate output rows.
    let r = rows(run(
        &mut db,
        "SELECT name FROM users WHERE id IN (SELECT user_id FROM orders) ORDER BY name",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("Alice".into())],
            vec![Value::Text("Bob".into())]
        ]
    );

    // NOT IN (subquery).
    let r = rows(run(
        &mut db,
        "SELECT name FROM users WHERE id NOT IN (SELECT user_id FROM orders)",
    ));
    assert_eq!(r, vec![vec![Value::Text("Carol".into())]]);

    // Scalar subquery in the projection.
    let r = rows(run(&mut db, "SELECT (SELECT count(*) FROM orders) AS n"));
    assert_eq!(r[0][0], Value::Int(3));

    // Scalar subquery in WHERE.
    let r = rows(run(
        &mut db,
        "SELECT name FROM users WHERE (SELECT max(amount) FROM orders) > 150 AND id = 1",
    ));
    assert_eq!(r, vec![vec![Value::Text("Alice".into())]]);

    // EXISTS / NOT EXISTS (uncorrelated).
    let r = rows(run(
        &mut db,
        "SELECT name FROM users WHERE EXISTS (SELECT 1 FROM orders WHERE amount > 1000)",
    ));
    assert_eq!(r.len(), 0);
    let r = rows(run(
        &mut db,
        "SELECT count(*) FROM users WHERE NOT EXISTS (SELECT 1 FROM orders WHERE amount > 1000)",
    ));
    assert_eq!(r[0][0], Value::Int(3));

    // DELETE driven by a scalar subquery.
    run(
        &mut db,
        "DELETE FROM orders WHERE amount < (SELECT avg(amount) FROM orders)",
    );
    let r = rows(run(&mut db, "SELECT amount FROM orders ORDER BY amount"));
    assert_eq!(r, vec![vec![Value::Int(200)]]);
}

#[test]
fn statistical_and_boolean_aggregates() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (x integer, flag boolean)");
    run(
        &mut db,
        "INSERT INTO t VALUES (2,true),(4,true),(4,false),(4,true),(5,true),(5,true),(7,true),(9,true)",
    );

    // Population vs sample variance/stddev on the classic 2,4,4,4,5,5,7,9 set:
    // mean=5, var_pop=4, stddev_pop=2.
    let r = rows(run(
        &mut db,
        "SELECT var_pop(x), stddev_pop(x) FROM t",
    ));
    assert_eq!(r[0], vec![Value::Float(4.0), Value::Float(2.0)]);

    // Sample variance = 32/7 ≈ 4.571..., stddev = sqrt of that.
    let r = rows(run(&mut db, "SELECT variance(x), stddev(x) FROM t"));
    let var = match r[0][0] {
        Value::Float(f) => f,
        _ => panic!("expected float"),
    };
    assert!((var - 32.0 / 7.0).abs() < 1e-9);

    // bool_and / bool_or.
    let r = rows(run(&mut db, "SELECT bool_and(flag), bool_or(flag) FROM t"));
    assert_eq!(r[0], vec![Value::Bool(false), Value::Bool(true)]);

    // Sample stddev of a single row is NULL.
    let r = rows(run(&mut db, "SELECT stddev(x) FROM t WHERE x = 2"));
    assert_eq!(r[0][0], Value::Null);
}

#[test]
fn window_functions() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE sales (dept text, name text, amount integer)",
    );
    run(
        &mut db,
        "INSERT INTO sales VALUES \
         ('a','Al',100),('a','Bo',100),('a','Cy',300),('b','Di',50),('b','Ed',70)",
    );

    // row_number, rank, dense_rank partitioned by dept ordered by amount.
    let r = rows(run(
        &mut db,
        "SELECT name, \
                row_number() OVER (PARTITION BY dept ORDER BY amount) AS rn, \
                rank() OVER (PARTITION BY dept ORDER BY amount) AS rk, \
                dense_rank() OVER (PARTITION BY dept ORDER BY amount) AS dr \
         FROM sales ORDER BY dept, amount, name",
    ));
    // dept a: Al(100,rn1,rk1,dr1), Bo(100,rn2,rk1,dr1), Cy(300,rn3,rk3,dr2)
    // dept b: Di(50,rn1), Ed(70,rn2)
    assert_eq!(
        r,
        vec![
            vec![Value::Text("Al".into()), Value::Int(1), Value::Int(1), Value::Int(1)],
            vec![Value::Text("Bo".into()), Value::Int(2), Value::Int(1), Value::Int(1)],
            vec![Value::Text("Cy".into()), Value::Int(3), Value::Int(3), Value::Int(2)],
            vec![Value::Text("Di".into()), Value::Int(1), Value::Int(1), Value::Int(1)],
            vec![Value::Text("Ed".into()), Value::Int(2), Value::Int(2), Value::Int(2)],
        ]
    );

    // Partition-wide aggregate window (no ORDER BY → whole partition).
    let r = rows(run(
        &mut db,
        "SELECT name, sum(amount) OVER (PARTITION BY dept) AS dept_total \
         FROM sales ORDER BY dept, name",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("Al".into()), Value::Int(500)],
            vec![Value::Text("Bo".into()), Value::Int(500)],
            vec![Value::Text("Cy".into()), Value::Int(500)],
            vec![Value::Text("Di".into()), Value::Int(120)],
            vec![Value::Text("Ed".into()), Value::Int(120)],
        ]
    );

    // Running total via ORDER BY (default frame = up to current peer).
    let r = rows(run(
        &mut db,
        "SELECT amount, sum(amount) OVER (ORDER BY amount) AS running \
         FROM sales WHERE dept = 'b' ORDER BY amount",
    ));
    // 50 -> 50, 70 -> 120
    assert_eq!(
        r,
        vec![
            vec![Value::Int(50), Value::Int(50)],
            vec![Value::Int(70), Value::Int(120)],
        ]
    );

    // lag / lead over the whole set ordered by amount.
    let r = rows(run(
        &mut db,
        "SELECT amount, lag(amount) OVER (ORDER BY amount) AS prev, \
                lead(amount) OVER (ORDER BY amount) AS next \
         FROM sales WHERE dept = 'a' ORDER BY amount",
    ));
    // amounts 100,100,300
    assert_eq!(
        r,
        vec![
            vec![Value::Int(100), Value::Null, Value::Int(100)],
            vec![Value::Int(100), Value::Int(100), Value::Int(300)],
            vec![Value::Int(300), Value::Int(100), Value::Null],
        ]
    );
}

#[test]
fn recursive_ctes() {
    let mut db = Database::new();

    // Counter: 1..5 via UNION ALL.
    let r = rows(run(
        &mut db,
        "WITH RECURSIVE nums(n) AS ( \
           SELECT 1 \
           UNION ALL \
           SELECT n + 1 FROM nums WHERE n < 5 \
         ) SELECT n FROM nums ORDER BY n",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)],
            vec![Value::Int(4)],
            vec![Value::Int(5)],
        ]
    );

    // Graph reachability over an edges table, with UNION (dedup) to terminate
    // on cycles.
    run(&mut db, "CREATE TABLE edges (src integer, dst integer)");
    run(
        &mut db,
        "INSERT INTO edges VALUES (1,2),(2,3),(3,4),(4,2)",
    );
    let r = rows(run(
        &mut db,
        "WITH RECURSIVE reach(node) AS ( \
           SELECT 1 \
           UNION \
           SELECT e.dst FROM edges e JOIN reach r ON e.src = r.node \
         ) SELECT node FROM reach ORDER BY node",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)],
            vec![Value::Int(4)],
        ]
    );

    // A runaway recursion is capped rather than looping forever.
    let err = Parser::parse_sql(
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r) SELECT n FROM r",
    )
    .map(|mut s| executor::execute(&mut db, s.remove(0)));
    assert!(matches!(err, Ok(Err(_))));
}

#[test]
fn correlated_subqueries() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE users (id serial PRIMARY KEY, name text)",
    );
    run(
        &mut db,
        "CREATE TABLE orders (id serial, user_id integer, amount integer)",
    );
    run(
        &mut db,
        "INSERT INTO users (name) VALUES ('Alice'),('Bob'),('Carol')",
    );
    run(
        &mut db,
        "INSERT INTO orders (user_id, amount) VALUES (1,100),(1,50),(2,200)",
    );

    // Correlated EXISTS: keep users who have at least one order.
    let r = rows(run(
        &mut db,
        "SELECT name FROM users u \
         WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id) \
         ORDER BY name",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("Alice".into())],
            vec![Value::Text("Bob".into())]
        ]
    );

    // Correlated NOT EXISTS: users without orders.
    let r = rows(run(
        &mut db,
        "SELECT name FROM users u \
         WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id)",
    ));
    assert_eq!(r, vec![vec![Value::Text("Carol".into())]]);

    // Correlated scalar subquery in the projection (per-user order count).
    let r = rows(run(
        &mut db,
        "SELECT name, (SELECT count(*) FROM orders o WHERE o.user_id = u.id) AS n \
         FROM users u ORDER BY name",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("Alice".into()), Value::Int(2)],
            vec![Value::Text("Bob".into()), Value::Int(1)],
            vec![Value::Text("Carol".into()), Value::Int(0)],
        ]
    );

    // Correlated scalar subquery in WHERE: users whose total spend exceeds 120.
    let r = rows(run(
        &mut db,
        "SELECT name FROM users u \
         WHERE (SELECT sum(amount) FROM orders o WHERE o.user_id = u.id) > 120 \
         ORDER BY name",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("Alice".into())],
            vec![Value::Text("Bob".into())]
        ]
    );

    // Correlated subquery comparing an inner column to an outer column directly.
    let r = rows(run(
        &mut db,
        "SELECT DISTINCT u.name FROM users u \
         WHERE u.id IN (SELECT o.user_id FROM orders o WHERE o.amount >= u.id * 100) \
         ORDER BY u.name",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("Alice".into())],
            vec![Value::Text("Bob".into())]
        ]
    );

    // Uncorrelated subqueries still resolve unchanged alongside correlated ones.
    let r = rows(run(
        &mut db,
        "SELECT name FROM users u \
         WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id) \
           AND (SELECT count(*) FROM orders) = 3 \
         ORDER BY name",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("Alice".into())],
            vec![Value::Text("Bob".into())]
        ]
    );
}

#[test]
fn common_table_expressions_materialize_select_sources() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE orders (id integer, user_id integer, amount integer)",
    );
    run(
        &mut db,
        "INSERT INTO orders VALUES (1, 10, 100), (2, 10, 75), (3, 20, 25)",
    );

    let r = rows(run(
        &mut db,
        "WITH totals(user_id, total) AS ( \
           SELECT user_id, sum(amount) FROM orders GROUP BY user_id \
         ) \
         SELECT user_id, total FROM totals WHERE total > 50 ORDER BY user_id",
    ));
    assert_eq!(r, vec![vec![Value::Int(10), Value::Int(175)]]);

    let r = rows(run(
        &mut db,
        "WITH expensive AS (SELECT id, user_id FROM orders WHERE amount >= 75), \
              renamed(order_id, owner_id) AS (SELECT id, user_id FROM expensive) \
         SELECT order_id, owner_id FROM renamed ORDER BY order_id",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(2), Value::Int(10)],
        ]
    );

    let r = rows(run(
        &mut db,
        "WITH ids AS (SELECT id FROM orders WHERE id < 3) \
         SELECT o.id, o.amount FROM orders o JOIN ids ON ids.id = o.id ORDER BY o.id",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Int(100)],
            vec![Value::Int(2), Value::Int(75)],
        ]
    );
}

#[test]
fn unique_and_primary_key_enforcement() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t (id integer PRIMARY KEY, name text)",
    );
    run(&mut db, "INSERT INTO t VALUES (1, 'a'), (2, 'b')");

    // Duplicate primary key (against existing row) is rejected.
    let dup = Parser::parse_sql("INSERT INTO t VALUES (1, 'dup')")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(executor::execute(&mut db, dup).is_err());

    // Duplicate within the same batch is rejected.
    let batch = Parser::parse_sql("INSERT INTO t VALUES (3, 'c'), (3, 'd')")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(executor::execute(&mut db, batch).is_err());

    // UPDATE that collides with another row's key is rejected.
    let upd = Parser::parse_sql("UPDATE t SET id = 2 WHERE id = 1")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(executor::execute(&mut db, upd).is_err());

    // The table is unchanged by the rejected operations.
    let r = rows(run(&mut db, "SELECT id FROM t ORDER BY id"));
    assert_eq!(r, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);

    // A standalone UNIQUE index is enforced too.
    run(&mut db, "CREATE TABLE u (email text)");
    run(&mut db, "CREATE UNIQUE INDEX u_email ON u (email)");
    run(&mut db, "INSERT INTO u VALUES ('x@y.com')");
    let dup = Parser::parse_sql("INSERT INTO u VALUES ('x@y.com')")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(executor::execute(&mut db, dup).is_err());
}

#[test]
fn multi_column_primary_key_and_unique_constraints_are_enforced_and_cataloged() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE memberships (org integer, user_id integer, role text, PRIMARY KEY (org, user_id))",
    );
    run(
        &mut db,
        "ALTER TABLE memberships ADD CONSTRAINT memberships_org_role_key UNIQUE (org, role)",
    );
    run(
        &mut db,
        "INSERT INTO memberships VALUES (1, 10, 'admin'), (1, 11, 'member'), (2, 10, 'admin')",
    );

    let dup_pk = Parser::parse_sql("INSERT INTO memberships VALUES (1, 10, 'owner')")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(executor::execute(&mut db, dup_pk).is_err());

    let dup_unique = Parser::parse_sql("INSERT INTO memberships VALUES (1, 12, 'admin')")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(executor::execute(&mut db, dup_unique).is_err());

    let batch_dup = Parser::parse_sql("INSERT INTO memberships VALUES (3, 1, 'a'), (3, 1, 'b')")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(executor::execute(&mut db, batch_dup).is_err());

    let upd =
        Parser::parse_sql("UPDATE memberships SET user_id = 10 WHERE org = 1 AND user_id = 11")
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
    assert!(executor::execute(&mut db, upd).is_err());

    let r = rows(run(
        &mut db,
        "SELECT conname, contype, conkey FROM pg_constraint \
         WHERE conname IN ('memberships_org_user_id_pkey', 'memberships_org_role_key') \
         ORDER BY conname",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("memberships_org_role_key".into()),
                Value::Text("u".into()),
                Value::Text("1 3".into()),
            ],
            vec![
                Value::Text("memberships_org_user_id_pkey".into()),
                Value::Text("p".into()),
                Value::Text("1 2".into()),
            ],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT indisunique, indisprimary, indkey FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indexrelid \
         WHERE c.relname IN ('memberships_org_user_id_pkey', 'memberships_org_role_key') \
         ORDER BY c.relname",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Bool(true),
                Value::Bool(false),
                Value::Text("1 3".into())
            ],
            vec![
                Value::Bool(true),
                Value::Bool(true),
                Value::Text("1 2".into())
            ],
        ]
    );
}

#[test]
fn insert_on_conflict_do_nothing() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t (id integer PRIMARY KEY, name text)",
    );
    run(&mut db, "INSERT INTO t VALUES (1, 'a')");

    let r = run(
        &mut db,
        "INSERT INTO t VALUES (1, 'dup'), (2, 'b'), (2, 'dup2') ON CONFLICT (id) DO NOTHING RETURNING id, name",
    );
    match r {
        ExecResult::Rows { rows, tag, .. } => {
            assert_eq!(tag, "INSERT 0 1");
            assert_eq!(rows, vec![vec![Value::Int(2), Value::Text("b".into())]]);
        }
        _ => panic!("expected RETURNING rows"),
    }

    let r = rows(run(&mut db, "SELECT id, name FROM t ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Text("a".into())],
            vec![Value::Int(2), Value::Text("b".into())],
        ]
    );

    let r = run(
        &mut db,
        "INSERT INTO t VALUES (1, 'again') ON CONFLICT DO NOTHING",
    );
    assert!(matches!(r, ExecResult::Command(ref tag) if tag == "INSERT 0 0"));
}

#[test]
fn insert_on_conflict_do_update_updates_existing_rows() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE counters (id integer PRIMARY KEY, name text, hits integer)",
    );
    run(
        &mut db,
        "INSERT INTO counters VALUES (1, 'a', 1), (2, 'b', 5)",
    );

    let r = run(
        &mut db,
        "INSERT INTO counters VALUES (1, 'renamed', 10), (3, 'c', 1) \
         ON CONFLICT (id) DO UPDATE SET name = excluded.name, hits = hits + excluded.hits \
         RETURNING id, name, hits",
    );
    match r {
        ExecResult::Rows { rows, tag, .. } => {
            assert_eq!(tag, "INSERT 0 2");
            assert_eq!(
                rows,
                vec![
                    vec![Value::Int(1), Value::Text("renamed".into()), Value::Int(11),],
                    vec![Value::Int(3), Value::Text("c".into()), Value::Int(1)],
                ]
            );
        }
        _ => panic!("expected RETURNING rows"),
    }

    let r = rows(run(
        &mut db,
        "SELECT id, name, hits FROM counters ORDER BY id",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Text("renamed".into()), Value::Int(11),],
            vec![Value::Int(2), Value::Text("b".into()), Value::Int(5)],
            vec![Value::Int(3), Value::Text("c".into()), Value::Int(1)],
        ]
    );

    let r = run(
        &mut db,
        "INSERT INTO counters VALUES (2, 'ignored', 100) \
         ON CONFLICT (id) DO UPDATE SET hits = excluded.hits WHERE excluded.hits < 10",
    );
    assert!(matches!(r, ExecResult::Command(ref tag) if tag == "INSERT 0 0"));
    let r = rows(run(&mut db, "SELECT hits FROM counters WHERE id = 2"));
    assert_eq!(r, vec![vec![Value::Int(5)]]);

    let err = Parser::parse_sql(
        "INSERT INTO counters VALUES (1, 'x', 1), (1, 'y', 2) \
         ON CONFLICT (id) DO UPDATE SET hits = excluded.hits",
    )
    .and_then(|mut stmts| executor::execute(&mut db, stmts.remove(0)).map(|_| ()))
    .expect_err("same row cannot be updated twice");
    assert_eq!(
        err,
        "ON CONFLICT DO UPDATE command cannot affect row a second time"
    );
}

#[test]
fn truncate_table_clears_rows_and_indexes() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t (id serial PRIMARY KEY, category text)",
    );
    run(&mut db, "CREATE INDEX t_category_idx ON t (category)");
    run(
        &mut db,
        "INSERT INTO t (category) VALUES ('a'), ('b'), ('a')",
    );

    run(&mut db, "TRUNCATE TABLE t");
    let r = rows(run(&mut db, "SELECT count(*) FROM t"));
    assert_eq!(r, vec![vec![Value::Int(0)]]);
    let r = rows(run(&mut db, "SELECT id FROM t WHERE category = 'a'"));
    assert!(r.is_empty());

    run(&mut db, "INSERT INTO t (category) VALUES ('c')");
    let r = rows(run(&mut db, "SELECT id, category FROM t"));
    assert_eq!(r, vec![vec![Value::Int(4), Value::Text("c".into())]]);
}

#[test]
fn sequences_are_first_class_objects() {
    let mut db = Database::new();

    match run(
        &mut db,
        "CREATE SEQUENCE invoice_seq START WITH 10 INCREMENT BY 5",
    ) {
        ExecResult::Command(tag) => assert_eq!(tag, "CREATE SEQUENCE"),
        other => panic!("expected CREATE SEQUENCE command, got {}", tag_of(&other)),
    }

    let r = rows(run(
        &mut db,
        "SELECT relname, relkind FROM pg_class WHERE relname = 'invoice_seq'",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("invoice_seq".into()),
            Value::Text("S".into()),
        ]]
    );

    let r = rows(run(
        &mut db,
        "SELECT seqstart, seqincrement FROM pg_sequence",
    ));
    assert_eq!(r, vec![vec![Value::Int(10), Value::Int(5)]]);

    let r = rows(run(&mut db, "SELECT nextval('invoice_seq')"));
    assert_eq!(r, vec![vec![Value::Int(10)]]);
    let r = rows(run(&mut db, "SELECT nextval('invoice_seq')"));
    assert_eq!(r, vec![vec![Value::Int(15)]]);
    let r = rows(run(&mut db, "SELECT currval('invoice_seq')"));
    assert_eq!(r, vec![vec![Value::Int(15)]]);

    match run(
        &mut db,
        "ALTER SEQUENCE invoice_seq RESTART WITH 100 INCREMENT BY 10",
    ) {
        ExecResult::Command(tag) => assert_eq!(tag, "ALTER SEQUENCE"),
        other => panic!("expected ALTER SEQUENCE command, got {}", tag_of(&other)),
    }
    let r = rows(run(&mut db, "SELECT nextval('invoice_seq')"));
    assert_eq!(r, vec![vec![Value::Int(100)]]);

    let r = rows(run(&mut db, "SELECT setval('invoice_seq', 250)"));
    assert_eq!(r, vec![vec![Value::Int(250)]]);
    let r = rows(run(&mut db, "SELECT nextval('invoice_seq')"));
    assert_eq!(r, vec![vec![Value::Int(260)]]);

    match run(&mut db, "DROP SEQUENCE invoice_seq") {
        ExecResult::Command(tag) => assert_eq!(tag, "DROP SEQUENCE"),
        other => panic!("expected DROP SEQUENCE command, got {}", tag_of(&other)),
    }
    let r = rows(run(
        &mut db,
        "SELECT relname FROM pg_class WHERE relname = 'invoice_seq'",
    ));
    assert!(r.is_empty());
}

#[test]
fn alter_table() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, name text)");
    run(&mut db, "INSERT INTO t VALUES (1, 'a'), (2, 'b')");

    // ADD COLUMN with a default backfills existing rows.
    run(
        &mut db,
        "ALTER TABLE t ADD COLUMN active boolean DEFAULT true",
    );
    run(&mut db, "ALTER TABLE t ADD COLUMN score integer");
    let r = rows(run(&mut db, "SELECT id, active, score FROM t ORDER BY id"));
    assert_eq!(r[0], vec![Value::Int(1), Value::Bool(true), Value::Null]);

    // RENAME COLUMN.
    run(&mut db, "ALTER TABLE t RENAME COLUMN name TO label");
    let r = rows(run(&mut db, "SELECT label FROM t WHERE id = 1"));
    assert_eq!(r[0][0], Value::Text("a".into()));

    // DROP COLUMN.
    run(&mut db, "ALTER TABLE t DROP COLUMN score");
    let r = rows(run(&mut db, "SELECT * FROM t WHERE id = 1"));
    assert_eq!(
        r[0],
        vec![Value::Int(1), Value::Text("a".into()), Value::Bool(true)]
    );

    // RENAME TABLE.
    run(&mut db, "ALTER TABLE t RENAME TO items");
    let r = rows(run(&mut db, "SELECT count(*) FROM items"));
    assert_eq!(r[0][0], Value::Int(2));
}

#[test]
fn distinct_aggregates_and_string_agg() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (region text, amount integer)");
    run(
        &mut db,
        "INSERT INTO t VALUES ('w',10),('w',10),('w',20),('e',30)",
    );

    let r = rows(run(
        &mut db,
        "SELECT count(*), count(DISTINCT amount), sum(DISTINCT amount) FROM t",
    ));
    assert_eq!(r[0], vec![Value::Int(4), Value::Int(3), Value::Int(60)]);

    let r = rows(run(
        &mut db,
        "SELECT region, string_agg(amount::text, ',') FROM t GROUP BY region ORDER BY region",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("e".into()), Value::Text("30".into())],
            vec![Value::Text("w".into()), Value::Text("10,10,20".into())],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT string_agg(DISTINCT region, '|') FROM t",
    ));
    assert_eq!(r[0][0], Value::Text("w|e".into()));
}

#[test]
fn aggregate_filter_where_filters_input_rows() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (region text, amount integer)");
    run(
        &mut db,
        "INSERT INTO t VALUES ('w',10),('w',20),('e',30),('e',NULL)",
    );

    let r = rows(run(
        &mut db,
        "SELECT count(*) FILTER (WHERE region = 'w'), \
         count(amount) FILTER (WHERE region = 'e'), \
         sum(amount) FILTER (WHERE amount >= 20) FROM t",
    ));
    assert_eq!(r[0], vec![Value::Int(2), Value::Int(1), Value::Int(50)]);

    let r = rows(run(
        &mut db,
        "SELECT region, count(*) FILTER (WHERE amount IS NOT NULL) \
         FROM t GROUP BY region ORDER BY region",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("e".into()), Value::Int(1)],
            vec![Value::Text("w".into()), Value::Int(2)],
        ]
    );
}

#[test]
fn date_time_functions() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE e (ts timestamp)");
    run(&mut db, "INSERT INTO e VALUES ('2024-03-15 10:30:45')");

    let r = rows(run(
        &mut db,
        "SELECT EXTRACT(year FROM ts), EXTRACT(month FROM ts), date_part('day', ts) FROM e",
    ));
    assert_eq!(
        r[0],
        vec![Value::Float(2024.0), Value::Float(3.0), Value::Float(15.0)]
    );

    let r = rows(run(&mut db, "SELECT date_trunc('month', ts) FROM e"));
    assert_eq!(r[0][0], Value::Text("2024-03-01 00:00:00".into()));
}

#[test]
fn date_plus_minus_integer_days() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE e (d date)");
    run(&mut db, "INSERT INTO e VALUES ('2024-02-28')");

    let r = rows(run(&mut db, "SELECT d + 2, 3 + d, d - 30 FROM e"));
    assert_eq!(
        r[0],
        vec![
            Value::Text("2024-03-01".into()),
            Value::Text("2024-03-02".into()),
            Value::Text("2024-01-29".into()),
        ]
    );
}

#[test]
fn qualified_column_on_single_table() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, v integer)");
    run(&mut db, "INSERT INTO t VALUES (1, 10), (2, 20)");
    // A qualified reference must still resolve against a single table.
    let r = rows(run(&mut db, "SELECT t.v FROM t WHERE t.id = 2"));
    assert_eq!(r, vec![vec![Value::Int(20)]]);
}

#[test]
fn boolean_logic_in_where() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (x integer, ok boolean)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, true), (2, false), (3, true)",
    );
    let r = rows(run(&mut db, "SELECT x FROM t WHERE ok = true AND x > 1"));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(3));
}

fn command_tag(res: ExecResult) -> String {
    match res {
        ExecResult::Command(t) => t,
        other => panic!("expected command, got {}", tag_of(&other)),
    }
}

#[test]
fn grant_revoke_table_privileges() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer)");
    run(&mut db, "CREATE ROLE alice");

    assert_eq!(
        command_tag(run(&mut db, "GRANT SELECT, INSERT ON t TO alice")),
        "GRANT"
    );
    assert_eq!(
        command_tag(run(&mut db, "GRANT ALL ON t TO public")),
        "GRANT"
    );
    assert_eq!(
        command_tag(run(&mut db, "GRANT UPDATE ON TABLE t TO alice WITH GRANT OPTION")),
        "GRANT"
    );
    assert_eq!(
        command_tag(run(&mut db, "REVOKE UPDATE ON t FROM alice")),
        "REVOKE"
    );
    assert_eq!(
        command_tag(run(&mut db, "REVOKE ALL ON t FROM public CASCADE")),
        "REVOKE"
    );
}

#[test]
fn grant_on_missing_table_errors() {
    let mut db = Database::new();
    run(&mut db, "CREATE ROLE alice");
    let stmts = Parser::parse_sql("GRANT SELECT ON nope TO alice").expect("parse");
    let result = executor::execute(&mut db, stmts.into_iter().next().unwrap());
    match result {
        Err(err) => assert!(err.contains("nope"), "unexpected error: {err}"),
        Ok(_) => panic!("expected error"),
    }
}

#[test]
fn grant_revoke_role_membership() {
    let mut db = Database::new();
    run(&mut db, "CREATE ROLE devs");
    run(&mut db, "CREATE ROLE alice");

    assert_eq!(command_tag(run(&mut db, "GRANT devs TO alice")), "GRANT");

    // pg_auth_members reflects the membership: roleid = group (devs), member = alice.
    let r = rows(run(
        &mut db,
        "SELECT m.rolname, g.rolname \
         FROM pg_auth_members am \
         JOIN pg_roles g ON g.oid = am.roleid \
         JOIN pg_roles m ON m.oid = am.member \
         ORDER BY m.rolname",
    ));
    assert_eq!(
        r,
        vec![vec![Value::Text("alice".into()), Value::Text("devs".into())]]
    );

    assert_eq!(command_tag(run(&mut db, "REVOKE devs FROM alice")), "REVOKE");
    let r = rows(run(&mut db, "SELECT * FROM pg_auth_members"));
    assert_eq!(r.len(), 0);
}

#[test]
fn create_role_in_role_records_membership() {
    let mut db = Database::new();
    run(&mut db, "CREATE ROLE devs");
    run(&mut db, "CREATE ROLE ops");
    // alice is IN ROLE devs; bob is a member added via ROLE.
    run(&mut db, "CREATE ROLE alice IN ROLE devs");
    run(&mut db, "CREATE ROLE bob");
    run(&mut db, "CREATE ROLE team ROLE bob, alice");

    let r = rows(run(
        &mut db,
        "SELECT m.rolname, g.rolname \
         FROM pg_auth_members am \
         JOIN pg_roles g ON g.oid = am.roleid \
         JOIN pg_roles m ON m.oid = am.member \
         ORDER BY g.rolname, m.rolname",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("alice".into()), Value::Text("devs".into())],
            vec![Value::Text("alice".into()), Value::Text("team".into())],
            vec![Value::Text("bob".into()), Value::Text("team".into())],
        ]
    );
}

/// Parse + execute SQL, returning `Ok(())` on success or the first error so
/// tests can assert on rejection messages. (`ExecResult` is not `Debug`.)
fn try_run(db: &mut Database, sql: &str) -> Result<(), String> {
    for s in Parser::parse_sql(sql).expect("parse") {
        executor::execute(db, s)?;
    }
    Ok(())
}

#[test]
fn enum_type_accepts_and_rejects_values() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')");
    run(&mut db, "CREATE TABLE t (id integer, m mood)");
    run(&mut db, "INSERT INTO t VALUES (1, 'happy'), (2, 'sad')");
    let r = rows(run(&mut db, "SELECT m FROM t ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("happy".into())],
            vec![Value::Text("sad".into())],
        ]
    );

    let err = try_run(&mut db, "INSERT INTO t VALUES (3, 'angry')")
        .expect_err("non-label enum value should be rejected");
    assert!(
        err.contains("invalid input value for enum mood: \"angry\""),
        "unexpected error: {err}"
    );

    // NULL is allowed when the column is nullable.
    run(&mut db, "INSERT INTO t VALUES (4, NULL)");

    // UPDATE to an invalid label is rejected too.
    let err = try_run(&mut db, "UPDATE t SET m = 'meh' WHERE id = 1")
        .expect_err("invalid enum update should be rejected");
    assert!(err.contains("invalid input value for enum mood"), "{err}");
}

#[test]
fn enum_drop_type() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE mood AS ENUM ('a', 'b')");
    let r = run(&mut db, "DROP TYPE mood");
    assert!(matches!(r, ExecResult::Command(ref t) if t == "DROP TYPE"));
    // DROP TYPE IF EXISTS on a missing type is a no-op.
    run(&mut db, "DROP TYPE IF EXISTS mood");
    let err = try_run(&mut db, "DROP TYPE mood").expect_err("missing type");
    assert!(err.contains("does not exist"), "{err}");
}

#[test]
fn domain_enforces_not_null_and_check() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE DOMAIN pos AS integer NOT NULL CHECK (VALUE > 0)",
    );
    run(&mut db, "CREATE TABLE t (id integer, p pos)");
    run(&mut db, "INSERT INTO t VALUES (1, 5)");

    // Base type coercion: a numeric string coerces to the integer base type.
    run(&mut db, "INSERT INTO t VALUES (2, '7')");
    let r = rows(run(&mut db, "SELECT p FROM t ORDER BY id"));
    assert_eq!(r, vec![vec![Value::Int(5)], vec![Value::Int(7)]]);

    let err = try_run(&mut db, "INSERT INTO t VALUES (3, 0)")
        .expect_err("domain CHECK violation should be rejected");
    assert!(err.contains("violates check constraint"), "{err}");

    let err = try_run(&mut db, "INSERT INTO t VALUES (4, NULL)")
        .expect_err("domain NOT NULL should be rejected");
    assert!(err.contains("does not allow null values"), "{err}");

    // UPDATE is enforced too.
    let err = try_run(&mut db, "UPDATE t SET p = -1 WHERE id = 1")
        .expect_err("domain CHECK on update should be rejected");
    assert!(err.contains("violates check constraint"), "{err}");
}

#[test]
fn domain_drop() {
    let mut db = Database::new();
    run(&mut db, "CREATE DOMAIN pos AS integer CHECK (VALUE > 0)");
    let r = run(&mut db, "DROP DOMAIN pos");
    assert!(matches!(r, ExecResult::Command(ref t) if t == "DROP DOMAIN"));
    run(&mut db, "DROP DOMAIN IF EXISTS pos");
}

#[test]
fn composite_type_create_and_use() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE addr AS (street text, zip integer)");
    run(&mut db, "CREATE TABLE t (id integer, a addr)");
    // Text-backed: any value is accepted; no value semantics enforced.
    run(&mut db, "INSERT INTO t VALUES (1, '(main,12345)')");
    let r = rows(run(&mut db, "SELECT a FROM t"));
    assert_eq!(r, vec![vec![Value::Text("(main,12345)".into())]]);
    let r = run(&mut db, "DROP TYPE addr");
    assert!(matches!(r, ExecResult::Command(ref t) if t == "DROP TYPE"));
}

#[test]
fn range_type_create_and_use() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE intrange AS RANGE (subtype = integer)");
    run(&mut db, "CREATE TABLE t (id integer, r intrange)");
    run(&mut db, "INSERT INTO t VALUES (1, '[1,10)')");
    let r = rows(run(&mut db, "SELECT r FROM t"));
    assert_eq!(r, vec![vec![Value::Text("[1,10)".into())]]);
    let r = run(&mut db, "DROP TYPE intrange");
    assert!(matches!(r, ExecResult::Command(ref t) if t == "DROP TYPE"));
}

#[test]
fn duplicate_type_name_is_error() {
    let mut db = Database::new();
    run(&mut db, "CREATE TYPE mood AS ENUM ('a')");
    let err = try_run(&mut db, "CREATE TYPE mood AS ENUM ('b')")
        .expect_err("duplicate type name");
    assert!(err.contains("already exists"), "{err}");
    let err = try_run(&mut db, "CREATE DOMAIN mood AS integer")
        .expect_err("domain clashing with type name");
    assert!(err.contains("already exists"), "{err}");
}

#[test]
fn group_by_rollup_produces_subtotals_and_grand_total() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE sales (region text, city text, amount integer)");
    run(
        &mut db,
        "INSERT INTO sales VALUES \
         ('east', 'ny', 10), ('east', 'ny', 5), ('east', 'boston', 3), \
         ('west', 'la', 7)",
    );
    // ROLLUP(region, city): (region,city) detail rows, per-region subtotals,
    // and the grand total. Order deterministically with NULLs sorting last.
    let r = rows(run(
        &mut db,
        "SELECT region, city, sum(amount) AS total FROM sales \
         GROUP BY ROLLUP(region, city) \
         ORDER BY region, city",
    ));
    assert_eq!(
        r,
        vec![
            // east detail
            vec![Value::Text("east".into()), Value::Text("boston".into()), Value::Int(3)],
            vec![Value::Text("east".into()), Value::Text("ny".into()), Value::Int(15)],
            // east subtotal
            vec![Value::Text("east".into()), Value::Null, Value::Int(18)],
            // west detail + subtotal
            vec![Value::Text("west".into()), Value::Text("la".into()), Value::Int(7)],
            vec![Value::Text("west".into()), Value::Null, Value::Int(7)],
            // grand total
            vec![Value::Null, Value::Null, Value::Int(25)],
        ]
    );
}

#[test]
fn group_by_cube_produces_all_subsets() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (a text, b text, v integer)");
    run(
        &mut db,
        "INSERT INTO t VALUES ('x', 'p', 1), ('x', 'q', 2), ('y', 'p', 4)",
    );
    let r = rows(run(
        &mut db,
        "SELECT a, b, sum(v) FROM t \
         GROUP BY CUBE(a, b) \
         ORDER BY a, b",
    ));
    assert_eq!(
        r,
        vec![
            // (a, b) detail
            vec![Value::Text("x".into()), Value::Text("p".into()), Value::Int(1)],
            vec![Value::Text("x".into()), Value::Text("q".into()), Value::Int(2)],
            vec![Value::Text("x".into()), Value::Null, Value::Int(3)], // a = x
            vec![Value::Text("y".into()), Value::Text("p".into()), Value::Int(4)],
            vec![Value::Text("y".into()), Value::Null, Value::Int(4)], // a = y
            // grouped by b only
            vec![Value::Null, Value::Text("p".into()), Value::Int(5)],
            vec![Value::Null, Value::Text("q".into()), Value::Int(2)],
            // grand total
            vec![Value::Null, Value::Null, Value::Int(7)],
        ]
    );
}

#[test]
fn group_by_grouping_sets_explicit() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (a text, b text, v integer)");
    run(
        &mut db,
        "INSERT INTO t VALUES ('x', 'p', 1), ('x', 'q', 2), ('y', 'p', 4)",
    );
    // Explicit sets: by a, by b, and the grand total ().
    let r = rows(run(
        &mut db,
        "SELECT a, b, sum(v) FROM t \
         GROUP BY GROUPING SETS ((a), (b), ()) \
         ORDER BY a, b",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("x".into()), Value::Null, Value::Int(3)],
            vec![Value::Text("y".into()), Value::Null, Value::Int(4)],
            vec![Value::Null, Value::Text("p".into()), Value::Int(5)],
            vec![Value::Null, Value::Text("q".into()), Value::Int(2)],
            vec![Value::Null, Value::Null, Value::Int(7)],
        ]
    );
}

#[test]
fn ordered_set_aggregates_percentile_and_mode() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE m (g text, v integer)");
    run(
        &mut db,
        "INSERT INTO m VALUES \
         ('a', 1), ('a', 2), ('a', 3), ('a', 4), \
         ('b', 5), ('b', 5), ('b', 7)",
    );

    // percentile_cont(0.5) = median with linear interpolation.
    // group a = [1,2,3,4] -> 2.5 ; group b = [5,5,7] -> 5.0
    let r = rows(run(
        &mut db,
        "SELECT g, percentile_cont(0.5) WITHIN GROUP (ORDER BY v) AS med \
         FROM m GROUP BY g ORDER BY g",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("a".into()), Value::Float(2.5)],
            vec![Value::Text("b".into()), Value::Float(5.0)],
        ]
    );

    // percentile_disc(0.5): smallest value whose cumulative position >= 0.5.
    // group a = [1,2,3,4] -> 2 ; group b = [5,5,7] -> 5
    let r = rows(run(
        &mut db,
        "SELECT g, percentile_disc(0.5) WITHIN GROUP (ORDER BY v) AS d \
         FROM m GROUP BY g ORDER BY g",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("a".into()), Value::Int(2)],
            vec![Value::Text("b".into()), Value::Int(5)],
        ]
    );

    // mode(): most frequent value; ties resolve to the smallest.
    // group a: all distinct -> smallest = 1 ; group b: 5 appears twice -> 5
    let r = rows(run(
        &mut db,
        "SELECT g, mode() WITHIN GROUP (ORDER BY v) AS m \
         FROM m GROUP BY g ORDER BY g",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("a".into()), Value::Int(1)],
            vec![Value::Text("b".into()), Value::Int(5)],
        ]
    );
}

#[test]
fn percentile_cont_whole_table_median() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE p (v integer)");
    run(&mut db, "INSERT INTO p VALUES (1), (2), (3), (4), (100)");
    // No GROUP BY: single group, [1,2,3,4,100] -> median 3.0
    let r = rows(run(
        &mut db,
        "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY v) FROM p",
    ));
    assert_eq!(r[0], vec![Value::Float(3.0)]);
}

#[test]
fn rollup_view_roundtrips_through_serialize() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE sv (a text, v integer)");
    run(&mut db, "INSERT INTO sv VALUES ('x', 1), ('x', 2), ('y', 3)");
    // CREATE VIEW serializes the SELECT (which contains grouping sets) and
    // re-parses it on read, exercising the serialize round-trip.
    run(
        &mut db,
        "CREATE VIEW vr AS SELECT a, sum(v) AS s FROM sv GROUP BY ROLLUP(a)",
    );
    let r = rows(run(&mut db, "SELECT a, s FROM vr ORDER BY a"));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("x".into()), Value::Int(3)],
            vec![Value::Text("y".into()), Value::Int(3)],
            vec![Value::Null, Value::Int(6)],
        ]
    );
}

#[test]
fn merge_matched_update_and_not_matched_insert() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE target (id integer PRIMARY KEY, val text)");
    run(&mut db, "CREATE TABLE source (id integer, val text)");
    run(&mut db, "INSERT INTO target VALUES (1, 'old'), (2, 'keep')");
    run(&mut db, "INSERT INTO source VALUES (1, 'new'), (3, 'inserted')");

    let tag = command_tag(run(
        &mut db,
        "MERGE INTO target t USING source s ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET val = s.val \
         WHEN NOT MATCHED THEN INSERT (id, val) VALUES (s.id, s.val)",
    ));
    // 1 updated (id=1) + 1 inserted (id=3) = MERGE 2.
    assert_eq!(tag, "MERGE 2");

    let r = rows(run(&mut db, "SELECT id, val FROM target ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Text("new".into())],
            vec![Value::Int(2), Value::Text("keep".into())],
            vec![Value::Int(3), Value::Text("inserted".into())],
        ]
    );
}

#[test]
fn merge_matched_delete() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE target (id integer PRIMARY KEY, val text)");
    run(&mut db, "INSERT INTO target VALUES (1, 'a'), (2, 'b'), (3, 'c')");

    let tag = command_tag(run(
        &mut db,
        "MERGE INTO target t \
         USING (VALUES (1), (3)) AS s(id) ON t.id = s.id \
         WHEN MATCHED THEN DELETE",
    ));
    assert_eq!(tag, "MERGE 2");

    let r = rows(run(&mut db, "SELECT id FROM target ORDER BY id"));
    assert_eq!(r, vec![vec![Value::Int(2)]]);
}

#[test]
fn merge_conditional_clauses_and_do_nothing() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE target (id integer PRIMARY KEY, qty integer)");
    run(&mut db, "INSERT INTO target VALUES (1, 10), (2, 20), (3, 30)");

    // For id=1 (qty 10) the AND condition fails -> falls through to the second
    // matched clause (DELETE). For id=2 (qty 20) the first matched clause
    // (UPDATE) applies. id=3 is not in the source, untouched. id=4 inserted.
    let tag = command_tag(run(
        &mut db,
        "MERGE INTO target t \
         USING (VALUES (1, 100), (2, 200), (4, 400)) AS s(id, delta) ON t.id = s.id \
         WHEN MATCHED AND t.qty >= 20 THEN UPDATE SET qty = t.qty + s.delta \
         WHEN MATCHED THEN DELETE \
         WHEN NOT MATCHED THEN INSERT (id, qty) VALUES (s.id, s.delta)",
    ));
    // id=2 update + id=1 delete + id=4 insert = 3.
    assert_eq!(tag, "MERGE 3");

    let r = rows(run(&mut db, "SELECT id, qty FROM target ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(2), Value::Int(220)],
            vec![Value::Int(3), Value::Int(30)],
            vec![Value::Int(4), Value::Int(400)],
        ]
    );
}

#[test]
fn merge_do_nothing_skips_without_counting() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE target (id integer PRIMARY KEY, val text)");
    run(&mut db, "INSERT INTO target VALUES (1, 'a'), (2, 'b')");

    // Matched rows are explicitly skipped; nothing inserted either.
    let tag = command_tag(run(
        &mut db,
        "MERGE INTO target t USING (VALUES (1), (2)) AS s(id) ON t.id = s.id \
         WHEN MATCHED THEN DO NOTHING \
         WHEN NOT MATCHED THEN DO NOTHING",
    ));
    assert_eq!(tag, "MERGE 0");

    let r = rows(run(&mut db, "SELECT id, val FROM target ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Text("a".into())],
            vec![Value::Int(2), Value::Text("b".into())],
        ]
    );
}

#[test]
fn merge_with_subquery_source() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE target (id integer PRIMARY KEY, total integer)");
    run(&mut db, "CREATE TABLE feed (id integer, amount integer)");
    run(&mut db, "INSERT INTO target VALUES (1, 0)");
    run(
        &mut db,
        "INSERT INTO feed VALUES (1, 5), (1, 7), (2, 9)",
    );

    let tag = command_tag(run(
        &mut db,
        "MERGE INTO target t \
         USING (SELECT id, sum(amount) AS amt FROM feed GROUP BY id) AS s ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET total = t.total + s.amt \
         WHEN NOT MATCHED THEN INSERT (id, total) VALUES (s.id, s.amt)",
    ));
    assert_eq!(tag, "MERGE 2");

    let r = rows(run(&mut db, "SELECT id, total FROM target ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Int(12)],
            vec![Value::Int(2), Value::Int(9)],
        ]
    );
}

// --- user-defined functions / triggers / rules / aggregates ----------------

#[test]
fn scalar_udf_called_in_query() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE FUNCTION my_add(a integer, b integer) RETURNS integer \
         AS $$ SELECT a + b $$ LANGUAGE sql",
    );
    run(&mut db, "CREATE TABLE t (x integer)");
    run(&mut db, "INSERT INTO t VALUES (10), (20)");
    let r = rows(run(&mut db, "SELECT my_add(x, 1) FROM t ORDER BY x"));
    assert_eq!(r, vec![vec![Value::Int(11)], vec![Value::Int(21)]]);
}

#[test]
fn scalar_udf_positional_params() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE FUNCTION double_it(integer) RETURNS integer \
         AS $$ SELECT $1 * 2 $$ LANGUAGE sql",
    );
    let r = rows(run(&mut db, "SELECT double_it(21)"));
    assert_eq!(r, vec![vec![Value::Int(42)]]);
}

#[test]
fn scalar_udf_or_replace_and_drop() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE FUNCTION f(a integer) RETURNS integer AS $$ SELECT a + 1 $$ LANGUAGE sql",
    );
    assert_eq!(rows(run(&mut db, "SELECT f(1)")), vec![vec![Value::Int(2)]]);
    run(
        &mut db,
        "CREATE OR REPLACE FUNCTION f(a integer) RETURNS integer AS $$ SELECT a + 100 $$ LANGUAGE sql",
    );
    assert_eq!(rows(run(&mut db, "SELECT f(1)")), vec![vec![Value::Int(101)]]);

    let r = run(&mut db, "DROP FUNCTION f(integer)");
    assert!(matches!(r, ExecResult::Command(ref t) if t == "DROP FUNCTION"));
    let err = Parser::parse_sql("SELECT f(1)")
        .map(|stmts| {
            let mut last = Ok(ExecResult::Empty);
            for s in stmts {
                last = executor::execute(&mut db, s);
            }
            last
        })
        .expect("parse");
    assert!(err.is_err(), "calling a dropped function should error");
}

#[test]
fn scalar_udf_string_body() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE FUNCTION shout(s text) RETURNS text AS $$ SELECT upper(s) $$ LANGUAGE sql",
    );
    assert_eq!(
        rows(run(&mut db, "SELECT shout('hi')")),
        vec![vec![Value::Text("HI".into())]]
    );
}

#[test]
fn trigger_fires_on_insert_with_observable_effect() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer)");
    run(&mut db, "CREATE TABLE audit (note text)");
    run(
        &mut db,
        "CREATE FUNCTION log_change() RETURNS trigger \
         AS $$ INSERT INTO audit (note) VALUES ('changed') $$ LANGUAGE sql",
    );
    run(
        &mut db,
        "CREATE TRIGGER trg AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION log_change()",
    );
    run(&mut db, "INSERT INTO t VALUES (1), (2), (3)");
    // One audit row per inserted row.
    let r = rows(run(&mut db, "SELECT count(*) FROM audit"));
    assert_eq!(r, vec![vec![Value::Int(3)]]);
}

#[test]
fn trigger_fires_on_update_and_delete() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer)");
    run(&mut db, "CREATE TABLE audit (note text)");
    run(
        &mut db,
        "CREATE FUNCTION log_change() RETURNS trigger \
         AS $$ INSERT INTO audit (note) VALUES ('x') $$ LANGUAGE sql",
    );
    run(
        &mut db,
        "CREATE TRIGGER trg_u AFTER UPDATE ON t FOR EACH ROW EXECUTE FUNCTION log_change()",
    );
    run(
        &mut db,
        "CREATE TRIGGER trg_d AFTER DELETE ON t FOR EACH ROW EXECUTE FUNCTION log_change()",
    );
    run(&mut db, "INSERT INTO t VALUES (1), (2)");
    run(&mut db, "UPDATE t SET id = id + 1");
    run(&mut db, "DELETE FROM t");
    // 2 updates + 2 deletes = 4 audit rows.
    assert_eq!(
        rows(run(&mut db, "SELECT count(*) FROM audit")),
        vec![vec![Value::Int(4)]]
    );
}

#[test]
fn drop_trigger() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer)");
    run(&mut db, "CREATE TABLE audit (note text)");
    run(
        &mut db,
        "CREATE FUNCTION log_change() RETURNS trigger \
         AS $$ INSERT INTO audit (note) VALUES ('x') $$ LANGUAGE sql",
    );
    run(
        &mut db,
        "CREATE TRIGGER trg AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION log_change()",
    );
    let r = run(&mut db, "DROP TRIGGER trg ON t");
    assert!(matches!(r, ExecResult::Command(ref t) if t == "DROP TRIGGER"));
    run(&mut db, "INSERT INTO t VALUES (1)");
    // No trigger now; audit stays empty.
    assert_eq!(
        rows(run(&mut db, "SELECT count(*) FROM audit")),
        vec![vec![Value::Int(0)]]
    );
}

#[test]
fn create_and_drop_rule() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer)");
    let r = run(
        &mut db,
        "CREATE RULE r AS ON INSERT TO t DO ALSO NOTHING",
    );
    assert!(matches!(r, ExecResult::Command(ref t) if t == "CREATE RULE"));
    let r = run(&mut db, "DROP RULE r ON t");
    assert!(matches!(r, ExecResult::Command(ref t) if t == "DROP RULE"));
}

#[test]
fn create_and_drop_aggregate() {
    let mut db = Database::new();
    let r = run(
        &mut db,
        "CREATE AGGREGATE my_sum(integer) (SFUNC = int4pl, STYPE = integer, INITCOND = '0')",
    );
    assert!(matches!(r, ExecResult::Command(ref t) if t == "CREATE AGGREGATE"));
    let r = run(&mut db, "DROP AGGREGATE my_sum(integer)");
    assert!(matches!(r, ExecResult::Command(ref t) if t == "DROP AGGREGATE"));
}

#[test]
fn udf_appears_in_pg_proc() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE FUNCTION my_special_fn(a integer) RETURNS integer \
         AS $$ SELECT a $$ LANGUAGE sql",
    );
    let r = rows(run(
        &mut db,
        "SELECT proname FROM pg_proc WHERE proname = 'my_special_fn'",
    ));
    assert_eq!(r, vec![vec![Value::Text("my_special_fn".into())]]);
}

#[test]
fn udf_body_needing_tables_errors_not_panics() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE src (v integer)");
    // Body has a FROM clause, so it is not registered as a scalar UDF; a call
    // surfaces a normal "does not exist" error rather than panicking.
    run(
        &mut db,
        "CREATE FUNCTION reads_table() RETURNS integer \
         AS $$ SELECT v FROM src LIMIT 1 $$ LANGUAGE sql",
    );
    let res = {
        let stmt = Parser::parse_sql("SELECT reads_table()").unwrap().remove(0);
        executor::execute(&mut db, stmt)
    };
    assert!(res.is_err());
}

#[test]
fn lateral_join_references_left_column() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, n integer)");
    run(&mut db, "INSERT INTO t VALUES (1, 10), (2, 20)");
    // LATERAL subquery references the left row's column `t.n`.
    let r = rows(run(
        &mut db,
        "SELECT t.id, s.v FROM t, LATERAL (SELECT t.n * 2 AS v) s ORDER BY t.id",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Int(20)],
            vec![Value::Int(2), Value::Int(40)],
        ]
    );
}

#[test]
fn lateral_join_over_generate_series() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, n integer)");
    run(&mut db, "INSERT INTO t VALUES (1, 1), (2, 3)");
    // LATERAL set-returning function whose arg references the left column.
    let r = rows(run(
        &mut db,
        "SELECT t.id, g.generate_series FROM t JOIN LATERAL generate_series(1, t.n) g ON true \
         ORDER BY t.id, g.generate_series",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Int(1)],
            vec![Value::Int(2), Value::Int(1)],
            vec![Value::Int(2), Value::Int(2)],
            vec![Value::Int(2), Value::Int(3)],
        ]
    );
}

#[test]
fn writable_cte_insert_returning_feeds_outer_select() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, name text)");
    let r = rows(run(
        &mut db,
        "WITH ins AS (INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b') RETURNING id, name) \
         SELECT id, name FROM ins ORDER BY id",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Text("a".into())],
            vec![Value::Int(2), Value::Text("b".into())],
        ]
    );
    // The rows were actually persisted by the writable CTE.
    let r = rows(run(&mut db, "SELECT count(*) FROM t"));
    assert_eq!(r, vec![vec![Value::Int(2)]]);
}

#[test]
fn writable_cte_delete_returning() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer)");
    run(&mut db, "INSERT INTO t VALUES (1), (2), (3)");
    let r = rows(run(
        &mut db,
        "WITH del AS (DELETE FROM t WHERE id > 1 RETURNING id) \
         SELECT id FROM del ORDER BY id",
    ));
    assert_eq!(r, vec![vec![Value::Int(2)], vec![Value::Int(3)]]);
    let r = rows(run(&mut db, "SELECT id FROM t"));
    assert_eq!(r, vec![vec![Value::Int(1)]]);
}

#[test]
fn jsonb_path_query_simple_path() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE j (doc jsonb)");
    run(
        &mut db,
        "INSERT INTO j VALUES ('{\"a\": {\"b\": 42}, \"arr\": [10, 20, 30]}')",
    );
    let r = rows(run(
        &mut db,
        "SELECT jsonb_path_query(doc, '$.a.b') FROM j",
    ));
    assert_eq!(r, vec![vec![Value::Text("42".into())]]);
    let r = rows(run(
        &mut db,
        "SELECT jsonb_path_query(doc, '$.arr[1]') FROM j",
    ));
    assert_eq!(r, vec![vec![Value::Text("20".into())]]);
    let r = rows(run(
        &mut db,
        "SELECT jsonb_path_exists(doc, '$.a.b'), jsonb_path_exists(doc, '$.a.z') FROM j",
    ));
    assert_eq!(r, vec![vec![Value::Bool(true), Value::Bool(false)]]);
}

#[test]
fn interval_literal_parses_and_normalizes() {
    let mut db = Database::new();
    let r = rows(run(
        &mut db,
        "SELECT INTERVAL '1 year 2 months 3 days'",
    ));
    assert_eq!(r, vec![vec![Value::Text("1 year 2 mons 3 days".into())]]);

    let r = rows(run(&mut db, "SELECT INTERVAL '2 hours 30 minutes'"));
    assert_eq!(r, vec![vec![Value::Text("02:30:00".into())]]);

    let r = rows(run(&mut db, "SELECT INTERVAL '1 day'"));
    assert_eq!(r, vec![vec![Value::Text("1 day".into())]]);

    let r = rows(run(&mut db, "SELECT INTERVAL '1-2' YEAR TO MONTH"));
    assert_eq!(r, vec![vec![Value::Text("1 year 2 mons".into())]]);
}

// --- table inheritance -------------------------------------------------------

#[test]
fn inheritance_parent_scan_includes_children() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE animal (id integer, name text)");
    run(
        &mut db,
        "CREATE TABLE dog (breed text) INHERITS (animal)",
    );
    // The child gets the parent's columns prepended to its own.
    run(&mut db, "INSERT INTO animal (id, name) VALUES (1, 'generic')");
    run(
        &mut db,
        "INSERT INTO dog (id, name, breed) VALUES (2, 'rex', 'lab')",
    );

    // Scanning the parent returns parent rows AND child rows (default).
    let r = rows(run(&mut db, "SELECT id, name FROM animal ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Text("generic".into())],
            vec![Value::Int(2), Value::Text("rex".into())],
        ]
    );

    // ONLY restricts to the parent's own rows.
    let r = rows(run(&mut db, "SELECT id, name FROM ONLY animal ORDER BY id"));
    assert_eq!(r, vec![vec![Value::Int(1), Value::Text("generic".into())]]);

    // The child has the parent's columns (prepended) plus its own.
    let r = rows(run(
        &mut db,
        "SELECT id, name, breed FROM dog ORDER BY id",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Int(2),
            Value::Text("rex".into()),
            Value::Text("lab".into())
        ]]
    );
}

#[test]
fn inheritance_multi_level_and_pg_inherits() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE base (a integer)");
    run(&mut db, "CREATE TABLE mid (b integer) INHERITS (base)");
    run(&mut db, "CREATE TABLE leaf (c integer) INHERITS (mid)");
    run(&mut db, "INSERT INTO base (a) VALUES (1)");
    run(&mut db, "INSERT INTO mid (a, b) VALUES (2, 20)");
    run(&mut db, "INSERT INTO leaf (a, b, c) VALUES (3, 30, 300)");

    // Scanning the root unions grandchildren too.
    let r = rows(run(&mut db, "SELECT a FROM base ORDER BY a"));
    assert_eq!(
        r,
        vec![vec![Value::Int(1)], vec![Value::Int(2)], vec![Value::Int(3)]]
    );

    // pg_inherits has one row per direct child→parent link.
    let r = rows(run(
        &mut db,
        "SELECT inhseqno FROM pg_inherits ORDER BY inhseqno",
    ));
    assert_eq!(r, vec![vec![Value::Int(1)], vec![Value::Int(1)]]);
}

// --- partitioned tables ------------------------------------------------------

#[test]
fn range_partition_routing_and_scan() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE measurement (id integer, val integer) PARTITION BY RANGE (val)",
    );
    run(
        &mut db,
        "CREATE TABLE measurement_low PARTITION OF measurement FOR VALUES FROM (0) TO (100)",
    );
    run(
        &mut db,
        "CREATE TABLE measurement_high PARTITION OF measurement FOR VALUES FROM (100) TO (200)",
    );

    run(&mut db, "INSERT INTO measurement (id, val) VALUES (1, 10), (2, 150)");

    // Each row routed to the right partition.
    let r = rows(run(
        &mut db,
        "SELECT id, val FROM measurement_low ORDER BY id",
    ));
    assert_eq!(r, vec![vec![Value::Int(1), Value::Int(10)]]);
    let r = rows(run(
        &mut db,
        "SELECT id, val FROM measurement_high ORDER BY id",
    ));
    assert_eq!(r, vec![vec![Value::Int(2), Value::Int(150)]]);

    // Parent scan unions all partitions.
    let r = rows(run(&mut db, "SELECT id, val FROM measurement ORDER BY id"));
    assert_eq!(
        r,
        vec![
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(2), Value::Int(150)],
        ]
    );

    // A value matching no partition errors.
    let err = Parser::parse_sql("INSERT INTO measurement (id, val) VALUES (3, 999)")
        .and_then(|mut s| executor::execute(&mut db, s.remove(0)).map(|_| ()))
        .expect_err("out-of-range insert should fail");
    assert_eq!(
        err,
        "no partition of relation \"measurement\" found for row"
    );
}

#[test]
fn list_partition_routing() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE sales (id integer, region text) PARTITION BY LIST (region)",
    );
    run(
        &mut db,
        "CREATE TABLE sales_eu PARTITION OF sales FOR VALUES IN ('de', 'fr')",
    );
    run(
        &mut db,
        "CREATE TABLE sales_us PARTITION OF sales FOR VALUES IN ('us')",
    );
    run(
        &mut db,
        "INSERT INTO sales (id, region) VALUES (1, 'de'), (2, 'us'), (3, 'fr')",
    );

    let r = rows(run(&mut db, "SELECT id FROM sales_eu ORDER BY id"));
    assert_eq!(r, vec![vec![Value::Int(1)], vec![Value::Int(3)]]);
    let r = rows(run(&mut db, "SELECT id FROM sales_us ORDER BY id"));
    assert_eq!(r, vec![vec![Value::Int(2)]]);
    let r = rows(run(&mut db, "SELECT id FROM sales ORDER BY id"));
    assert_eq!(
        r,
        vec![vec![Value::Int(1)], vec![Value::Int(2)], vec![Value::Int(3)]]
    );

    let err = Parser::parse_sql("INSERT INTO sales (id, region) VALUES (4, 'jp')")
        .and_then(|mut s| executor::execute(&mut db, s.remove(0)).map(|_| ()))
        .expect_err("unlisted value should fail");
    assert_eq!(err, "no partition of relation \"sales\" found for row");
}

#[test]
fn hash_partition_routing() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE h (id integer) PARTITION BY HASH (id)",
    );
    run(
        &mut db,
        "CREATE TABLE h0 PARTITION OF h FOR VALUES WITH (MODULUS 2, REMAINDER 0)",
    );
    run(
        &mut db,
        "CREATE TABLE h1 PARTITION OF h FOR VALUES WITH (MODULUS 2, REMAINDER 1)",
    );
    run(
        &mut db,
        "INSERT INTO h (id) VALUES (1), (2), (3), (4), (5)",
    );

    // Every row must land in exactly one of the two partitions; together they
    // reconstruct the full set, and the parent scan equals their union.
    let mut total = rows(run(&mut db, "SELECT id FROM h0 ORDER BY id"));
    total.extend(rows(run(&mut db, "SELECT id FROM h1 ORDER BY id")));
    total.sort_by_key(|r| match &r[0] {
        Value::Int(i) => *i,
        _ => 0,
    });
    assert_eq!(
        total,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)],
            vec![Value::Int(4)],
            vec![Value::Int(5)],
        ]
    );

    let parent = rows(run(&mut db, "SELECT id FROM h ORDER BY id"));
    assert_eq!(parent, total);
}

#[test]
fn plain_table_unaffected_by_partitioning_changes() {
    // A table with no parents / partitions scans exactly as before.
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE plain (id integer)");
    run(&mut db, "INSERT INTO plain VALUES (1), (2)");
    let r = rows(run(&mut db, "SELECT id FROM plain ORDER BY id"));
    assert_eq!(r, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    // ONLY on a plain table is a no-op.
    let r = rows(run(&mut db, "SELECT id FROM ONLY plain ORDER BY id"));
    assert_eq!(r, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
}

// --- extended DDL: accept + catalog/store (parse → tag → round-trip) ---------

use postgres_rs::sql::serialize::statement_to_sql;

/// Parse one statement and assert it serializes and re-parses to itself.
fn round_trip_one(sql: &str) {
    let stmt = Parser::parse_sql(sql)
        .expect("parse")
        .into_iter()
        .next()
        .expect("one statement");
    let serialized = statement_to_sql(&stmt);
    let reparsed = Parser::parse_sql(&serialized)
        .expect("reparse serialized")
        .into_iter()
        .next()
        .expect("one statement");
    assert_eq!(stmt, reparsed, "round-trip mismatch for: {sql}\n -> {serialized}");
}

fn cmd_tag(res: ExecResult) -> String {
    match res {
        ExecResult::Command(t) => t,
        other => panic!("expected command, got {}", tag_of(&other)),
    }
}

#[test]
fn copy_file_and_binary_forms_round_trip() {
    // STDIN/STDOUT forms (unchanged behavior).
    round_trip_one("COPY t FROM STDIN");
    round_trip_one("COPY t (a, b) TO STDOUT WITH (FORMAT csv, HEADER)");
    // File paths.
    round_trip_one("COPY t FROM '/tmp/in.csv' WITH (FORMAT csv)");
    round_trip_one("COPY t (a, b) TO '/var/data/out.dat'");
    // Binary format, for STDIN/STDOUT and files.
    round_trip_one("COPY t FROM STDIN WITH (FORMAT binary)");
    round_trip_one("COPY t TO '/tmp/out.bin' WITH (FORMAT binary)");
    // COPY of a query to a file.
    round_trip_one("COPY (SELECT id, name FROM t WHERE id > 1) TO '/tmp/q.csv' WITH (FORMAT csv)");
}

#[test]
fn inheritance_and_partition_clauses_round_trip() {
    round_trip_one("CREATE TABLE dog (breed text) INHERITS (animal)");
    round_trip_one("CREATE TABLE m (id integer, val integer) PARTITION BY RANGE (val)");
    round_trip_one("CREATE TABLE l (id integer, region text) PARTITION BY LIST (region)");
    round_trip_one("CREATE TABLE h (id integer) PARTITION BY HASH (id)");
    round_trip_one("CREATE TABLE m_lo PARTITION OF m FOR VALUES FROM (0) TO (100)");
    round_trip_one("CREATE TABLE l_eu PARTITION OF l FOR VALUES IN ('de', 'fr')");
    round_trip_one("CREATE TABLE h0 PARTITION OF h FOR VALUES WITH (MODULUS 2, REMAINDER 0)");
}

#[test]
fn exclusion_constraint_in_create_table_and_alter() {
    let mut db = Database::new();
    let tag = cmd_tag(run(
        &mut db,
        "CREATE TABLE rooms (id int, during int4range, EXCLUDE USING gist (during WITH &&))",
    ));
    assert_eq!(tag, "CREATE TABLE");
    let tag = cmd_tag(run(
        &mut db,
        "ALTER TABLE rooms ADD CONSTRAINT no_overlap EXCLUDE USING gist (during WITH &&)",
    ));
    assert_eq!(tag, "ALTER TABLE");
    // Dropping the named exclusion constraint succeeds.
    let tag = cmd_tag(run(&mut db, "ALTER TABLE rooms DROP CONSTRAINT no_overlap"));
    assert_eq!(tag, "ALTER TABLE");
    round_trip_one("CREATE TABLE r2 (id int, c int4range, EXCLUDE USING gist (c WITH &&))");
    round_trip_one("ALTER TABLE r2 ADD CONSTRAINT x EXCLUDE USING gist (c WITH &&)");
}

#[test]
fn deferrable_constraints_and_set_constraints() {
    let mut db = Database::new();
    let tag = cmd_tag(run(
        &mut db,
        "CREATE TABLE p (id int PRIMARY KEY DEFERRABLE INITIALLY DEFERRED)",
    ));
    assert_eq!(tag, "CREATE TABLE");
    let tag = cmd_tag(run(
        &mut db,
        "CREATE TABLE c (id int, pid int, \
         CONSTRAINT fk FOREIGN KEY (pid) REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED)",
    ));
    assert_eq!(tag, "CREATE TABLE");
    let tag = cmd_tag(run(&mut db, "SET CONSTRAINTS ALL DEFERRED"));
    assert_eq!(tag, "SET CONSTRAINTS");
    let tag = cmd_tag(run(&mut db, "SET CONSTRAINTS fk IMMEDIATE"));
    assert_eq!(tag, "SET CONSTRAINTS");
    round_trip_one(
        "CREATE TABLE c2 (id int, pid int, \
         CONSTRAINT fk FOREIGN KEY (pid) REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED)",
    );
}

#[test]
fn operator_classes_families_and_operators() {
    let mut db = Database::new();
    assert_eq!(
        cmd_tag(run(
            &mut db,
            "CREATE OPERATOR CLASS my_ops DEFAULT FOR TYPE int4 USING btree AS OPERATOR 1 <",
        )),
        "CREATE OPERATOR CLASS"
    );
    assert_eq!(
        cmd_tag(run(&mut db, "CREATE OPERATOR FAMILY my_fam USING btree")),
        "CREATE OPERATOR FAMILY"
    );
    assert_eq!(
        cmd_tag(run(
            &mut db,
            "CREATE OPERATOR === (LEFTARG = int4, RIGHTARG = int4, FUNCTION = int4eq)",
        )),
        "CREATE OPERATOR"
    );
    assert_eq!(
        cmd_tag(run(&mut db, "DROP OPERATOR === (int4, int4)")),
        "DROP OPERATOR"
    );
    assert_eq!(
        cmd_tag(run(&mut db, "DROP OPERATOR CLASS my_ops USING btree")),
        "DROP OPERATOR CLASS"
    );
    round_trip_one("CREATE OPERATOR FAMILY my_fam USING btree");
    round_trip_one("CREATE OPERATOR === (LEFTARG = int4, RIGHTARG = int4, FUNCTION = int4eq)");
}

#[test]
fn event_triggers() {
    let mut db = Database::new();
    assert_eq!(
        cmd_tag(run(
            &mut db,
            "CREATE EVENT TRIGGER et ON ddl_command_start EXECUTE FUNCTION snitch()",
        )),
        "CREATE EVENT TRIGGER"
    );
    assert_eq!(
        cmd_tag(run(&mut db, "DROP EVENT TRIGGER et")),
        "DROP EVENT TRIGGER"
    );
    round_trip_one("CREATE EVENT TRIGGER et ON ddl_command_start EXECUTE FUNCTION snitch()");
}

#[test]
fn foreign_data_wrappers_servers_mappings_and_tables() {
    let mut db = Database::new();
    assert_eq!(
        cmd_tag(run(
            &mut db,
            "CREATE FOREIGN DATA WRAPPER w HANDLER h OPTIONS (a 'b')",
        )),
        "CREATE FOREIGN DATA WRAPPER"
    );
    assert_eq!(
        cmd_tag(run(
            &mut db,
            "CREATE SERVER s FOREIGN DATA WRAPPER w OPTIONS (host 'localhost')",
        )),
        "CREATE SERVER"
    );
    assert_eq!(
        cmd_tag(run(
            &mut db,
            "CREATE USER MAPPING FOR postgres SERVER s OPTIONS (user 'a')",
        )),
        "CREATE USER MAPPING"
    );
    // A foreign table is stored like a regular table so it appears in catalogs.
    assert_eq!(
        cmd_tag(run(
            &mut db,
            "CREATE FOREIGN TABLE ft (id int, name text) SERVER s OPTIONS (tab 'x')",
        )),
        "CREATE TABLE"
    );
    let r = run(&mut db, "INSERT INTO ft (id, name) VALUES (1, 'a')");
    assert!(matches!(r, ExecResult::Command(ref t) if t == "INSERT 0 1"));
    assert_eq!(cmd_tag(run(&mut db, "DROP FOREIGN TABLE ft")), "DROP TABLE");
    assert_eq!(
        cmd_tag(run(&mut db, "DROP USER MAPPING FOR postgres SERVER s")),
        "DROP USER MAPPING"
    );
    assert_eq!(cmd_tag(run(&mut db, "DROP SERVER s")), "DROP SERVER");
    assert_eq!(
        cmd_tag(run(&mut db, "DROP FOREIGN DATA WRAPPER w")),
        "DROP FOREIGN DATA WRAPPER"
    );
    round_trip_one("CREATE SERVER s FOREIGN DATA WRAPPER w OPTIONS (host 'localhost')");
}

#[test]
fn publications_and_subscriptions() {
    let mut db = Database::new();
    assert_eq!(
        cmd_tag(run(&mut db, "CREATE PUBLICATION p FOR ALL TABLES")),
        "CREATE PUBLICATION"
    );
    assert_eq!(
        cmd_tag(run(
            &mut db,
            "CREATE SUBSCRIPTION sub CONNECTION 'host=x dbname=d' PUBLICATION p",
        )),
        "CREATE SUBSCRIPTION"
    );
    assert_eq!(
        cmd_tag(run(&mut db, "DROP SUBSCRIPTION sub")),
        "DROP SUBSCRIPTION"
    );
    assert_eq!(
        cmd_tag(run(&mut db, "DROP PUBLICATION IF EXISTS p")),
        "DROP PUBLICATION"
    );
    round_trip_one("CREATE PUBLICATION p FOR ALL TABLES");
    round_trip_one("CREATE SUBSCRIPTION sub CONNECTION 'host=x dbname=d' PUBLICATION p");
}

#[test]
fn replication_slot_functions() {
    let mut db = Database::new();
    let r = rows(run(
        &mut db,
        "SELECT pg_create_physical_replication_slot('slot1')",
    ));
    assert_eq!(r, vec![vec![Value::Text("slot1".into())]]);
    let r = rows(run(&mut db, "SELECT pg_drop_replication_slot('slot1')"));
    assert_eq!(r, vec![vec![Value::Null]]);
}

#[test]
fn two_phase_commit_statements_execute() {
    // Executed directly (autocommit / replay path), 2PC commands acknowledge.
    let mut db = Database::new();
    assert_eq!(
        cmd_tag(run(&mut db, "PREPARE TRANSACTION 'gid1'")),
        "PREPARE TRANSACTION"
    );
    assert_eq!(
        cmd_tag(run(&mut db, "COMMIT PREPARED 'gid1'")),
        "COMMIT PREPARED"
    );
    assert_eq!(
        cmd_tag(run(&mut db, "ROLLBACK PREPARED 'gid1'")),
        "ROLLBACK PREPARED"
    );
    round_trip_one("PREPARE TRANSACTION 'gid1'");
    round_trip_one("COMMIT PREPARED 'gid1'");
    round_trip_one("ROLLBACK PREPARED 'gid1'");
}

/// information_schema.columns exposes the full ORM/JDBC column metadata set.
#[test]
fn information_schema_columns_full_metadata() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE books (\
            id serial PRIMARY KEY, \
            title text NOT NULL DEFAULT 'untitled', \
            pages integer, \
            isbn text)",
    );
    let r = rows(run(
        &mut db,
        "SELECT column_name, ordinal_position, column_default, is_nullable, \
                data_type, udt_name, numeric_precision \
         FROM information_schema.columns \
         WHERE table_name = 'books' ORDER BY ordinal_position",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("id".into()),
                Value::Int(1),
                Value::Null,
                Value::Text("NO".into()),
                Value::Text("integer".into()),
                Value::Text("int4".into()),
                Value::Int(32),
            ],
            vec![
                Value::Text("title".into()),
                Value::Int(2),
                Value::Text("'untitled'".into()),
                Value::Text("NO".into()),
                Value::Text("text".into()),
                Value::Text("text".into()),
                Value::Null,
            ],
            vec![
                Value::Text("pages".into()),
                Value::Int(3),
                Value::Null,
                Value::Text("YES".into()),
                Value::Text("integer".into()),
                Value::Text("int4".into()),
                Value::Int(32),
            ],
            vec![
                Value::Text("isbn".into()),
                Value::Int(4),
                Value::Null,
                Value::Text("YES".into()),
                Value::Text("text".into()),
                Value::Text("text".into()),
                Value::Null,
            ],
        ]
    );
}

/// table_constraints / key_column_usage / referential_constraints /
/// constraint_column_usage cover the PK, UNIQUE and FK introspection ORMs run.
#[test]
fn information_schema_constraint_views() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE authors (id serial PRIMARY KEY, name text NOT NULL)",
    );
    run(
        &mut db,
        "CREATE TABLE books (\
            id serial PRIMARY KEY, \
            isbn text, \
            author_id integer, \
            CONSTRAINT books_author_id_fkey FOREIGN KEY (author_id) REFERENCES authors(id))",
    );
    run(
        &mut db,
        "CREATE UNIQUE INDEX books_isbn_idx ON books (isbn)",
    );

    let r = rows(run(
        &mut db,
        "SELECT constraint_name, constraint_type \
         FROM information_schema.table_constraints \
         WHERE table_name = 'books' ORDER BY constraint_name",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("books_author_id_fkey".into()),
                Value::Text("FOREIGN KEY".into()),
            ],
            vec![
                Value::Text("books_id_pkey".into()),
                Value::Text("PRIMARY KEY".into()),
            ],
            vec![
                Value::Text("books_isbn_idx".into()),
                Value::Text("UNIQUE".into()),
            ],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT constraint_name, column_name, ordinal_position \
         FROM information_schema.key_column_usage \
         WHERE table_name = 'books' ORDER BY constraint_name",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("books_author_id_fkey".into()),
                Value::Text("author_id".into()),
                Value::Int(1),
            ],
            vec![
                Value::Text("books_id_pkey".into()),
                Value::Text("id".into()),
                Value::Int(1),
            ],
            vec![
                Value::Text("books_isbn_idx".into()),
                Value::Text("isbn".into()),
                Value::Int(1),
            ],
        ]
    );

    let r = rows(run(
        &mut db,
        "SELECT constraint_name, unique_constraint_name, update_rule, delete_rule \
         FROM information_schema.referential_constraints",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("books_author_id_fkey".into()),
            Value::Text("authors_id_pkey".into()),
            Value::Text("NO ACTION".into()),
            Value::Text("NO ACTION".into()),
        ]]
    );

    // The FK's constraint_column_usage points at the *referenced* parent column.
    let r = rows(run(
        &mut db,
        "SELECT table_name, column_name \
         FROM information_schema.constraint_column_usage \
         WHERE constraint_name = 'books_author_id_fkey'",
    ));
    assert_eq!(
        r,
        vec![vec![Value::Text("authors".into()), Value::Text("id".into())]]
    );
}

/// schemata, views and sequences views report live catalog objects.
#[test]
fn information_schema_schemata_views_sequences() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer)");
    run(&mut db, "CREATE VIEW v AS SELECT id FROM t");
    run(&mut db, "CREATE SEQUENCE s START 5 INCREMENT 2");

    let r = rows(run(
        &mut db,
        "SELECT schema_name FROM information_schema.schemata \
         WHERE schema_name = 'public'",
    ));
    assert_eq!(r, vec![vec![Value::Text("public".into())]]);

    let r = rows(run(
        &mut db,
        "SELECT table_name FROM information_schema.views",
    ));
    assert_eq!(r, vec![vec![Value::Text("v".into())]]);

    let r = rows(run(
        &mut db,
        "SELECT sequence_name, data_type, start_value, increment \
         FROM information_schema.sequences",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("s".into()),
            Value::Text("bigint".into()),
            Value::Text("5".into()),
            Value::Text("2".into()),
        ]]
    );
}

/// The pg_attribute column list psql `\d <table>` issues (with format_type)
/// returns each column's resolved type name and nullability.
#[test]
fn pg_attribute_column_list_with_format_type() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE widget (id integer NOT NULL, label text, qty bigint)",
    );
    let r = rows(run(
        &mut db,
        "SELECT a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull \
         FROM pg_catalog.pg_attribute a \
         JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
         WHERE c.relname = 'widget' AND a.attnum > 0 AND NOT a.attisdropped \
         ORDER BY a.attnum",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("id".into()),
                Value::Text("integer".into()),
                Value::Bool(true),
            ],
            vec![
                Value::Text("label".into()),
                Value::Text("text".into()),
                Value::Bool(false),
            ],
            vec![
                Value::Text("qty".into()),
                Value::Text("bigint".into()),
                Value::Bool(false),
            ],
        ]
    );
}

/// Exercises the catalog queries `psql`'s `\d <table>` issues end to end: the
/// pg_class metadata probe (resolving `'name'::regclass`), the pg_attribute
/// column list with `format_type`, the index/constraint section with
/// `pg_get_indexdef`/`pg_get_constraintdef`, and the foreign-key footer. This
/// regression-protects `\d` support without needing a live `psql`.
#[test]
fn describe_table_catalog_queries() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE owners (id integer PRIMARY KEY)");
    run(
        &mut db,
        "CREATE TABLE t ( \
            id integer PRIMARY KEY, \
            name text NOT NULL DEFAULT 'x', \
            email text, \
            owner integer, \
            CONSTRAINT t_email_key UNIQUE (email), \
            CONSTRAINT fk_owner FOREIGN KEY (owner) REFERENCES owners(id), \
            CONSTRAINT t_name_chk CHECK (length(name) > 0) \
        )",
    );

    // pg_class metadata probe, resolving the relation OID via `::regclass`.
    let r = rows(run(
        &mut db,
        "SELECT relkind, relhasindex, relchecks, relhastriggers, relpersistence \
         FROM pg_catalog.pg_class WHERE oid = 't'::pg_catalog.regclass",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("r".into()),
            Value::Bool(true),
            Value::Int(1),
            // FK constraints surface as triggers so psql probes the FK footer.
            Value::Bool(true),
            Value::Text("p".into()),
        ]]
    );

    // pg_attribute column list with format_type + atthasdef + attnotnull.
    let r = rows(run(
        &mut db,
        "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), \
                a.attnotnull, a.atthasdef \
         FROM pg_catalog.pg_attribute a \
         WHERE a.attrelid = 't'::pg_catalog.regclass \
           AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("id".into()),
                Value::Text("integer".into()),
                Value::Bool(true),
                Value::Bool(false),
            ],
            vec![
                Value::Text("name".into()),
                Value::Text("text".into()),
                Value::Bool(true),
                Value::Bool(true),
            ],
            vec![
                Value::Text("email".into()),
                Value::Text("text".into()),
                Value::Bool(false),
                Value::Bool(false),
            ],
            vec![
                Value::Text("owner".into()),
                Value::Text("integer".into()),
                Value::Bool(false),
                Value::Bool(false),
            ],
        ]
    );

    // Index/constraint section: primary key + unique index, with rendered defs.
    let r = rows(run(
        &mut db,
        "SELECT c2.relname, i.indisprimary, i.indisunique, \
                pg_catalog.pg_get_indexdef(i.indexrelid, 0, true), \
                pg_catalog.pg_get_constraintdef(con.oid, true), con.contype \
         FROM pg_catalog.pg_class c, pg_catalog.pg_class c2, pg_catalog.pg_index i \
         LEFT JOIN pg_catalog.pg_constraint con \
           ON (con.conrelid = i.indrelid AND con.conindid = i.indexrelid \
               AND con.contype IN ('p','u','x')) \
         WHERE c.oid = 't'::pg_catalog.regclass AND c.oid = i.indrelid \
           AND i.indexrelid = c2.oid \
         ORDER BY i.indisprimary DESC, c2.relname",
    ));
    assert_eq!(
        r,
        vec![
            vec![
                Value::Text("t_id_pkey".into()),
                Value::Bool(true),
                Value::Bool(true),
                Value::Text("CREATE UNIQUE INDEX t_id_pkey ON public.t USING btree (id)".into()),
                Value::Text("PRIMARY KEY (id)".into()),
                Value::Text("p".into()),
            ],
            vec![
                Value::Text("t_email_key".into()),
                Value::Bool(false),
                Value::Bool(true),
                Value::Text(
                    "CREATE UNIQUE INDEX t_email_key ON public.t USING btree (email)".into(),
                ),
                Value::Text("UNIQUE (email)".into()),
                Value::Text("u".into()),
            ],
        ]
    );

    // Check-constraint footer.
    let r = rows(run(
        &mut db,
        "SELECT r.conname, pg_catalog.pg_get_constraintdef(r.oid, true) \
         FROM pg_catalog.pg_constraint r \
         WHERE r.conrelid = 't'::pg_catalog.regclass AND r.contype = 'c' ORDER BY 1",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("t_name_chk".into()),
            Value::Text("CHECK ((length(name) > 0))".into()),
        ]]
    );

    // Foreign-key footer, including `conrelid::regclass` rendering the table
    // name back from the OID.
    let r = rows(run(
        &mut db,
        "SELECT conname, conrelid::pg_catalog.regclass, \
                pg_catalog.pg_get_constraintdef(r.oid, true) \
         FROM pg_catalog.pg_constraint r \
         WHERE r.conrelid = 't'::pg_catalog.regclass AND r.contype = 'f' \
           AND conparentid = 0 ORDER BY conname",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("fk_owner".into()),
            Value::Text("t".into()),
            Value::Text("FOREIGN KEY (owner) REFERENCES owners(id)".into()),
        ]]
    );

    // "Referenced by" footer for the target table (owners): confrelid points
    // back to owners' OID.
    let r = rows(run(
        &mut db,
        "SELECT conname, conrelid::pg_catalog.regclass \
         FROM pg_catalog.pg_constraint \
         WHERE confrelid = 'owners'::pg_catalog.regclass AND contype = 'f' ORDER BY conname",
    ));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("fk_owner".into()),
            Value::Text("t".into()),
        ]]
    );
}

// --- runtime configuration parameters (GUCs) ---------------------------------

/// Extract the single text value of a one-row, one-column result.
fn scalar_text(res: ExecResult) -> String {
    let r = rows(res);
    assert_eq!(r.len(), 1, "expected exactly one row");
    match &r[0][0] {
        Value::Text(s) => s.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn set_and_show_custom_guc_round_trips() {
    let mut db = Database::new();
    run(&mut db, "SET myapp.feature = 'enabled'");
    assert_eq!(scalar_text(run(&mut db, "SHOW myapp.feature")), "enabled");

    // SET ... TO ... and re-reading the new value.
    run(&mut db, "SET myapp.feature TO 'disabled'");
    assert_eq!(scalar_text(run(&mut db, "SHOW myapp.feature")), "disabled");
}

#[test]
fn show_standard_guc_defaults() {
    let mut db = Database::new();
    assert_eq!(scalar_text(run(&mut db, "SHOW client_encoding")), "UTF8");
    assert_eq!(scalar_text(run(&mut db, "SHOW statement_timeout")), "0");
    assert_eq!(scalar_text(run(&mut db, "SHOW TimeZone")), "UTC");
    // search_path keeps its dedicated default.
    assert_eq!(
        scalar_text(run(&mut db, "SHOW search_path")),
        "$user, public"
    );
}

#[test]
fn show_unknown_guc_errors() {
    let mut db = Database::new();
    let stmts = Parser::parse_sql("SHOW does_not_exist").unwrap();
    let err = executor::execute(&mut db, stmts.into_iter().next().unwrap());
    assert!(err.is_err());
}

#[test]
fn current_setting_and_set_config_round_trip() {
    let mut db = Database::new();
    // set_config writes the parameter and returns the value.
    assert_eq!(
        scalar_text(run(&mut db, "SELECT set_config('myapp.x', 'v1', false)")),
        "v1"
    );
    // current_setting reads it back (must be a separate statement so the write
    // has been flushed to the database).
    assert_eq!(
        scalar_text(run(&mut db, "SELECT current_setting('myapp.x')")),
        "v1"
    );
    // SHOW sees the same value.
    assert_eq!(scalar_text(run(&mut db, "SHOW myapp.x")), "v1");

    // current_setting of a known default.
    assert_eq!(
        scalar_text(run(&mut db, "SELECT current_setting('client_encoding')")),
        "UTF8"
    );
}

#[test]
fn current_setting_missing_ok() {
    let mut db = Database::new();
    // Without missing_ok an unknown parameter errors.
    let stmts = Parser::parse_sql("SELECT current_setting('nope.nope')").unwrap();
    assert!(executor::execute(&mut db, stmts.into_iter().next().unwrap()).is_err());
    // With missing_ok = true it returns NULL.
    let r = rows(run(&mut db, "SELECT current_setting('nope.nope', true)"));
    assert_eq!(r, vec![vec![Value::Null]]);
}

#[test]
fn reset_restores_default() {
    let mut db = Database::new();
    run(&mut db, "SET statement_timeout = '5000'");
    assert_eq!(scalar_text(run(&mut db, "SHOW statement_timeout")), "5000");
    run(&mut db, "RESET statement_timeout");
    assert_eq!(scalar_text(run(&mut db, "SHOW statement_timeout")), "0");

    // SET ... TO DEFAULT is equivalent to RESET.
    run(&mut db, "SET statement_timeout = '7000'");
    run(&mut db, "SET statement_timeout TO DEFAULT");
    assert_eq!(scalar_text(run(&mut db, "SHOW statement_timeout")), "0");

    // Custom GUC: RESET removes it (it has no built-in default, so SHOW errors).
    run(&mut db, "SET myapp.k = 'v'");
    run(&mut db, "RESET myapp.k");
    let stmts = Parser::parse_sql("SHOW myapp.k").unwrap();
    assert!(executor::execute(&mut db, stmts.into_iter().next().unwrap()).is_err());
}

#[test]
fn reset_all_clears_settings_and_search_path() {
    let mut db = Database::new();
    run(&mut db, "SET search_path = myschema, public");
    run(&mut db, "SET myapp.k = 'v'");
    run(&mut db, "RESET ALL");
    assert_eq!(
        scalar_text(run(&mut db, "SHOW search_path")),
        "$user, public"
    );
    let stmts = Parser::parse_sql("SHOW myapp.k").unwrap();
    assert!(executor::execute(&mut db, stmts.into_iter().next().unwrap()).is_err());
}

#[test]
fn show_all_returns_setting_rows() {
    let mut db = Database::new();
    run(&mut db, "SET myapp.custom = 'hello'");
    let r = rows(run(&mut db, "SHOW ALL"));
    // Three columns: name, setting, description.
    assert!(r.iter().all(|row| row.len() == 3));
    // The custom setting appears with its value.
    let custom = r
        .iter()
        .find(|row| row[0] == Value::Text("myapp.custom".into()))
        .expect("custom setting present in SHOW ALL");
    assert_eq!(custom[1], Value::Text("hello".into()));
    // A standard GUC is present too.
    assert!(r
        .iter()
        .any(|row| row[0] == Value::Text("client_encoding".into())));
}

#[test]
fn pg_settings_reflects_custom_and_timeout_gucs() {
    let mut db = Database::new();
    run(&mut db, "SET myapp.flag = 'on'");
    run(&mut db, "SET statement_timeout = '1234'");
    let r = rows(run(
        &mut db,
        "SELECT name, setting FROM pg_catalog.pg_settings \
         WHERE name IN ('myapp.flag', 'statement_timeout') ORDER BY name",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("myapp.flag".into()), Value::Text("on".into())],
            vec![
                Value::Text("statement_timeout".into()),
                Value::Text("1234".into())
            ],
        ]
    );
}

#[test]
fn array_column_insert_subscript_length_and_agg() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t (id integer, vals integer[], tags text[])",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (1, ARRAY[10,20,30], ARRAY['a','b']), \
         (2, ARRAY[40,50], ARRAY['c'])",
    );

    // Round-trip the stored array text.
    let r = rows(run(&mut db, "SELECT vals FROM t WHERE id = 1"));
    assert_eq!(r, vec![vec![Value::Text("{10,20,30}".into())]]);

    // 1-based element subscript.
    let r = rows(run(&mut db, "SELECT vals[1], vals[3] FROM t WHERE id = 1"));
    assert_eq!(
        r,
        vec![vec![Value::Text("10".into()), Value::Text("30".into())]]
    );
    // Out-of-range subscript yields NULL.
    let r = rows(run(&mut db, "SELECT vals[9] FROM t WHERE id = 1"));
    assert_eq!(r, vec![vec![Value::Null]]);

    // array_length / cardinality.
    let r = rows(run(
        &mut db,
        "SELECT array_length(vals, 1), cardinality(tags) FROM t WHERE id = 1",
    ));
    assert_eq!(r, vec![vec![Value::Int(3), Value::Int(2)]]);

    // array_agg over a grouped/whole-table aggregate.
    let r = rows(run(&mut db, "SELECT array_agg(id) FROM t"));
    assert_eq!(r, vec![vec![Value::Text("{1,2}".into())]]);
}

#[test]
fn unnest_in_from_and_select() {
    let mut db = Database::new();
    // unnest as a set-returning function in FROM.
    let r = rows(run(
        &mut db,
        "SELECT unnest FROM unnest(ARRAY[100,200,300]) AS x ORDER BY unnest",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("100".into())],
            vec![Value::Text("200".into())],
            vec![Value::Text("300".into())],
        ]
    );

    // unnest in the SELECT list (set-returning projection).
    let r = rows(run(&mut db, "SELECT unnest(ARRAY['a','b','c'])"));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("a".into())],
            vec![Value::Text("b".into())],
            vec![Value::Text("c".into())],
        ]
    );
}

#[test]
fn range_constructors_operators_and_bounds() {
    let mut db = Database::new();

    // Constructor produces canonical [lo,hi) text.
    let r = rows(run(&mut db, "SELECT int4range(1, 5)"));
    assert_eq!(r, vec![vec![Value::Text("[1,5)".into())]]);

    // lower() / upper() on a range.
    let r = rows(run(&mut db, "SELECT lower(int4range(1, 5)), upper(int4range(1, 5))"));
    assert_eq!(r, vec![vec![Value::Int(1), Value::Int(5)]]);

    // @> element containment (5 is excluded, 4 included).
    let r = rows(run(
        &mut db,
        "SELECT int4range(1, 5) @> 4, int4range(1, 5) @> 5",
    ));
    assert_eq!(r, vec![vec![Value::Bool(true), Value::Bool(false)]]);

    // @> range containment and <@.
    let r = rows(run(
        &mut db,
        "SELECT int4range(1, 10) @> int4range(2, 5), \
         int4range(2, 5) <@ int4range(1, 10)",
    ));
    assert_eq!(r, vec![vec![Value::Bool(true), Value::Bool(true)]]);

    // && overlap.
    let r = rows(run(
        &mut db,
        "SELECT int4range(1, 5) && int4range(4, 9), \
         int4range(1, 5) && int4range(5, 9)",
    ));
    assert_eq!(r, vec![vec![Value::Bool(true), Value::Bool(false)]]);
}

#[test]
fn range_column_and_multirange_accept() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE r (id integer, span int4range)");
    run(&mut db, "INSERT INTO r VALUES (1, int4range(10, 20))");
    let r = rows(run(&mut db, "SELECT span FROM r WHERE id = 1"));
    assert_eq!(r, vec![vec![Value::Text("[10,20)".into())]]);

    // Containment against a stored range column.
    let r = rows(run(
        &mut db,
        "SELECT id FROM r WHERE span @> 15",
    ));
    assert_eq!(r, vec![vec![Value::Int(1)]]);

    // Multirange constructor accepted and stored as text.
    let r = rows(run(
        &mut db,
        "SELECT int4multirange(int4range(1, 5), int4range(8, 10))",
    ));
    assert_eq!(
        r,
        vec![vec![Value::Text("{[1,5), [8,10)}".into())]]
    );
}

#[test]
fn geometric_column_round_trip_and_pg_type() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE g (id integer, p point, b box)",
    );
    run(&mut db, "INSERT INTO g VALUES (1, '(1,2)', '(0,0),(3,4)')");
    let r = rows(run(&mut db, "SELECT p, b FROM g WHERE id = 1"));
    assert_eq!(
        r,
        vec![vec![
            Value::Text("(1,2)".into()),
            Value::Text("(0,0),(3,4)".into())
        ]]
    );

    // point(x, y) constructor.
    let r = rows(run(&mut db, "SELECT point(5, 6)"));
    assert_eq!(r, vec![vec![Value::Text("(5,6)".into())]]);

    // Geometric and range types appear in pg_type.
    let r = rows(run(
        &mut db,
        "SELECT typname FROM pg_catalog.pg_type \
         WHERE typname IN ('point', 'int4range') ORDER BY typname",
    ));
    assert_eq!(
        r,
        vec![
            vec![Value::Text("int4range".into())],
            vec![Value::Text("point".into())],
        ]
    );
}

// --- ownership / security definer / row-level security ----------------------

/// A scalar Value helper: extract the single cell of a single-row result.
fn one(res: ExecResult) -> Value {
    let r = rows(res);
    assert_eq!(r.len(), 1, "expected exactly one row");
    assert_eq!(r[0].len(), 1, "expected exactly one column");
    r[0][0].clone()
}

#[test]
fn pg_class_relowner_reflects_owner_and_alter_owner_to() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE owned (id integer)");
    // Default owner is the 'postgres' superuser, OID 10.
    let owner = one(run(
        &mut db,
        "SELECT relowner FROM pg_class WHERE relname = 'owned'",
    ));
    assert_eq!(owner, Value::Int(10));

    // Create a new role and transfer ownership; relowner reflects its OID.
    run(&mut db, "CREATE ROLE alice");
    run(&mut db, "ALTER TABLE owned OWNER TO alice");
    let alice_oid = one(run(
        &mut db,
        "SELECT oid FROM pg_roles WHERE rolname = 'alice'",
    ));
    let owner = one(run(
        &mut db,
        "SELECT relowner FROM pg_class WHERE relname = 'owned'",
    ));
    assert_eq!(owner, alice_oid);
    assert_ne!(owner, Value::Int(10));

    // ALTER TABLE ... OWNER TO a nonexistent role errors.
    let stmts = Parser::parse_sql("ALTER TABLE owned OWNER TO ghost").expect("parse");
    let err = executor::execute(&mut db, stmts.into_iter().next().unwrap());
    assert!(err.is_err());
}

#[test]
fn pg_proc_prosecdef_records_security_definer() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE FUNCTION sd() RETURNS integer AS $$ SELECT 1 $$ LANGUAGE sql SECURITY DEFINER",
    );
    run(
        &mut db,
        "CREATE FUNCTION si() RETURNS integer AS $$ SELECT 2 $$ LANGUAGE sql SECURITY INVOKER",
    );
    let secdef = one(run(
        &mut db,
        "SELECT prosecdef FROM pg_proc WHERE proname = 'sd'",
    ));
    assert_eq!(secdef, Value::Bool(true));
    let invoker = one(run(
        &mut db,
        "SELECT prosecdef FROM pg_proc WHERE proname = 'si'",
    ));
    assert_eq!(invoker, Value::Bool(false));
}

#[test]
fn row_level_security_flag_and_pg_policy() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE secured (id integer, tenant text)");
    // RLS off by default.
    let rls = one(run(
        &mut db,
        "SELECT relrowsecurity FROM pg_class WHERE relname = 'secured'",
    ));
    assert_eq!(rls, Value::Bool(false));

    run(&mut db, "ALTER TABLE secured ENABLE ROW LEVEL SECURITY");
    let rls = one(run(
        &mut db,
        "SELECT relrowsecurity FROM pg_class WHERE relname = 'secured'",
    ));
    assert_eq!(rls, Value::Bool(true));

    run(&mut db, "ALTER TABLE secured FORCE ROW LEVEL SECURITY");
    let forced = one(run(
        &mut db,
        "SELECT relforcerowsecurity FROM pg_class WHERE relname = 'secured'",
    ));
    assert_eq!(forced, Value::Bool(true));

    // CREATE POLICY populates pg_policy.
    run(
        &mut db,
        "CREATE POLICY tenant_isolation ON secured FOR SELECT USING (tenant = 'acme')",
    );
    let r = rows(run(
        &mut db,
        "SELECT polname, polcmd, polpermissive FROM pg_policy WHERE polname = 'tenant_isolation'",
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("tenant_isolation".into()));
    assert_eq!(r[0][1], Value::Text("r".into())); // SELECT
    assert_eq!(r[0][2], Value::Bool(true)); // permissive

    // ALTER POLICY and DROP POLICY.
    run(
        &mut db,
        "ALTER POLICY tenant_isolation ON secured USING (tenant = 'globex')",
    );
    let qual = one(run(
        &mut db,
        "SELECT polqual FROM pg_policy WHERE polname = 'tenant_isolation'",
    ));
    assert!(matches!(qual, Value::Text(ref s) if s.contains("globex")));

    run(&mut db, "DROP POLICY tenant_isolation ON secured");
    let r = rows(run(
        &mut db,
        "SELECT polname FROM pg_policy WHERE polrelid = (SELECT oid FROM pg_class WHERE relname = 'secured')",
    ));
    assert_eq!(r.len(), 0);

    run(&mut db, "ALTER TABLE secured DISABLE ROW LEVEL SECURITY");
    let rls = one(run(
        &mut db,
        "SELECT relrowsecurity FROM pg_class WHERE relname = 'secured'",
    ));
    assert_eq!(rls, Value::Bool(false));
}
