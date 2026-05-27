//! Index micro-benchmark: proves B-tree indexes turn full scans into
//! sub-millisecond lookups on a large table.
//!
//! Run with optimizations:
//!
//! ```text
//! cargo run --release --bin bench [row_count]
//! ```
//!
//! It builds one table without an index and an identical one with indexes,
//! then times point lookups, `IN` lookups, and range scans against each.
//! Results are reported as scan-time vs index-time and the speedup factor.

use std::time::{Duration, Instant};

use postgres_rs::executor::{self, ExecResult};
use postgres_rs::sql::Parser;
use postgres_rs::storage::Database;

/// Execute one SQL string against `db`, returning the last statement's result.
fn run(db: &mut Database, sql: &str) -> ExecResult {
    let mut last = ExecResult::Empty;
    for stmt in Parser::parse_sql(sql).expect("parse") {
        last = executor::execute(db, stmt).expect("execute");
    }
    last
}

/// Number of rows a query returned (0 for non-row results).
fn row_count(res: &ExecResult) -> usize {
    match res {
        ExecResult::Rows { rows, .. } => rows.len(),
        _ => 0,
    }
}

/// Build a table of `n` rows: id (1..=n, PRIMARY KEY → auto-indexed), a
/// category cycling through 10 values, and price = pseudo-random in 0..100000.
/// `with_secondary` additionally creates a B-tree index on `price`.
fn build(n: i64, with_secondary: bool) -> Database {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id integer PRIMARY KEY, category integer, price integer)");

    // Batch the inserts so we don't pay parser overhead per row, but cap each
    // statement's size to keep memory reasonable.
    const BATCH: i64 = 1000;
    let mut id = 1;
    while id <= n {
        let end = (id + BATCH - 1).min(n);
        let mut sql = String::from("INSERT INTO t VALUES ");
        for i in id..=end {
            if i > id {
                sql.push(',');
            }
            // A cheap deterministic hash to scatter prices.
            let price = (i.wrapping_mul(2654435761) & 0xFFFF) % 100_000;
            sql.push_str(&format!("({}, {}, {})", i, i % 10, price));
        }
        run(&mut db, &sql);
        id = end + 1;
    }

    if with_secondary {
        run(&mut db, "CREATE INDEX idx_price ON t (price)");
    }
    db
}

/// Run `query` `iters` times, returning the total elapsed time and the row
/// count of the final run (to confirm both paths return the same rows).
fn time(db: &mut Database, query: &str, iters: u32) -> (Duration, usize) {
    // One warm-up run (e.g. to fault in pages) before timing.
    let _ = run(db, query);
    let start = Instant::now();
    let mut last = 0;
    for _ in 0..iters {
        last = row_count(&run(db, query));
    }
    (start.elapsed(), last)
}

/// Print one labeled comparison line.
fn report(label: &str, scan: Duration, index: Duration, iters: u32, scan_rows: usize, idx_rows: usize) {
    let scan_us = scan.as_secs_f64() * 1e6 / iters as f64;
    let idx_us = index.as_secs_f64() * 1e6 / iters as f64;
    let speedup = if idx_us > 0.0 { scan_us / idx_us } else { f64::INFINITY };
    let agree = if scan_rows == idx_rows { "ok" } else { "MISMATCH" };
    println!(
        "{label:<28} scan {scan_us:>10.2} us   index {idx_us:>8.2} us   speedup {speedup:>8.1}x   rows {scan_rows} ({agree})"
    );
}

fn main() {
    let n: i64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(200_000);

    println!("building two tables of {n} rows (one indexed, one not)...");
    let build_start = Instant::now();
    let mut scan_db = build(n, false); // only the PK index exists
    let mut index_db = build(n, true); // + secondary index on price
    println!("build took {:.2?}\n", build_start.elapsed());

    // Point lookup on the secondary-indexed column. The scan_db has no index on
    // `price`, so it full-scans; index_db uses idx_price.
    let target_price = {
        // Pick a price that exists (the value for id = n/2).
        let i = n / 2;
        (i.wrapping_mul(2654435761) & 0xFFFF) % 100_000
    };
    let q_point = format!("SELECT id FROM t WHERE price = {target_price}");
    let (s, sr) = time(&mut scan_db, &q_point, 50);
    let (x, xr) = time(&mut index_db, &q_point, 50);
    report("point lookup (price=)", s, x, 50, sr, xr);

    // IN list on the indexed column.
    let q_in = format!(
        "SELECT id FROM t WHERE price IN ({}, {}, {})",
        target_price,
        (target_price + 7) % 100_000,
        (target_price + 13) % 100_000
    );
    let (s, sr) = time(&mut scan_db, &q_in, 50);
    let (x, xr) = time(&mut index_db, &q_in, 50);
    report("IN list (3 prices)", s, x, 50, sr, xr);

    // Narrow range scan.
    let q_range = "SELECT id FROM t WHERE price BETWEEN 100 AND 300";
    let (s, sr) = time(&mut scan_db, q_range, 50);
    let (x, xr) = time(&mut index_db, q_range, 50);
    report("range scan (BETWEEN)", s, x, 50, sr, xr);

    // Point lookup on the PRIMARY KEY column. Both DBs have the auto-created PK
    // index, so to show the contrast we compare against a fresh unindexed copy.
    // Here both use the index — this line demonstrates the absolute latency of
    // an indexed PK lookup on a large table.
    let q_pk = format!("SELECT category FROM t WHERE id = {}", n / 2);
    let (x, xr) = time(&mut index_db, &q_pk, 50);
    let pk_us = x.as_secs_f64() * 1e6 / 50.0;
    println!("{:<28} index {pk_us:>8.2} us   rows {xr} (PK auto-index)", "PK lookup (id=)");
}
