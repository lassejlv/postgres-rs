//! Tests that B-tree indexes return results identical to the full-scan path,
//! stay consistent across UPDATE/DELETE, and survive a WAL-style replay.
//!
//! The strategy throughout is differential: build two databases with identical
//! data, add an index to only one, run the same query against both, and assert
//! the rows match exactly. Because the executor always re-checks the predicate
//! after using an index, any divergence would be a real bug.

use postgres_rs::executor::{self, ExecResult};
use postgres_rs::sql::Parser;
use postgres_rs::sql::serialize::statement_to_sql;
use postgres_rs::storage::Database;
use postgres_rs::types::Value;

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
        _ => panic!("expected rows"),
    }
}

/// Populate a fresh products table with `n` rows: id (1..=n), category cycling
/// through a few values, and price = id * 3 % 1000.
fn seed_products(db: &mut Database, n: i64) {
    run(
        db,
        "CREATE TABLE products (id integer PRIMARY KEY, category text, price integer)",
    );
    let cats = ["a", "b", "c", "d"];
    let mut sql = String::from("INSERT INTO products VALUES ");
    for i in 1..=n {
        if i > 1 {
            sql.push(',');
        }
        sql.push_str(&format!(
            "({}, '{}', {})",
            i,
            cats[(i as usize) % cats.len()],
            (i * 3) % 1000
        ));
    }
    run(db, &sql);
}

/// Build two identical databases, indexing `index_col` on only the second.
fn paired(n: i64, index_sql: Option<&str>) -> (Database, Database) {
    let mut unindexed = Database::new();
    let mut indexed = Database::new();
    seed_products(&mut unindexed, n);
    seed_products(&mut indexed, n);
    if let Some(sql) = index_sql {
        run(&mut indexed, sql);
    }
    (unindexed, indexed)
}

/// Assert the same query yields identical rows with and without an index.
fn assert_same(unindexed: &mut Database, indexed: &mut Database, query: &str) {
    let a = rows(run(unindexed, query));
    let b = rows(run(indexed, query));
    assert_eq!(a, b, "index path diverged for query: {query}");
}

#[test]
fn point_lookup_matches_scan() {
    let (mut u, mut i) = paired(500, Some("CREATE INDEX idx_price ON products (price)"));
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price = 300 ORDER BY id",
    );
    // Reversed operand order must use the index too.
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE 300 = price ORDER BY id",
    );
    // A value with no match returns nothing on both paths.
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price = 999999",
    );
}

#[test]
fn primary_key_autoindex_matches_scan() {
    // No explicit index: the PRIMARY KEY index is auto-created on `id`.
    let (mut u, mut i) = paired(500, None);
    assert_same(
        &mut u,
        &mut i,
        "SELECT id, category FROM products WHERE id = 250",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE id IN (1, 100, 250, 9999) ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE id BETWEEN 10 AND 20 ORDER BY id",
    );
}

#[test]
fn range_and_between_match_scan() {
    let (mut u, mut i) = paired(500, Some("CREATE INDEX idx_price ON products (price)"));
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price < 50 ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price <= 50 ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price > 950 ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price >= 950 ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price BETWEEN 100 AND 200 ORDER BY id",
    );
    // Combined with an extra predicate the index can't cover: still correct.
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price BETWEEN 100 AND 400 AND category = 'a' ORDER BY id",
    );
}

#[test]
fn in_list_matches_scan() {
    let (mut u, mut i) = paired(300, Some("CREATE INDEX idx_cat ON products (category)"));
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE category IN ('a', 'c') ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT count(*) FROM products WHERE category = 'b'",
    );
}

#[test]
fn update_keeps_index_consistent() {
    let (mut u, mut i) = paired(200, Some("CREATE INDEX idx_price ON products (price)"));
    // Change indexed values; the index must follow.
    run(
        &mut u,
        "UPDATE products SET price = price + 1000 WHERE id <= 50",
    );
    run(
        &mut i,
        "UPDATE products SET price = price + 1000 WHERE id <= 50",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price >= 1000 ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price = 1003 ORDER BY id",
    );
    // The old key must no longer resolve to the moved rows.
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price = 3 ORDER BY id",
    );
}

#[test]
fn delete_keeps_index_consistent() {
    let (mut u, mut i) = paired(200, Some("CREATE INDEX idx_price ON products (price)"));
    run(&mut u, "DELETE FROM products WHERE price < 100");
    run(&mut i, "DELETE FROM products WHERE price < 100");
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price < 200 ORDER BY id",
    );
    assert_same(&mut u, &mut i, "SELECT count(*) FROM products");
    // Deleted rows are gone from point lookups too.
    assert_same(&mut u, &mut i, "SELECT id FROM products WHERE id = 1");
}

#[test]
fn indexed_join_matches_scan() {
    let mut u = Database::new();
    let mut i = Database::new();
    for db in [&mut u, &mut i] {
        run(db, "CREATE TABLE users (id integer PRIMARY KEY, name text)");
        run(
            db,
            "CREATE TABLE orders (id integer, user_id integer, amount integer)",
        );
        run(
            db,
            "INSERT INTO users VALUES (1,'Alice'),(2,'Bob'),(3,'Carol')",
        );
        run(
            db,
            "INSERT INTO orders VALUES (10,1,100),(11,1,50),(12,2,200),(13,3,75)",
        );
    }
    // users.id is the PK index; the join probes it per order row.
    let q = "SELECT u.name, o.amount FROM orders o INNER JOIN users u ON u.id = o.user_id ORDER BY o.amount";
    assert_same(&mut u, &mut i, q);
    // LEFT join with the indexed side as the inner table.
    let q2 =
        "SELECT o.amount, u.name FROM orders o LEFT JOIN users u ON u.id = o.user_id ORDER BY o.id";
    assert_same(&mut u, &mut i, q2);
}

#[test]
fn drop_index_falls_back_to_scan() {
    let mut db = Database::new();
    seed_products(&mut db, 100);
    run(&mut db, "CREATE INDEX idx_price ON products (price)");
    let before = rows(run(
        &mut db,
        "SELECT id FROM products WHERE price = 30 ORDER BY id",
    ));
    run(&mut db, "DROP INDEX idx_price");
    let after = rows(run(
        &mut db,
        "SELECT id FROM products WHERE price = 30 ORDER BY id",
    ));
    assert_eq!(before, after, "dropping the index must not change results");
}

#[test]
fn index_survives_wal_replay() {
    // Mimic the WAL: serialize each mutating statement (including CREATE INDEX),
    // then rebuild a fresh database from the serialized log and confirm the
    // index is present and correct.
    let mut original = Database::new();
    let mut log = String::new();
    let mut apply = |db: &mut Database, sql: &str| {
        for stmt in Parser::parse_sql(sql).expect("parse") {
            let serialized = statement_to_sql(&stmt);
            let res = executor::execute(db, stmt).expect("execute");
            if !serialized.is_empty() && !matches!(res, ExecResult::Rows { .. }) {
                log.push_str(&serialized);
                log.push_str(";\n");
            }
        }
    };
    apply(
        &mut original,
        "CREATE TABLE t (id integer PRIMARY KEY, v integer)",
    );
    apply(&mut original, "CREATE INDEX idx_v ON t (v)");
    apply(
        &mut original,
        "INSERT INTO t VALUES (1,10),(2,20),(3,30),(4,20)",
    );
    apply(&mut original, "UPDATE t SET v = 99 WHERE id = 1");
    apply(&mut original, "DELETE FROM t WHERE id = 3");

    let mut recovered = Database::new();
    for stmt in Parser::parse_sql(&log).expect("reparse WAL") {
        executor::execute(&mut recovered, stmt).expect("replay");
    }

    // The index must be present after replay and produce identical results.
    let q1 = "SELECT id FROM t WHERE v = 20 ORDER BY id";
    let q2 = "SELECT id FROM t WHERE v = 99";
    let q3 = "SELECT id FROM t WHERE v BETWEEN 0 AND 100 ORDER BY id";
    assert_eq!(rows(run(&mut original, q1)), rows(run(&mut recovered, q1)));
    assert_eq!(rows(run(&mut original, q2)), rows(run(&mut recovered, q2)));
    assert_eq!(rows(run(&mut original, q3)), rows(run(&mut recovered, q3)));
}

#[test]
fn create_index_round_trips_through_serialize() {
    let stmt = Parser::parse_sql("CREATE UNIQUE INDEX my_idx ON t (col)")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let sql = statement_to_sql(&stmt);
    let reparsed = Parser::parse_sql(&sql).unwrap().into_iter().next().unwrap();
    assert_eq!(stmt, reparsed, "CREATE INDEX did not round-trip: {sql}");

    let stmt = Parser::parse_sql("DROP INDEX IF EXISTS my_idx")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let sql = statement_to_sql(&stmt);
    let reparsed = Parser::parse_sql(&sql).unwrap().into_iter().next().unwrap();
    assert_eq!(stmt, reparsed, "DROP INDEX did not round-trip: {sql}");
}

#[test]
fn multi_column_index_full_key_matches_scan() {
    // products has (id, category, price); index on (category, price).
    let (mut u, mut i) = paired(
        400,
        Some("CREATE INDEX idx_cat_price ON products (category, price)"),
    );
    // Full-key equality must use the multi-column index and match the scan.
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE category = 'a' AND price = 12 ORDER BY id",
    );
    // Operand order and conjunct order shouldn't matter.
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price = 12 AND 'a' = category ORDER BY id",
    );
    // Leading-prefix equality (only the first key column).
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE category = 'b' ORDER BY id",
    );
    // Extra non-indexable conjunct is re-checked correctly.
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE category = 'c' AND price > 100 ORDER BY id",
    );
    // A non-matching full key returns nothing.
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE category = 'a' AND price = 999999",
    );
}

#[test]
fn multi_column_index_consistent_after_mutations() {
    let (mut u, mut i) = paired(
        200,
        Some("CREATE INDEX idx_cat_price ON products (category, price)"),
    );
    run(&mut u, "UPDATE products SET category = 'z' WHERE id <= 40");
    run(&mut i, "UPDATE products SET category = 'z' WHERE id <= 40");
    run(&mut u, "DELETE FROM products WHERE price < 50");
    run(&mut i, "DELETE FROM products WHERE price < 50");
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE category = 'z' ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE category = 'a' AND price = 600 ORDER BY id",
    );
}

#[test]
fn hash_index_equality_matches_scan() {
    let (mut u, mut i) = paired(
        400,
        Some("CREATE INDEX idx_price_h ON products USING hash (price)"),
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price = 300 ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE 300 = price ORDER BY id",
    );
    // No match.
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price = 999999",
    );
    // Range query on a hash index must fall back to a scan and stay correct.
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price > 950 ORDER BY id",
    );
    // Mutations keep the hash index consistent.
    run(&mut u, "UPDATE products SET price = 7 WHERE id = 5");
    run(&mut i, "UPDATE products SET price = 7 WHERE id = 5");
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price = 7 ORDER BY id",
    );
}

#[test]
fn expression_index_matches_scan() {
    let mut u = Database::new();
    let mut i = Database::new();
    for db in [&mut u, &mut i] {
        run(db, "CREATE TABLE people (id integer PRIMARY KEY, name text)");
        run(
            db,
            "INSERT INTO people VALUES (1,'Alice'),(2,'BOB'),(3,'alice'),(4,'Carol'),(5,'bob')",
        );
    }
    run(
        &mut i,
        "CREATE INDEX idx_lower_name ON people ((lower(name)))",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM people WHERE lower(name) = 'alice' ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM people WHERE lower(name) = 'bob' ORDER BY id",
    );
    // Update changes the expression key; index must follow.
    run(&mut u, "UPDATE people SET name = 'ALICE' WHERE id = 4");
    run(&mut i, "UPDATE people SET name = 'ALICE' WHERE id = 4");
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM people WHERE lower(name) = 'alice' ORDER BY id",
    );
}

#[test]
fn partial_index_matches_scan() {
    let mut u = Database::new();
    let mut i = Database::new();
    for db in [&mut u, &mut i] {
        run(
            db,
            "CREATE TABLE tasks (id integer PRIMARY KEY, active boolean, owner integer)",
        );
        run(
            db,
            "INSERT INTO tasks VALUES (1,true,10),(2,false,10),(3,true,20),(4,true,10),(5,false,20)",
        );
    }
    run(
        &mut i,
        "CREATE INDEX idx_active_owner ON tasks (owner) WHERE active",
    );
    // Query whose WHERE contains the partial predicate verbatim uses the index.
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM tasks WHERE active AND owner = 10 ORDER BY id",
    );
    // Just the predicate: scans the whole partial index.
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM tasks WHERE active ORDER BY id",
    );
    // A row leaving the predicate must drop out of the index.
    run(&mut u, "UPDATE tasks SET active = false WHERE id = 1");
    run(&mut i, "UPDATE tasks SET active = false WHERE id = 1");
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM tasks WHERE active AND owner = 10 ORDER BY id",
    );
    // A row entering the predicate must appear.
    run(&mut u, "UPDATE tasks SET active = true WHERE id = 2");
    run(&mut i, "UPDATE tasks SET active = true WHERE id = 2");
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM tasks WHERE active AND owner = 10 ORDER BY id",
    );
    // A query NOT implying the predicate must still be correct (falls back).
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM tasks WHERE owner = 20 ORDER BY id",
    );
}

#[test]
fn include_and_method_round_trip_through_serialize() {
    for sql in [
        "CREATE INDEX m_idx ON t (a, b)",
        "CREATE INDEX e_idx ON t ((lower(name)))",
        "CREATE INDEX p_idx ON t (a) WHERE active",
        "CREATE INDEX c_idx ON t (a) INCLUDE (b, c)",
        "CREATE INDEX h_idx ON t USING hash (a)",
        "CREATE UNIQUE INDEX u_idx ON t (a, b) INCLUDE (c) WHERE active",
    ] {
        let stmt = Parser::parse_sql(sql)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let out = statement_to_sql(&stmt);
        let reparsed = Parser::parse_sql(&out).unwrap().into_iter().next().unwrap();
        assert_eq!(stmt, reparsed, "did not round-trip: {sql} -> {out}");
    }
}

#[test]
fn advanced_indexes_survive_wal_replay() {
    let mut original = Database::new();
    let mut log = String::new();
    let mut apply = |db: &mut Database, sql: &str| {
        for stmt in Parser::parse_sql(sql).expect("parse") {
            let serialized = statement_to_sql(&stmt);
            let res = executor::execute(db, stmt).expect("execute");
            if !serialized.is_empty() && !matches!(res, ExecResult::Rows { .. }) {
                log.push_str(&serialized);
                log.push_str(";\n");
            }
        }
    };
    apply(
        &mut original,
        "CREATE TABLE t (id integer PRIMARY KEY, c text, p integer, active boolean)",
    );
    apply(&mut original, "CREATE INDEX idx_cp ON t (c, p)");
    apply(&mut original, "CREATE INDEX idx_lc ON t ((lower(c)))");
    apply(&mut original, "CREATE INDEX idx_pa ON t (p) WHERE active");
    apply(&mut original, "CREATE INDEX idx_ph ON t USING hash (p)");
    apply(
        &mut original,
        "INSERT INTO t VALUES (1,'A',10,true),(2,'b',20,false),(3,'A',10,true),(4,'C',30,true)",
    );
    apply(&mut original, "UPDATE t SET c = 'X' WHERE id = 4");

    let mut recovered = Database::new();
    for stmt in Parser::parse_sql(&log).expect("reparse WAL") {
        executor::execute(&mut recovered, stmt).expect("replay");
    }

    for q in [
        "SELECT id FROM t WHERE c = 'A' AND p = 10 ORDER BY id",
        "SELECT id FROM t WHERE lower(c) = 'a' ORDER BY id",
        "SELECT id FROM t WHERE active AND p = 10 ORDER BY id",
        "SELECT id FROM t WHERE p = 20 ORDER BY id",
    ] {
        assert_eq!(
            rows(run(&mut original, q)),
            rows(run(&mut recovered, q)),
            "replay diverged for: {q}"
        );
    }
}

#[test]
fn gist_index_equality_and_range_match_scan() {
    // GiST is btree-backed here: equality + range must match the scan path.
    let (mut u, mut i) = paired(
        500,
        Some("CREATE INDEX idx_price_g ON products USING gist (price)"),
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price = 300 ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price > 950 ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price BETWEEN 100 AND 200 ORDER BY id",
    );
    // Mutations keep it consistent.
    run(&mut u, "UPDATE products SET price = price + 1000 WHERE id <= 30");
    run(&mut i, "UPDATE products SET price = price + 1000 WHERE id <= 30");
    run(&mut u, "DELETE FROM products WHERE price < 60");
    run(&mut i, "DELETE FROM products WHERE price < 60");
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price >= 1000 ORDER BY id",
    );
}

#[test]
fn spgist_index_equality_and_range_match_scan() {
    // SP-GiST is btree-backed here too.
    let (mut u, mut i) = paired(
        400,
        Some("CREATE INDEX idx_price_sp ON products USING spgist (price)"),
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price = 99 ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price < 50 ORDER BY id",
    );
    run(&mut u, "UPDATE products SET price = 5 WHERE id = 7");
    run(&mut i, "UPDATE products SET price = 5 WHERE id = 7");
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price = 5 ORDER BY id",
    );
}

#[test]
fn brin_index_range_and_equality_match_scan() {
    // BRIN summarises block ranges of `price`; range/eq must match the scan
    // exactly (the executor re-checks each surviving range).
    let (mut u, mut i) = paired(
        500,
        Some("CREATE INDEX idx_price_b ON products USING brin (price)"),
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price < 50 ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price > 950 ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price BETWEEN 100 AND 200 ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price = 300 ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price = 999999",
    );
    // Mutations must resummarise affected ranges.
    run(
        &mut u,
        "UPDATE products SET price = price + 1000 WHERE id <= 60",
    );
    run(
        &mut i,
        "UPDATE products SET price = price + 1000 WHERE id <= 60",
    );
    run(&mut u, "DELETE FROM products WHERE price < 80");
    run(&mut i, "DELETE FROM products WHERE price < 80");
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price >= 1000 ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM products WHERE price BETWEEN 80 AND 500 ORDER BY id",
    );
}

#[test]
fn brin_index_with_nulls_matches_scan() {
    let mut u = Database::new();
    let mut i = Database::new();
    for db in [&mut u, &mut i] {
        run(db, "CREATE TABLE t (id integer, v integer)");
        run(
            db,
            "INSERT INTO t (id, v) VALUES (1,10),(2,NULL),(3,30),(4,NULL),(5,5),(6,500),(7,NULL),(8,42)",
        );
    }
    run(&mut i, "CREATE INDEX idx_vb ON t USING brin (v)");
    assert_same(&mut u, &mut i, "SELECT id FROM t WHERE v > 0 ORDER BY id");
    assert_same(&mut u, &mut i, "SELECT id FROM t WHERE v = 42 ORDER BY id");
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM t WHERE v BETWEEN 5 AND 50 ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM t WHERE v IS NULL ORDER BY id",
    );
}

/// Build paired databases with a `tags text[]`-style column (stored as array
/// text). Only the second gets the GIN index.
fn paired_tags(index_sql: Option<&str>) -> (Database, Database) {
    let mut u = Database::new();
    let mut i = Database::new();
    for db in [&mut u, &mut i] {
        run(db, "CREATE TABLE docs (id integer PRIMARY KEY, tags text[])");
        run(
            db,
            "INSERT INTO docs VALUES \
             (1, '{a,b,c}'), (2, '{b}'), (3, '{c,d}'), (4, '{a,d,e}'), \
             (5, '{}'), (6, '{b,c}'), (7, '{a}'), (8, '{d,e,f}')",
        );
    }
    if let Some(sql) = index_sql {
        run(&mut i, sql);
    }
    (u, i)
}

#[test]
fn gin_index_containment_matches_scan() {
    let (mut u, mut i) = paired_tags(Some("CREATE INDEX idx_tags ON docs USING gin (tags)"));
    // Single-element containment.
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM docs WHERE tags @> ARRAY['a'] ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM docs WHERE tags @> ARRAY['d'] ORDER BY id",
    );
    // Multi-element containment (intersection of posting lists).
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM docs WHERE tags @> ARRAY['a','b'] ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM docs WHERE tags @> ARRAY['d','e'] ORDER BY id",
    );
    // An element nobody has.
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM docs WHERE tags @> ARRAY['z'] ORDER BY id",
    );
}

#[test]
fn gin_index_consistent_after_mutations() {
    let (mut u, mut i) = paired_tags(Some("CREATE INDEX idx_tags ON docs USING gin (tags)"));
    run(&mut u, "UPDATE docs SET tags = '{a,z}' WHERE id = 2");
    run(&mut i, "UPDATE docs SET tags = '{a,z}' WHERE id = 2");
    run(&mut u, "DELETE FROM docs WHERE id = 4");
    run(&mut i, "DELETE FROM docs WHERE id = 4");
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM docs WHERE tags @> ARRAY['a'] ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM docs WHERE tags @> ARRAY['z'] ORDER BY id",
    );
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM docs WHERE tags @> ARRAY['d'] ORDER BY id",
    );
}

#[test]
fn new_access_methods_round_trip_and_replay() {
    // Round-trip each method through serialize.
    for sql in [
        "CREATE INDEX g_idx ON t USING gist (a)",
        "CREATE INDEX sp_idx ON t USING spgist (a)",
        "CREATE INDEX b_idx ON t USING brin (a)",
        "CREATE INDEX gin_idx ON t USING gin (a)",
    ] {
        let stmt = Parser::parse_sql(sql)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let out = statement_to_sql(&stmt);
        let reparsed = Parser::parse_sql(&out).unwrap().into_iter().next().unwrap();
        assert_eq!(stmt, reparsed, "did not round-trip: {sql} -> {out}");
    }

    // WAL replay must rebuild each method correctly.
    let mut original = Database::new();
    let mut log = String::new();
    let mut apply = |db: &mut Database, sql: &str| {
        for stmt in Parser::parse_sql(sql).expect("parse") {
            let serialized = statement_to_sql(&stmt);
            let res = executor::execute(db, stmt).expect("execute");
            if !serialized.is_empty() && !matches!(res, ExecResult::Rows { .. }) {
                log.push_str(&serialized);
                log.push_str(";\n");
            }
        }
    };
    apply(
        &mut original,
        "CREATE TABLE t (id integer PRIMARY KEY, p integer, tags text[])",
    );
    apply(&mut original, "CREATE INDEX idx_g ON t USING gist (p)");
    apply(&mut original, "CREATE INDEX idx_b ON t USING brin (p)");
    apply(&mut original, "CREATE INDEX idx_gin ON t USING gin (tags)");
    apply(
        &mut original,
        "INSERT INTO t VALUES (1,10,'{a,b}'),(2,20,'{b,c}'),(3,30,'{a}'),(4,20,'{c}')",
    );
    apply(&mut original, "DELETE FROM t WHERE id = 4");

    let mut recovered = Database::new();
    for stmt in Parser::parse_sql(&log).expect("reparse WAL") {
        executor::execute(&mut recovered, stmt).expect("replay");
    }
    for q in [
        "SELECT id FROM t WHERE p > 15 ORDER BY id",
        "SELECT id FROM t WHERE p = 10 ORDER BY id",
        "SELECT id FROM t WHERE tags @> ARRAY['a'] ORDER BY id",
        "SELECT id FROM t WHERE tags @> ARRAY['b','c'] ORDER BY id",
    ] {
        assert_eq!(
            rows(run(&mut original, q)),
            rows(run(&mut recovered, q)),
            "replay diverged for: {q}"
        );
    }
}

#[test]
fn pg_am_lists_new_access_methods() {
    let mut db = Database::new();
    let res = run(
        &mut db,
        "SELECT amname FROM pg_am WHERE amname IN ('btree','hash','gist','spgist','brin','gin') ORDER BY amname",
    );
    let got: Vec<String> = rows(res)
        .into_iter()
        .map(|r| match &r[0] {
            Value::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect();
    assert_eq!(
        got,
        vec!["brin", "btree", "gin", "gist", "hash", "spgist"]
    );
}

#[test]
fn null_values_excluded_from_index_scans() {
    // A nullable indexed column: NULLs must never be returned by range scans
    // (comparisons with NULL are never true), matching the scan path.
    let mut u = Database::new();
    let mut i = Database::new();
    for db in [&mut u, &mut i] {
        run(db, "CREATE TABLE t (id integer, v integer)");
        run(
            db,
            "INSERT INTO t (id, v) VALUES (1, 10), (2, NULL), (3, 30), (4, NULL)",
        );
    }
    run(&mut i, "CREATE INDEX idx_v ON t (v)");
    assert_same(&mut u, &mut i, "SELECT id FROM t WHERE v > 0 ORDER BY id");
    assert_same(&mut u, &mut i, "SELECT id FROM t WHERE v = 10");
    assert_same(
        &mut u,
        &mut i,
        "SELECT id FROM t WHERE v IS NULL ORDER BY id",
    );
}
