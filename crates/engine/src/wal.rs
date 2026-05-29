//! Logical write-ahead log for durability.
//!
//! Each successful mutating statement is re-serialized to canonical SQL (see
//! [`crate::sql::serialize`]) and appended to an on-disk log, followed by an
//! `fsync`. On startup the log is replayed in order to reconstruct state.
//!
//! This is a *logical* log (statements, not physical pages). It is simple and
//! correct for a deterministic executor; a future iteration can add physical
//! page WAL, checkpoints, and log truncation. Today the log grows unbounded.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// An append-only logical WAL backed by a single file.
pub struct Wal {
    file: File,
    #[allow(dead_code)]
    path: PathBuf,
}

impl Wal {
    /// Open (creating if needed) the WAL inside `dir`. Returns the WAL handle
    /// and the existing log contents for the caller to replay.
    pub fn open(dir: &str) -> io::Result<(Wal, String)> {
        fs::create_dir_all(dir)?;
        let path = Path::new(dir).join("wal.sql");

        // Read the raw bytes and decode lossily rather than `read_to_string`,
        // which would reject the whole log on a single invalid byte. A crash can
        // leave a torn (partially written, possibly non-UTF8) tail; lossy
        // decoding preserves the well-formed prefix so replay can recover the
        // committed statements and drop only the torn fragment.
        let existing = match fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };

        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok((Wal { file, path }, existing))
    }

    /// Durably append one statement's SQL to the log.
    pub fn append(&mut self, sql: &str) -> io::Result<()> {
        if sql.is_empty() {
            return Ok(());
        }
        // Statements are terminated with `;` so the whole file re-parses as a
        // script on replay. String literals may contain `;`/newlines; the
        // lexer handles that since it tracks quotes.
        self.file.write_all(sql.as_bytes())?;
        self.file.write_all(b";\n")?;
        self.file.flush()?;
        // fsync so the write survives a crash/power loss.
        self.file.sync_data()
    }
}
