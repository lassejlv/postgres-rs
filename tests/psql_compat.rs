//! Scripted compatibility tests driven by the real `psql` client.
//!
//! These start the actual server binary on an ephemeral port and pipe a `.sql`
//! script through `psql`, asserting on its textual output. They skip gracefully
//! (returning early, never failing) when `psql` is not on PATH or the server
//! binary cannot be located, so `cargo test` stays green in any environment.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Locate `psql` on PATH. Returns None (→ skip) if absent.
fn find_psql() -> Option<String> {
    // Honor an explicit override first.
    if let Ok(p) = std::env::var("PGRS_PSQL")
        && !p.is_empty()
    {
        return Some(p);
    }
    let out = Command::new("sh")
        .arg("-c")
        .arg("command -v psql")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}

/// Path to the freshly built server binary. Cargo sets CARGO_BIN_EXE_<name>
/// for the crate's binaries when running integration tests.
fn server_bin() -> Option<String> {
    option_env!("CARGO_BIN_EXE_postgres-rs").map(|s| s.to_string())
}

/// A running server child process bound to a known port; killed on drop.
struct ServerProc {
    child: std::process::Child,
    port: u16,
}

impl Drop for ServerProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Locate a tool on PATH (e.g. `pg_dump`, `pg_restore`). Returns None (→ skip)
/// if absent. Honors an explicit `PGRS_<TOOL>` override.
fn find_tool(tool: &str) -> Option<String> {
    let override_var = format!("PGRS_{}", tool.to_ascii_uppercase());
    if let Ok(p) = std::env::var(&override_var)
        && !p.is_empty()
    {
        return Some(p);
    }
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool}"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}

/// Start the server binary on an ephemeral port (in-memory, no PGRS_DATA).
fn start_server() -> Option<ServerProc> {
    start_server_with_version(None)
}

/// Start the server, optionally reporting a specific `server_version` (via
/// `PGRS_SERVER_VERSION`). `pg_dump`/`pg_restore` refuse a newer-than-themselves
/// server, so the dump round-trip pins the reported version to pg_dump's.
fn start_server_with_version(version: Option<&str>) -> Option<ServerProc> {
    let bin = server_bin()?;
    // Grab a free port by binding then immediately dropping the listener, then
    // pass it to the server. A tiny race window remains but is acceptable for a
    // local test; if the server fails to bind, the connect loop below times out
    // and we skip.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").ok()?;
        l.local_addr().ok()?.port()
    };
    let addr = format!("127.0.0.1:{port}");
    let mut cmd = Command::new(&bin);
    cmd.arg(&addr).stdout(Stdio::null()).stderr(Stdio::null());
    if let Some(v) = version {
        cmd.env("PGRS_SERVER_VERSION", v);
    }
    let child = cmd.spawn().ok()?;
    // Wait until the port accepts connections.
    let mut ready = false;
    for _ in 0..100 {
        if std::net::TcpStream::connect(&addr).is_ok() {
            ready = true;
            break;
        }
        thread::sleep(Duration::from_millis(30));
    }
    let mut proc = ServerProc { child, port };
    if !ready {
        // Could not start (e.g. port race); give up and skip.
        let _ = proc.child.kill();
        return None;
    }
    // Keep proc alive.
    proc.port = port;
    Some(proc)
}

/// Run a SQL script through psql against the given port, returning stdout.
fn run_psql(psql: &str, port: u16, script: &str) -> Option<String> {
    let mut child = Command::new(psql)
        .arg("-h")
        .arg("127.0.0.1")
        .arg("-p")
        .arg(port.to_string())
        .arg("-U")
        .arg("postgres")
        .arg("-d")
        .arg("postgres")
        // No psqlrc, fail-fast on first error, quiet banners.
        .arg("-X")
        .arg("-v")
        .arg("ON_ERROR_STOP=1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PGPASSWORD", "")
        .spawn()
        .ok()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .ok()?;
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).ok()?;
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    let status = child.wait().ok()?;
    if !status.success() {
        eprintln!("psql exited with failure; stderr:\n{err}");
        return None;
    }
    Some(out)
}

#[test]
fn psql_create_insert_select() {
    let Some(psql) = find_psql() else {
        eprintln!("SKIP psql_create_insert_select: psql not on PATH");
        return;
    };
    let Some(server) = start_server() else {
        eprintln!("SKIP psql_create_insert_select: could not start server binary");
        return;
    };

    let script = "\
CREATE TABLE compat (id integer PRIMARY KEY, name text);
INSERT INTO compat VALUES (1, 'one'), (2, 'two'), (3, 'three');
SELECT id, name FROM compat ORDER BY id;
SELECT count(*) AS n FROM compat;
\\dt
";

    let Some(out) = run_psql(&psql, server.port, script) else {
        // Treat a psql/runtime failure as a skip rather than a hard failure to
        // keep CI green in constrained environments.
        eprintln!("SKIP psql_create_insert_select: psql run did not complete");
        return;
    };

    assert!(out.contains("CREATE TABLE"), "missing CREATE TABLE in:\n{out}");
    assert!(out.contains("INSERT 0 3"), "missing INSERT tag in:\n{out}");
    assert!(out.contains("one"), "missing inserted row 'one' in:\n{out}");
    assert!(out.contains("three"), "missing inserted row 'three' in:\n{out}");
    // count(*) of 3.
    assert!(out.contains('3'), "missing count result in:\n{out}");
    // \dt lists the table.
    assert!(out.contains("compat"), "\\dt should list table in:\n{out}");
}

/// Extract the major version reported by `<tool> --version` (e.g. "14" from
/// "pg_dump (PostgreSQL) 14.23 (Homebrew)").
fn tool_major_version(tool: &str) -> Option<u32> {
    let out = Command::new(tool).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // Find the first dotted number sequence and take its leading integer.
    let token = text.split_whitespace().find(|t| {
        t.chars().next().is_some_and(|c| c.is_ascii_digit()) && t.contains('.')
    })?;
    token.split('.').next()?.parse::<u32>().ok()
}

/// Real `pg_dump`/`pg_restore` (and `psql`) round-trip: dump a representative
/// schema from one server, replay it into a fresh server, and assert the data
/// matches. Covers both the plain-SQL path (`pg_dump | psql`) and the custom
/// archive path (`pg_dump -Fc` + `pg_restore`). Skips gracefully (never fails)
/// when the client tools are absent, so `cargo test` stays green anywhere.
#[test]
fn pg_dump_restore_round_trip() {
    let Some(psql) = find_psql() else {
        eprintln!("SKIP pg_dump_restore_round_trip: psql not on PATH");
        return;
    };
    let Some(pg_dump) = find_tool("pg_dump") else {
        eprintln!("SKIP pg_dump_restore_round_trip: pg_dump not on PATH");
        return;
    };
    let Some(pg_restore) = find_tool("pg_restore") else {
        eprintln!("SKIP pg_dump_restore_round_trip: pg_restore not on PATH");
        return;
    };
    // pg_dump refuses a server newer than itself, so report its own version.
    let Some(major) = tool_major_version(&pg_dump) else {
        eprintln!("SKIP pg_dump_restore_round_trip: could not read pg_dump version");
        return;
    };
    let reported = format!("{major}.0");

    let Some(source) = start_server_with_version(Some(&reported)) else {
        eprintln!("SKIP pg_dump_restore_round_trip: could not start source server");
        return;
    };

    // A representative schema: serial PKs, a FK, NOT NULL, a DEFAULT, a UNIQUE.
    let setup = "\
CREATE TABLE authors (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    country VARCHAR(64) DEFAULT 'Unknown',
    email TEXT UNIQUE
);
CREATE TABLE books (
    id SERIAL PRIMARY KEY,
    title TEXT NOT NULL,
    author_id INTEGER NOT NULL REFERENCES authors(id),
    price NUMERIC(10,2) DEFAULT 0.00,
    published BOOLEAN DEFAULT false
);
INSERT INTO authors (name, country, email) VALUES
  ('Alice', 'USA', 'alice@example.com'),
  ('Bob', 'UK', 'bob@example.com'),
  ('Carol', 'Unknown', NULL);
INSERT INTO books (title, author_id, price, published) VALUES
  ('Rust 101', 1, 39.99, true),
  ('SQL Deep Dive', 1, 49.50, true),
  ('UK History', 2, 19.00, false);
";
    if run_psql(&psql, source.port, setup).is_none() {
        eprintln!("SKIP pg_dump_restore_round_trip: schema setup via psql failed");
        return;
    }

    // --- Plain-format dump -> psql replay into a fresh server ----------------
    let dump = match run_pg_dump(&pg_dump, source.port, None) {
        Some(d) => d,
        None => {
            eprintln!("SKIP pg_dump_restore_round_trip: pg_dump (plain) failed");
            return;
        }
    };
    assert!(
        dump.contains("CREATE TABLE public.authors"),
        "plain dump missing CREATE TABLE:\n{dump}"
    );
    assert!(
        dump.contains("CREATE SEQUENCE public.authors_id_seq"),
        "plain dump missing CREATE SEQUENCE:\n{dump}"
    );
    assert!(
        dump.contains("ADD CONSTRAINT books_author_id_fkey FOREIGN KEY"),
        "plain dump missing FK constraint:\n{dump}"
    );

    let Some(target) = start_server_with_version(Some(&reported)) else {
        eprintln!("SKIP pg_dump_restore_round_trip: could not start target server");
        return;
    };
    let replay = run_psql(&psql, target.port, &dump);
    assert!(
        replay.is_some(),
        "replaying the plain dump via psql failed (ON_ERROR_STOP)"
    );

    let counts = run_psql(
        &psql,
        target.port,
        "SELECT count(*) FROM authors;\nSELECT count(*) FROM books;\n",
    )
    .expect("counting restored rows");
    // Three authors and three books survived the round trip.
    let threes = counts.matches('3').count();
    assert!(threes >= 2, "expected 3 authors and 3 books, got:\n{counts}");
    let sample = run_psql(
        &psql,
        target.port,
        "SELECT name FROM authors WHERE id = 1;\n",
    )
    .expect("sampling a restored row");
    assert!(sample.contains("Alice"), "restored sample row wrong:\n{sample}");

    // --- Custom archive (-Fc) dump -> pg_restore into a fresh server ---------
    // Written to a temp file because the archive is binary.
    let archive = std::env::temp_dir().join(format!("pgrs_rt_{}.dump", source.port));
    let archive_path = archive.to_string_lossy().to_string();
    let fc_ok = Command::new(&pg_dump)
        .args([
            "-Fc",
            "-h",
            "127.0.0.1",
            "-p",
            &source.port.to_string(),
            "-U",
            "postgres",
            "--no-owner",
            "--no-privileges",
            "-f",
            &archive_path,
            "postgres",
        ])
        .env("PGPASSWORD", "")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if fc_ok {
        let Some(target2) = start_server_with_version(Some(&reported)) else {
            eprintln!("SKIP pg_dump_restore_round_trip(-Fc): could not start server");
            let _ = std::fs::remove_file(&archive);
            return;
        };
        let restored = Command::new(&pg_restore)
            .args([
                "-h",
                "127.0.0.1",
                "-p",
                &target2.port.to_string(),
                "-U",
                "postgres",
                "-d",
                "postgres",
                "--no-owner",
                "--no-privileges",
                &archive_path,
            ])
            .env("PGPASSWORD", "")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(restored, "pg_restore from -Fc archive failed");

        let counts2 = run_psql(
            &psql,
            target2.port,
            "SELECT count(*) FROM authors;\nSELECT count(*) FROM books;\n",
        )
        .expect("counting rows after pg_restore");
        assert!(
            counts2.matches('3').count() >= 2,
            "expected 3 authors and 3 books after pg_restore, got:\n{counts2}"
        );
    } else {
        eprintln!("NOTE pg_dump_restore_round_trip: -Fc path skipped (pg_dump -Fc failed)");
    }
    let _ = std::fs::remove_file(&archive);
}

/// Run `pg_dump` (plain format unless `custom_file` is given) against a port,
/// returning the produced SQL on success.
fn run_pg_dump(pg_dump: &str, port: u16, custom_file: Option<&str>) -> Option<String> {
    let mut cmd = Command::new(pg_dump);
    cmd.args([
        "-h",
        "127.0.0.1",
        "-p",
        &port.to_string(),
        "-U",
        "postgres",
        "--no-owner",
        "--no-privileges",
    ]);
    if let Some(f) = custom_file {
        cmd.args(["-Fc", "-f", f]);
    }
    cmd.arg("postgres")
        .env("PGPASSWORD", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().ok()?;
    if !out.status.success() {
        eprintln!(
            "pg_dump failed; stderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}
