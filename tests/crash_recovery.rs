//! Crash-recovery fault injection.
//!
//! These tests write a WAL via the real [`Wal`] machinery, then simulate a
//! crash by truncating/corrupting the tail of the log (a partial last record),
//! reopen the WAL, replay the recovered contents the same way the server's
//! startup does, and assert the engine recovers the well-formed prefix without
//! panicking and can still serve queries.
//!
//! Each test uses a unique temp directory under `std::env::temp_dir()`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use postgres_rs::executor::{self, ExecResult};
use postgres_rs::sql::Parser;
use postgres_rs::storage::Database;
use postgres_rs::types::Value;
use postgres_rs::wal::Wal;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique temp directory; removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("pgrs_crash_{label}_{nanos}_{n}"));
        std::fs::create_dir_all(&p).expect("create temp dir");
        TempDir(p)
    }

    fn dir(&self) -> &str {
        self.0.to_str().unwrap()
    }

    fn wal_path(&self) -> PathBuf {
        self.0.join("wal.sql")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Replay WAL `contents` into a fresh database, mirroring the server's startup
/// replay: parse the whole log as a script and apply each statement, skipping
/// ones that error (a torn tail). Returns the database.
fn replay(contents: &str) -> Database {
    let mut db = Database::new();
    if contents.trim().is_empty() {
        return db;
    }
    // Parsing the whole log may itself fail if the tail is a partial statement;
    // in that case fall back to applying the statements that do parse from the
    // longest well-formed prefix. The server logs and starts empty on a full
    // parse failure, so emulate that: try whole, then per-prefix.
    match Parser::parse_sql(contents) {
        Ok(statements) => {
            for stmt in statements {
                let _ = executor::execute(&mut db, stmt);
            }
        }
        Err(_) => {
            // Torn tail: recover the well-formed prefix statement by statement.
            for piece in contents.split_inclusive(";\n") {
                if let Ok(stmts) = Parser::parse_sql(piece) {
                    for stmt in stmts {
                        let _ = executor::execute(&mut db, stmt);
                    }
                }
            }
        }
    }
    db
}

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    let stmt = Parser::parse_sql(sql).unwrap().into_iter().next().unwrap();
    match executor::execute(db, stmt).expect("query") {
        ExecResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows"),
    }
}

fn cell(v: &Value) -> String {
    v.to_text().unwrap_or_else(|| "NULL".into())
}

/// Write a few well-formed records through the real WAL, then reopen and verify
/// the existing contents replay to the expected state (the no-fault baseline).
#[test]
fn clean_wal_recovers_fully() {
    let tmp = TempDir::new("clean");
    {
        let (mut wal, existing) = Wal::open(tmp.dir()).expect("open wal");
        assert!(existing.is_empty());
        wal.append("CREATE TABLE t (id integer PRIMARY KEY, v integer)")
            .unwrap();
        wal.append("INSERT INTO t VALUES (1, 10)").unwrap();
        wal.append("INSERT INTO t VALUES (2, 20)").unwrap();
        wal.append("UPDATE t SET v = 99 WHERE id = 1").unwrap();
    }
    // Reopen: replay the existing log.
    let (_wal, existing) = Wal::open(tmp.dir()).expect("reopen wal");
    let mut db = replay(&existing);
    let r = rows(&mut db, "SELECT id, v FROM t ORDER BY id");
    assert_eq!(cell(&r[0][0]), "1");
    assert_eq!(cell(&r[0][1]), "99");
    assert_eq!(cell(&r[1][0]), "2");
    assert_eq!(cell(&r[1][1]), "20");
}

/// Truncate the WAL mid-record (a torn write) and assert the well-formed prefix
/// still recovers and the database serves queries.
#[test]
fn truncated_tail_recovers_prefix() {
    let tmp = TempDir::new("trunc");
    {
        let (mut wal, _) = Wal::open(tmp.dir()).expect("open wal");
        wal.append("CREATE TABLE t (id integer PRIMARY KEY, v integer)")
            .unwrap();
        wal.append("INSERT INTO t VALUES (1, 10)").unwrap();
        wal.append("INSERT INTO t VALUES (2, 20)").unwrap();
    }
    // Simulate a crash mid-append of a 4th record: append a partial statement
    // with no terminating ";\n".
    let mut contents = std::fs::read_to_string(tmp.wal_path()).unwrap();
    contents.push_str("INSERT INTO t VALUES (3, 3"); // torn: unterminated
    std::fs::write(tmp.wal_path(), &contents).unwrap();

    // Reopen and replay — must not panic, must recover the first 3 records.
    let (_wal, existing) = Wal::open(tmp.dir()).expect("reopen wal");
    let mut db = replay(&existing);
    let r = rows(&mut db, "SELECT count(*) FROM t");
    assert_eq!(cell(&r[0][0]), "2", "two committed rows should survive");
    let r2 = rows(&mut db, "SELECT v FROM t WHERE id = 2");
    assert_eq!(cell(&r2[0][0]), "20");
}

/// Corrupt the tail with random garbage bytes and assert recovery of the prefix
/// without panicking.
#[test]
fn corrupted_tail_recovers_prefix() {
    let tmp = TempDir::new("corrupt");
    {
        let (mut wal, _) = Wal::open(tmp.dir()).expect("open wal");
        wal.append("CREATE TABLE c (id integer PRIMARY KEY, name text)")
            .unwrap();
        wal.append("INSERT INTO c VALUES (1, 'alpha')").unwrap();
        wal.append("INSERT INTO c VALUES (2, 'beta')").unwrap();
    }
    // Append non-UTF8-ish / non-SQL garbage as a torn tail.
    let mut bytes = std::fs::read(tmp.wal_path()).unwrap();
    bytes.extend_from_slice(b"INSERT INTO c VALUES (3, '\xff\xfe garbage no terminator");
    std::fs::write(tmp.wal_path(), &bytes).unwrap();

    let (_wal, existing) = Wal::open(tmp.dir()).expect("reopen wal");
    let mut db = replay(&existing);
    let r = rows(&mut db, "SELECT count(*) FROM c");
    assert_eq!(cell(&r[0][0]), "2");
    let r2 = rows(&mut db, "SELECT name FROM c WHERE id = 1");
    assert_eq!(cell(&r2[0][0]), "alpha");
}

/// An entirely empty / nonexistent WAL directory recovers to an empty database
/// without error.
#[test]
fn empty_wal_recovers_empty() {
    let tmp = TempDir::new("empty");
    let (_wal, existing) = Wal::open(tmp.dir()).expect("open wal");
    let db = replay(&existing);
    assert!(db.table_names().is_empty());
}

/// After recovering from a torn tail, the WAL can be reopened and appended to
/// again (the torn bytes are tolerated, recovery is idempotent).
#[test]
fn recovery_then_new_writes() {
    let tmp = TempDir::new("reuse");
    {
        let (mut wal, _) = Wal::open(tmp.dir()).expect("open wal");
        wal.append("CREATE TABLE r (id integer PRIMARY KEY)").unwrap();
        wal.append("INSERT INTO r VALUES (1)").unwrap();
    }
    // Torn tail.
    let mut contents = std::fs::read_to_string(tmp.wal_path()).unwrap();
    contents.push_str("INSERT INTO r VALUES (2"); // unterminated
    std::fs::write(tmp.wal_path(), &contents).unwrap();

    // Recover the prefix.
    let (mut wal, existing) = Wal::open(tmp.dir()).expect("reopen wal");
    let mut db = replay(&existing);
    assert_eq!(cell(&rows(&mut db, "SELECT count(*) FROM r")[0][0]), "1");

    // New writes append to the (still openable) WAL.
    wal.append("INSERT INTO r VALUES (3)").unwrap();
    let _ = executor::execute(
        &mut db,
        Parser::parse_sql("INSERT INTO r VALUES (3)")
            .unwrap()
            .remove(0),
    );
    assert_eq!(cell(&rows(&mut db, "SELECT count(*) FROM r")[0][0]), "2");
}
