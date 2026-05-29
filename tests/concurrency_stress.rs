//! Concurrency stress tests: many threads hammering one shared `Database`
//! through a mutex (the engine's actual concurrency model — see the server's
//! `Shared`). These assert that concurrent mutation + read traffic stays
//! consistent and never panics or deadlocks.

use std::sync::{Arc, Mutex};
use std::thread;

use postgres_rs::executor::{self, ExecResult};
use postgres_rs::sql::Parser;
use postgres_rs::storage::Database;
use postgres_rs::types::Value;

fn exec(db: &Arc<Mutex<Database>>, sql: &str) -> ExecResult {
    let stmt = Parser::parse_sql(sql).expect("parse").remove(0);
    let mut guard = db.lock().expect("db mutex");
    executor::execute(&mut guard, stmt).expect("execute")
}

fn scalar_i64(res: ExecResult) -> i64 {
    match res {
        ExecResult::Rows { rows, .. } => match &rows[0][0] {
            Value::Int(i) => *i,
            other => panic!("expected int, got {other:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn concurrent_inserts_are_consistent() {
    let db = Arc::new(Mutex::new(Database::new()));
    exec(&db, "CREATE TABLE t (id serial PRIMARY KEY, worker integer, n integer)");

    const WORKERS: i64 = 8;
    const PER_WORKER: i64 = 200;

    let mut handles = Vec::new();
    for w in 0..WORKERS {
        let db = Arc::clone(&db);
        handles.push(thread::spawn(move || {
            for n in 0..PER_WORKER {
                exec(
                    &db,
                    &format!("INSERT INTO t (worker, n) VALUES ({w}, {n})"),
                );
            }
        }));
    }
    for h in handles {
        h.join().expect("worker thread panicked");
    }

    // Every insert landed exactly once and the serial PK stayed unique.
    assert_eq!(
        scalar_i64(exec(&db, "SELECT count(*) FROM t")),
        WORKERS * PER_WORKER
    );
    assert_eq!(
        scalar_i64(exec(&db, "SELECT count(DISTINCT id) FROM t")),
        WORKERS * PER_WORKER
    );
    // Each worker's rows are all present.
    assert_eq!(
        scalar_i64(exec(&db, "SELECT count(*) FROM t WHERE worker = 3")),
        PER_WORKER
    );
}

#[test]
fn concurrent_readers_and_writers_do_not_corrupt_state() {
    let db = Arc::new(Mutex::new(Database::new()));
    exec(&db, "CREATE TABLE acct (id integer PRIMARY KEY, bal integer)");
    for id in 0..20 {
        exec(&db, &format!("INSERT INTO acct VALUES ({id}, 100)"));
    }

    let mut handles = Vec::new();
    // Writers: move balance around (total is invariant within each statement).
    for _ in 0..4 {
        let db = Arc::clone(&db);
        handles.push(thread::spawn(move || {
            for i in 0..150 {
                let a = i % 20;
                exec(&db, &format!("UPDATE acct SET bal = bal + 1 WHERE id = {a}"));
                exec(&db, &format!("UPDATE acct SET bal = bal - 1 WHERE id = {a}"));
            }
        }));
    }
    // Readers: aggregate repeatedly; must never panic or see a torn schema.
    for _ in 0..4 {
        let db = Arc::clone(&db);
        handles.push(thread::spawn(move || {
            for _ in 0..150 {
                let total = scalar_i64(exec(&db, "SELECT sum(bal) FROM acct"));
                // Each writer does a non-transactional +1 then -1, so a reader
                // may catch up to 4 writers mid-pair (sum in [2000, 2004]); it
                // must never see a torn/corrupt total outside that window.
                assert!(
                    (2000..=2004).contains(&total),
                    "saw corrupt total {total}"
                );
            }
        }));
    }
    for h in handles {
        h.join().expect("thread panicked");
    }
    assert_eq!(scalar_i64(exec(&db, "SELECT sum(bal) FROM acct")), 2000);
}
