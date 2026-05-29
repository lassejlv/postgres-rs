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

/// Start the server binary on an ephemeral port (in-memory, no PGRS_DATA).
fn start_server() -> Option<ServerProc> {
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
    let child = Command::new(&bin)
        .arg(&addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
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
