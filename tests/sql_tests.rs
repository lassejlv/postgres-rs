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
    let r = run(&mut db, "INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b')");
    assert!(matches!(r, ExecResult::Command(ref t) if t == "INSERT 0 2"));

    let r = rows(run(&mut db, "SELECT id, name FROM t ORDER BY id"));
    assert_eq!(r, vec![
        vec![Value::Int(1), Value::Text("a".into())],
        vec![Value::Int(2), Value::Text("b".into())],
    ]);
}

#[test]
fn where_and_ordering() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE n (x integer)");
    run(&mut db, "INSERT INTO n VALUES (5), (1), (3), (2), (4)");
    let r = rows(run(&mut db, "SELECT x FROM n WHERE x > 2 ORDER BY x DESC"));
    let got: Vec<i64> = r.iter().map(|row| match row[0] {
        Value::Int(i) => i,
        _ => panic!(),
    }).collect();
    assert_eq!(got, vec![5, 4, 3]);
}

#[test]
fn aggregates() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE s (v integer)");
    run(&mut db, "INSERT INTO s VALUES (10), (20), (30)");
    let r = rows(run(&mut db, "SELECT count(*), sum(v), min(v), max(v) FROM s"));
    assert_eq!(r[0], vec![
        Value::Int(3),
        Value::Int(60),
        Value::Int(10),
        Value::Int(30),
    ]);

    let r = rows(run(&mut db, "SELECT avg(v) FROM s"));
    assert_eq!(r[0], vec![Value::Float(20.0)]);
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
fn expressions_and_functions() {
    let mut db = Database::new();
    let r = rows(run(&mut db, "SELECT 1 + 2 * 3, upper('hi'), 'a' || 'b', 10 / 3, 10.0 / 4"));
    assert_eq!(r[0], vec![
        Value::Int(7),
        Value::Text("HI".into()),
        Value::Text("ab".into()),
        Value::Int(3),
        Value::Float(2.5),
    ]);
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
    let got: Vec<i64> = r.iter().map(|row| match row[0] { Value::Int(i) => i, _ => panic!() }).collect();
    assert_eq!(got, vec![2, 3]);
}

#[test]
fn group_by_and_having() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE sales (region text, amount integer)");
    run(&mut db, "INSERT INTO sales VALUES ('w', 100), ('w', 200), ('e', 50), ('e', 25)");

    // GROUP BY with ORDER BY on an aggregate alias.
    let r = rows(run(
        &mut db,
        "SELECT region, sum(amount) AS total FROM sales GROUP BY region ORDER BY total DESC",
    ));
    assert_eq!(r, vec![
        vec![Value::Text("w".into()), Value::Int(300)],
        vec![Value::Text("e".into()), Value::Int(75)],
    ]);

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
    let got: Vec<i64> = r.iter().map(|row| match row[0] { Value::Int(i) => i, _ => panic!() }).collect();
    assert_eq!(got, vec![30, 20, 10]);
}

#[test]
fn inner_and_left_join() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE users (id integer, name text)");
    run(&mut db, "CREATE TABLE orders (id integer, user_id integer, amount integer)");
    run(&mut db, "INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')");
    run(&mut db, "INSERT INTO orders VALUES (10, 1, 100), (11, 1, 50), (12, 2, 200)");

    // INNER JOIN excludes Carol (no orders).
    let r = rows(run(
        &mut db,
        "SELECT u.name, o.amount FROM users u INNER JOIN orders o ON u.id = o.user_id ORDER BY o.amount",
    ));
    assert_eq!(r, vec![
        vec![Value::Text("Alice".into()), Value::Int(50)],
        vec![Value::Text("Alice".into()), Value::Int(100)],
        vec![Value::Text("Bob".into()), Value::Int(200)],
    ]);

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
    assert_eq!(r, vec![
        vec![Value::Text("Alice".into()), Value::Int(2), Value::Int(150)],
        vec![Value::Text("Bob".into()), Value::Int(1), Value::Int(200)],
        vec![Value::Text("Carol".into()), Value::Int(0), Value::Null],
    ]);
}

#[test]
fn select_distinct() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (region text, n integer)");
    run(&mut db, "INSERT INTO t VALUES ('w',1),('w',2),('e',3),('w',1)");

    let r = rows(run(&mut db, "SELECT DISTINCT region FROM t ORDER BY region"));
    assert_eq!(r, vec![
        vec![Value::Text("e".into())],
        vec![Value::Text("w".into())],
    ]);

    // The duplicate ('w', 1) collapses to a single row.
    let r = rows(run(&mut db, "SELECT DISTINCT region, n FROM t"));
    assert_eq!(r.len(), 3);
}

#[test]
fn like_in_between_case() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE p (id integer, name text, price integer)");
    run(&mut db, "INSERT INTO p VALUES (1,'Apple',100),(2,'Apricot',150),(3,'Banana',80),(4,'Cherry',200)");

    let names = |r: Vec<Vec<Value>>| -> Vec<String> {
        r.into_iter().map(|row| match &row[0] { Value::Text(s) => s.clone(), _ => panic!() }).collect()
    };

    assert_eq!(names(rows(run(&mut db, "SELECT name FROM p WHERE name LIKE 'Ap%' ORDER BY name"))),
               vec!["Apple", "Apricot"]);
    assert_eq!(names(rows(run(&mut db, "SELECT name FROM p WHERE name ILIKE 'b%'"))),
               vec!["Banana"]);
    assert_eq!(names(rows(run(&mut db, "SELECT name FROM p WHERE name LIKE '_pple'"))),
               vec!["Apple"]);
    assert_eq!(names(rows(run(&mut db, "SELECT name FROM p WHERE price BETWEEN 90 AND 160 ORDER BY id"))),
               vec!["Apple", "Apricot"]);
    assert_eq!(names(rows(run(&mut db, "SELECT name FROM p WHERE id IN (1, 3) ORDER BY id"))),
               vec!["Apple", "Banana"]);
    assert_eq!(names(rows(run(&mut db, "SELECT name FROM p WHERE id NOT IN (1, 3) ORDER BY id"))),
               vec!["Apricot", "Cherry"]);

    // Searched CASE.
    let r = rows(run(&mut db, "SELECT CASE WHEN price >= 150 THEN 'hi' WHEN price >= 90 THEN 'mid' ELSE 'lo' END FROM p ORDER BY id"));
    assert_eq!(names(r), vec!["mid", "hi", "lo", "hi"]);

    // Simple CASE.
    let r = rows(run(&mut db, "SELECT CASE id WHEN 1 THEN 'one' ELSE 'other' END FROM p ORDER BY id"));
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
    let r = rows(run(&mut db, "SELECT a.label, b.note FROM a RIGHT JOIN b ON a.id = b.id ORDER BY b.id"));
    assert_eq!(r, vec![
        vec![Value::Text("a2".into()), Value::Text("b2".into())],
        vec![Value::Text("a3".into()), Value::Text("b3".into())],
        vec![Value::Null, Value::Text("b4".into())],
    ]);

    // FULL JOIN keeps unmatched rows from both sides.
    let r = rows(run(&mut db, "SELECT count(*) FROM a FULL JOIN b ON a.id = b.id"));
    assert_eq!(r[0][0], Value::Int(4));

    // CROSS JOIN is the cartesian product.
    let r = rows(run(&mut db, "SELECT count(*) FROM a CROSS JOIN b"));
    assert_eq!(r[0][0], Value::Int(9));
}

#[test]
fn returning_clause() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, name text, v integer)");

    let r = rows(run(&mut db, "INSERT INTO t VALUES (1,'a',10),(2,'b',20) RETURNING id, name"));
    assert_eq!(r, vec![
        vec![Value::Int(1), Value::Text("a".into())],
        vec![Value::Int(2), Value::Text("b".into())],
    ]);

    let r = rows(run(&mut db, "UPDATE t SET v = v * 10 WHERE id = 1 RETURNING v"));
    assert_eq!(r, vec![vec![Value::Int(100)]]);

    let r = rows(run(&mut db, "DELETE FROM t WHERE id = 2 RETURNING *"));
    assert_eq!(r, vec![vec![Value::Int(2), Value::Text("b".into()), Value::Int(20)]]);
}

#[test]
fn default_column_values() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, name text DEFAULT 'anon', score integer DEFAULT 0)");
    run(&mut db, "INSERT INTO t (id) VALUES (1)");
    run(&mut db, "INSERT INTO t (id, score) VALUES (2, 99)");
    let r = rows(run(&mut db, "SELECT id, name, score FROM t ORDER BY id"));
    assert_eq!(r, vec![
        vec![Value::Int(1), Value::Text("anon".into()), Value::Int(0)],
        vec![Value::Int(2), Value::Text("anon".into()), Value::Int(99)],
    ]);
}

#[test]
fn serial_auto_increment() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE u (id serial PRIMARY KEY, name text)");
    run(&mut db, "INSERT INTO u (name) VALUES ('a'), ('b')");
    // Explicit value advances the sequence past it.
    run(&mut db, "INSERT INTO u (id, name) VALUES (50, 'm')");
    run(&mut db, "INSERT INTO u (name) VALUES ('c')");

    let r = rows(run(&mut db, "SELECT id, name FROM u ORDER BY id"));
    assert_eq!(r, vec![
        vec![Value::Int(1), Value::Text("a".into())],
        vec![Value::Int(2), Value::Text("b".into())],
        vec![Value::Int(50), Value::Text("m".into())],
        vec![Value::Int(51), Value::Text("c".into())],
    ]);
}

#[test]
fn casts_and_functions() {
    let mut db = Database::new();
    let r = rows(run(&mut db, "SELECT '42'::integer + 8, CAST(3.7 AS integer), 100::text || '!'"));
    assert_eq!(r[0], vec![Value::Int(50), Value::Int(4), Value::Text("100!".into())]);

    let r = rows(run(&mut db, "SELECT round(3.14159, 2), greatest(3,7,2), least(3,7,2), nullif(5,5)"));
    assert_eq!(r[0], vec![Value::Float(3.14), Value::Int(7), Value::Int(2), Value::Null]);

    let r = rows(run(&mut db, "SELECT trim('  hi  '), substr('postgres',1,4), replace('a-b-c','-','_')"));
    assert_eq!(r[0], vec![
        Value::Text("hi".into()),
        Value::Text("post".into()),
        Value::Text("a_b_c".into()),
    ]);
}

#[test]
fn information_schema_introspection() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE users (id serial PRIMARY KEY, name text NOT NULL, email text)");
    run(&mut db, "CREATE TABLE orders (id integer)");

    let r = rows(run(&mut db, "SELECT table_name FROM information_schema.tables ORDER BY table_name"));
    assert_eq!(r, vec![
        vec![Value::Text("orders".into())],
        vec![Value::Text("users".into())],
    ]);

    let r = rows(run(
        &mut db,
        "SELECT column_name, is_nullable FROM information_schema.columns WHERE table_name = 'users' ORDER BY ordinal_position",
    ));
    assert_eq!(r, vec![
        vec![Value::Text("id".into()), Value::Text("NO".into())],
        vec![Value::Text("name".into()), Value::Text("NO".into())],
        vec![Value::Text("email".into()), Value::Text("YES".into())],
    ]);
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
    assert_eq!(r, vec![
        vec![Value::Text("public".into()), Value::Text("alpha".into())],
        vec![Value::Text("public".into()), Value::Text("beta".into())],
    ]);

    // Regex operators directly.
    let r = rows(run(&mut db, "SELECT 'public' !~ '^pg_toast', 'pg_toast_x' ~ '^pg_toast', 'ABC' ~* '^abc'"));
    assert_eq!(r[0], vec![Value::Bool(true), Value::Bool(true), Value::Bool(true)]);
}

#[test]
fn extended_types() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE e (id serial, amount numeric(10,2), created timestamp, day date, tags jsonb)");
    run(&mut db, "INSERT INTO e (amount, created, day, tags) VALUES (19.99, '2024-03-15 10:30:00', '2024-03-15', '{\"k\":1}'), (5.50, '2023-12-01 08:00:00', '2023-12-01', '{\"k\":2}')");

    // Timestamp comparison via ISO text ordering.
    let r = rows(run(&mut db, "SELECT day FROM e WHERE created > '2024-01-01' ORDER BY day"));
    assert_eq!(r, vec![vec![Value::Text("2024-03-15".into())]]);

    // numeric arithmetic.
    let r = rows(run(&mut db, "SELECT amount * 2 FROM e ORDER BY amount"));
    assert_eq!(r, vec![vec![Value::Float(11.0)], vec![Value::Float(39.98)]]);

    // Casts to the new types round-trip the text.
    let r = rows(run(&mut db, "SELECT '550e8400-e29b-41d4-a716-446655440000'::uuid"));
    assert_eq!(r[0][0], Value::Text("550e8400-e29b-41d4-a716-446655440000".into()));
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
    run(&mut db, "CREATE TABLE users (id serial PRIMARY KEY, name text)");
    run(&mut db, "CREATE TABLE orders (id serial, user_id integer, amount integer)");
    run(&mut db, "INSERT INTO users (name) VALUES ('Alice'),('Bob'),('Carol')");
    run(&mut db, "INSERT INTO orders (user_id, amount) VALUES (1,100),(1,50),(2,200)");

    // IN (subquery) — duplicate user_ids must not duplicate output rows.
    let r = rows(run(&mut db, "SELECT name FROM users WHERE id IN (SELECT user_id FROM orders) ORDER BY name"));
    assert_eq!(r, vec![vec![Value::Text("Alice".into())], vec![Value::Text("Bob".into())]]);

    // NOT IN (subquery).
    let r = rows(run(&mut db, "SELECT name FROM users WHERE id NOT IN (SELECT user_id FROM orders)"));
    assert_eq!(r, vec![vec![Value::Text("Carol".into())]]);

    // Scalar subquery in the projection.
    let r = rows(run(&mut db, "SELECT (SELECT count(*) FROM orders) AS n"));
    assert_eq!(r[0][0], Value::Int(3));

    // Scalar subquery in WHERE.
    let r = rows(run(&mut db, "SELECT name FROM users WHERE (SELECT max(amount) FROM orders) > 150 AND id = 1"));
    assert_eq!(r, vec![vec![Value::Text("Alice".into())]]);

    // EXISTS / NOT EXISTS (uncorrelated).
    let r = rows(run(&mut db, "SELECT name FROM users WHERE EXISTS (SELECT 1 FROM orders WHERE amount > 1000)"));
    assert_eq!(r.len(), 0);
    let r = rows(run(&mut db, "SELECT count(*) FROM users WHERE NOT EXISTS (SELECT 1 FROM orders WHERE amount > 1000)"));
    assert_eq!(r[0][0], Value::Int(3));

    // DELETE driven by a scalar subquery.
    run(&mut db, "DELETE FROM orders WHERE amount < (SELECT avg(amount) FROM orders)");
    let r = rows(run(&mut db, "SELECT amount FROM orders ORDER BY amount"));
    assert_eq!(r, vec![vec![Value::Int(200)]]);
}

#[test]
fn unique_and_primary_key_enforcement() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer PRIMARY KEY, name text)");
    run(&mut db, "INSERT INTO t VALUES (1, 'a'), (2, 'b')");

    // Duplicate primary key (against existing row) is rejected.
    let dup = Parser::parse_sql("INSERT INTO t VALUES (1, 'dup')").unwrap().into_iter().next().unwrap();
    assert!(executor::execute(&mut db, dup).is_err());

    // Duplicate within the same batch is rejected.
    let batch = Parser::parse_sql("INSERT INTO t VALUES (3, 'c'), (3, 'd')").unwrap().into_iter().next().unwrap();
    assert!(executor::execute(&mut db, batch).is_err());

    // UPDATE that collides with another row's key is rejected.
    let upd = Parser::parse_sql("UPDATE t SET id = 2 WHERE id = 1").unwrap().into_iter().next().unwrap();
    assert!(executor::execute(&mut db, upd).is_err());

    // The table is unchanged by the rejected operations.
    let r = rows(run(&mut db, "SELECT id FROM t ORDER BY id"));
    assert_eq!(r, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);

    // A standalone UNIQUE index is enforced too.
    run(&mut db, "CREATE TABLE u (email text)");
    run(&mut db, "CREATE UNIQUE INDEX u_email ON u (email)");
    run(&mut db, "INSERT INTO u VALUES ('x@y.com')");
    let dup = Parser::parse_sql("INSERT INTO u VALUES ('x@y.com')").unwrap().into_iter().next().unwrap();
    assert!(executor::execute(&mut db, dup).is_err());
}

#[test]
fn alter_table() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer, name text)");
    run(&mut db, "INSERT INTO t VALUES (1, 'a'), (2, 'b')");

    // ADD COLUMN with a default backfills existing rows.
    run(&mut db, "ALTER TABLE t ADD COLUMN active boolean DEFAULT true");
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
    assert_eq!(r[0], vec![Value::Int(1), Value::Text("a".into()), Value::Bool(true)]);

    // RENAME TABLE.
    run(&mut db, "ALTER TABLE t RENAME TO items");
    let r = rows(run(&mut db, "SELECT count(*) FROM items"));
    assert_eq!(r[0][0], Value::Int(2));
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
    run(&mut db, "INSERT INTO t VALUES (1, true), (2, false), (3, true)");
    let r = rows(run(&mut db, "SELECT x FROM t WHERE ok = true AND x > 1"));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(3));
}
