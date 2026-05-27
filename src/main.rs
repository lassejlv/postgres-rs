//! postgres-rs: a fast, lightweight, PostgreSQL wire-compatible database.
//!
//! Run with an optional `host:port` (defaults to `127.0.0.1:5432`):
//!
//! ```text
//! cargo run --release -- 127.0.0.1:5433
//! ```
//!
//! Then connect with any PostgreSQL client, e.g.:
//!
//! ```text
//! psql -h 127.0.0.1 -p 5433 -U postgres
//! ```
//!
//! Persistence is enabled by setting `PGRS_DATA` to a directory; the database
//! then writes a WAL there and recovers from it on restart. Without it, the
//! database runs entirely in memory.

use std::process::ExitCode;

use postgres_rs::server;

fn main() -> ExitCode {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:5432".to_string());
    let data_dir = std::env::var("PGRS_DATA").ok();

    match server::run(&addr, data_dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("postgres-rs failed to start on {addr}: {e}");
            ExitCode::FAILURE
        }
    }
}
