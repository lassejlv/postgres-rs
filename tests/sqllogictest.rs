//! A minimal, dependency-free sqllogictest-style runner.
//!
//! Supports the common subset of the sqllogictest format:
//!
//! ```text
//! statement ok
//! <sql>
//!
//! statement error
//! <sql>
//!
//! query <typestring>
//! <sql>
//! ----
//! <expected value>   (one per line, row-major; "NULL" for SQL NULL)
//! ```
//!
//! Records are separated by blank lines. Lines starting with `#` are comments.
//! The `<typestring>` (e.g. `ITT`) documents the column types; we use only its
//! length to know how many values make up one row when comparing.
//!
//! Each `.slt` file runs against a fresh in-memory [`Database`].

use std::path::{Path, PathBuf};

use postgres_rs::executor::{self, ExecResult};
use postgres_rs::sql::Parser;
use postgres_rs::storage::Database;
use postgres_rs::types::Value;

#[derive(Debug)]
enum Record {
    /// `statement ok` — SQL must execute without error.
    StatementOk(String),
    /// `statement error` — SQL must fail.
    StatementError(String),
    /// `query <types>` — SQL must produce exactly `expected` cells (row-major).
    Query {
        sql: String,
        col_count: usize,
        expected: Vec<String>,
    },
}

/// Parse a `.slt` source into a list of records.
fn parse_slt(src: &str) -> Vec<Record> {
    let mut records = Vec::new();
    let lines: Vec<&str> = src.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end();
        // Skip blank lines and comments between records.
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            i += 1;
            continue;
        }

        let mut header = line.split_whitespace();
        let kind = header.next().unwrap_or("");
        match kind {
            "statement" => {
                let mode = header.next().unwrap_or("ok");
                i += 1;
                let sql = collect_sql(&lines, &mut i);
                if mode == "error" {
                    records.push(Record::StatementError(sql));
                } else {
                    records.push(Record::StatementOk(sql));
                }
            }
            "query" => {
                let types = header.next().unwrap_or("");
                let col_count = types.len().max(1);
                i += 1;
                // SQL lines until the `----` separator.
                let mut sql_lines = Vec::new();
                while i < lines.len() && lines[i].trim() != "----" {
                    sql_lines.push(lines[i]);
                    i += 1;
                }
                let sql = sql_lines.join("\n").trim().to_string();
                // Skip the `----` line.
                if i < lines.len() {
                    i += 1;
                }
                // Expected values: every line until a blank line / EOF.
                let mut expected = Vec::new();
                while i < lines.len() && !lines[i].trim().is_empty() {
                    expected.push(lines[i].to_string());
                    i += 1;
                }
                records.push(Record::Query {
                    sql,
                    col_count,
                    expected,
                });
            }
            _ => {
                // Unknown directive: skip the line.
                i += 1;
            }
        }
    }
    records
}

/// Collect a single SQL statement (lines until a blank line / EOF).
fn collect_sql(lines: &[&str], i: &mut usize) -> String {
    let mut sql_lines = Vec::new();
    while *i < lines.len() && !lines[*i].trim().is_empty() {
        sql_lines.push(lines[*i]);
        *i += 1;
    }
    sql_lines.join("\n").trim().to_string()
}

/// Execute all statements in `sql` against `db`, returning the last result.
fn exec_all(db: &mut Database, sql: &str) -> Result<ExecResult, String> {
    let mut last = ExecResult::Empty;
    for stmt in Parser::parse_sql(sql)? {
        last = executor::execute(db, stmt)?;
    }
    Ok(last)
}

/// Render one cell exactly as the .slt files expect ("NULL" for SQL NULL).
fn render(v: &Value) -> String {
    v.to_text().unwrap_or_else(|| "NULL".into())
}

/// Run a single `.slt` file, panicking with a descriptive message on mismatch.
fn run_file(path: &Path) {
    let src = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let records = parse_slt(&src);
    let mut db = Database::new();
    let fname = path.file_name().unwrap().to_string_lossy();

    for (idx, rec) in records.iter().enumerate() {
        match rec {
            Record::StatementOk(sql) => {
                if let Err(e) = exec_all(&mut db, sql) {
                    panic!("[{fname} #{idx}] statement expected ok but failed: {e}\n  SQL: {sql}");
                }
            }
            Record::StatementError(sql) => {
                if exec_all(&mut db, sql).is_ok() {
                    panic!("[{fname} #{idx}] statement expected error but succeeded\n  SQL: {sql}");
                }
            }
            Record::Query {
                sql,
                col_count,
                expected,
            } => {
                let res = exec_all(&mut db, sql)
                    .unwrap_or_else(|e| panic!("[{fname} #{idx}] query failed: {e}\n  SQL: {sql}"));
                let rows = match res {
                    ExecResult::Rows { rows, .. } => rows,
                    other => {
                        let kind = match other {
                            ExecResult::Command(c) => format!("command {c}"),
                            ExecResult::Empty => "empty".into(),
                            ExecResult::Rows { .. } => unreachable!(),
                        };
                        panic!("[{fname} #{idx}] query produced no rowset ({kind})\n  SQL: {sql}");
                    }
                };
                // Flatten the result row-major into individual cells.
                let actual: Vec<String> =
                    rows.iter().flat_map(|r| r.iter().map(render)).collect();
                assert_eq!(
                    actual, *expected,
                    "[{fname} #{idx}] result mismatch (cols={col_count})\n  SQL: {sql}\n  expected: {expected:?}\n  actual:   {actual:?}"
                );
            }
        }
    }
}

/// Discover and run every `.slt` file under tests/slt/.
#[test]
fn run_all_slt_files() {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("tests/slt");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "slt").unwrap_or(false))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no .slt files found in {}", dir.display());
    for f in files {
        run_file(&f);
    }
}
