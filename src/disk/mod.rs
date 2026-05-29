//! Page-based on-disk persistence subsystem (opt-in via `PGRS_DISK`).
//!
//! This is an *additive* alternative to the logical SQL WAL ([`crate::wal`]).
//! It is built from four layers, each independently unit-tested:
//!
//! - [`page`]: the fixed 8 KB slotted page format (item ids + tuple data).
//! - [`heap`]: a heap file = a sequence of pages, with append/read/delete.
//! - [`buffer`]: an LRU page cache over a heap, tracking dirty pages.
//! - [`pwal`]: a physical, segmented WAL with truncation/compaction.
//!
//! [`DiskStore`] ties them together to implement **checkpoints** and
//! **recovery**: at `CHECKPOINT` every table's rows are serialized to per-table
//! heap files via the buffer manager, a small catalog records each table's
//! schema, a checkpoint marker is written to the physical WAL, and the WAL is
//! truncated before it. On startup [`DiskStore::recover`] rebuilds a
//! [`Database`] from the catalog + heap files and replays the physical-WAL tail.
//!
//! The in-memory query path and the logical WAL are untouched; this subsystem
//! only runs when the server is started with `PGRS_DISK` set to a directory.

pub mod buffer;
pub mod heap;
pub mod page;
pub mod pwal;

use std::io;
use std::path::{Path, PathBuf};

use crate::storage::{Column, Database, Table};
use crate::types::{DataType, Value};

use buffer::BufferManager;
use heap::Heap;
use pwal::{PhysicalWal, RecordKind};

/// Pool size (pages) for the buffer manager used during checkpoint/recovery.
const POOL_PAGES: usize = 64;

// --- value (tuple) codec -----------------------------------------------------
//
// A row is encoded as a length-prefixed sequence of typed cells. Each cell is
// one type tag byte followed by its payload. This round-trips Int/Float/Text/
// Bool/Null exactly. Floats are stored bit-for-bit so NaN/Inf survive.

const TAG_NULL: u8 = 0;
const TAG_INT: u8 = 1;
const TAG_FLOAT: u8 = 2;
const TAG_TEXT: u8 = 3;
const TAG_BOOL: u8 = 4;
const TAG_NUMERIC: u8 = 5;

/// Serialize a row of [`Value`]s into a self-describing tuple byte string.
pub fn encode_row(row: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(row.len() as u32).to_le_bytes());
    for cell in row {
        match cell {
            Value::Null => out.push(TAG_NULL),
            Value::Int(i) => {
                out.push(TAG_INT);
                out.extend_from_slice(&i.to_le_bytes());
            }
            Value::Float(f) => {
                out.push(TAG_FLOAT);
                out.extend_from_slice(&f.to_bits().to_le_bytes());
            }
            Value::Text(s) => {
                out.push(TAG_TEXT);
                out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            Value::Bool(b) => {
                out.push(TAG_BOOL);
                out.push(*b as u8);
            }
            // Numeric is persisted as its exact canonical decimal string.
            Value::Numeric(n) => {
                out.push(TAG_NUMERIC);
                let s = n.to_canonical_string();
                out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
        }
    }
    out
}

/// Decode a tuple produced by [`encode_row`]. Returns `None` on malformed input.
pub fn decode_row(bytes: &[u8]) -> Option<Vec<Value>> {
    let mut pos = 0usize;
    let n = u32::from_le_bytes(bytes.get(0..4)?.try_into().ok()?) as usize;
    pos += 4;
    let mut row = Vec::with_capacity(n);
    for _ in 0..n {
        let tag = *bytes.get(pos)?;
        pos += 1;
        let v = match tag {
            TAG_NULL => Value::Null,
            TAG_INT => {
                let v = i64::from_le_bytes(bytes.get(pos..pos + 8)?.try_into().ok()?);
                pos += 8;
                Value::Int(v)
            }
            TAG_FLOAT => {
                let bits = u64::from_le_bytes(bytes.get(pos..pos + 8)?.try_into().ok()?);
                pos += 8;
                Value::Float(f64::from_bits(bits))
            }
            TAG_TEXT => {
                let len = u32::from_le_bytes(bytes.get(pos..pos + 4)?.try_into().ok()?) as usize;
                pos += 4;
                let s = std::str::from_utf8(bytes.get(pos..pos + len)?).ok()?;
                pos += len;
                Value::Text(s.to_string())
            }
            TAG_BOOL => {
                let b = *bytes.get(pos)?;
                pos += 1;
                Value::Bool(b != 0)
            }
            TAG_NUMERIC => {
                let len = u32::from_le_bytes(bytes.get(pos..pos + 4)?.try_into().ok()?) as usize;
                pos += 4;
                let s = std::str::from_utf8(bytes.get(pos..pos + len)?).ok()?;
                pos += len;
                Value::Numeric(crate::numeric::BigDecimal::parse(s)?)
            }
            _ => return None,
        };
        row.push(v);
    }
    Some(row)
}

// --- catalog -----------------------------------------------------------------

/// Persisted schema of one table: enough to rebuild it (name + columns).
#[derive(Debug, Clone, PartialEq)]
struct TableCatalog {
    name: String,
    columns: Vec<(String, DataType, bool)>, // (name, type, not_null)
}

/// The whole on-disk catalog: every checkpointed table's schema, written as one
/// small text file (`catalog`). Text keeps it trivially round-trippable and
/// independent of the binary tuple format.
fn write_catalog(path: &Path, tables: &[TableCatalog]) -> io::Result<()> {
    let mut s = String::new();
    for t in tables {
        s.push_str("table\t");
        s.push_str(&escape(&t.name));
        s.push('\n');
        for (name, dt, not_null) in &t.columns {
            s.push_str("col\t");
            s.push_str(&escape(name));
            s.push('\t');
            s.push_str(dt.pg_type_name());
            s.push('\t');
            s.push_str(if *not_null { "1" } else { "0" });
            s.push('\n');
        }
    }
    std::fs::write(path, s)
}

fn read_catalog(path: &Path) -> io::Result<Vec<TableCatalog>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut tables: Vec<TableCatalog> = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        match parts.as_slice() {
            ["table", name] => tables.push(TableCatalog {
                name: unescape(name),
                columns: Vec::new(),
            }),
            ["col", name, ty, nn] => {
                if let Some(t) = tables.last_mut() {
                    let dt = datatype_from_pg_name(ty).unwrap_or(DataType::Text);
                    t.columns.push((unescape(name), dt, *nn == "1"));
                }
            }
            _ => {}
        }
    }
    Ok(tables)
}

/// Map a `pg_type.typname` back to a [`DataType`]. Built by reusing the forward
/// mapping over the known type list.
fn datatype_from_pg_name(name: &str) -> Option<DataType> {
    DataType::ALL.iter().copied().find(|dt| dt.pg_type_name() == name)
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n")
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A heap file name for `table`, sanitized so it is a safe filename.
fn heap_file_name(table: &str) -> String {
    let safe: String = table
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    format!("{safe}.heap")
}

// --- disk store: checkpoint + recovery ---------------------------------------

/// The disk persistence subsystem rooted at a directory. Holds the physical WAL
/// and orchestrates checkpoints and recovery over per-table heap files.
pub struct DiskStore {
    dir: PathBuf,
    wal: PhysicalWal,
    /// Monotonic checkpoint id, surfaced in the WAL checkpoint record.
    checkpoint_id: u64,
}

impl DiskStore {
    /// Open (creating if needed) the disk store under `dir`.
    pub fn open(dir: impl AsRef<Path>) -> io::Result<DiskStore> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let wal = PhysicalWal::open(dir.join("pwal"))?;
        Ok(DiskStore {
            dir,
            wal,
            checkpoint_id: 0,
        })
    }

    fn catalog_path(&self) -> PathBuf {
        self.dir.join("catalog")
    }

    fn heap_path(&self, table: &str) -> PathBuf {
        self.dir.join(heap_file_name(table))
    }

    /// Flush every table in `db` to its heap file (via the buffer manager),
    /// rewrite the catalog, record a checkpoint LSN in the physical WAL, and
    /// truncate the WAL before it. Returns the checkpoint LSN.
    pub fn checkpoint(&mut self, db: &Database) -> io::Result<u64> {
        let mut catalog = Vec::new();

        for name in db.table_names() {
            let Some(table) = db.table(&name) else { continue };
            catalog.push(TableCatalog {
                name: name.clone(),
                columns: table
                    .columns
                    .iter()
                    .map(|c| (c.name.clone(), c.data_type, c.not_null))
                    .collect(),
            });

            // Rewrite the heap from scratch each checkpoint: truncate and append
            // every current (detoasted) row through the buffer manager.
            let path = self.heap_path(&name);
            std::fs::write(&path, [])?; // truncate to empty
            let heap = Heap::open(&path)?;
            let mut buf = BufferManager::new(heap, POOL_PAGES);
            for row in &table.rows {
                let materialized = table.detoast_row(row);
                let tuple = encode_row(&materialized);
                append_via_buffer(&mut buf, &tuple)?;
            }
            buf.flush_all()?;
        }

        write_catalog(&self.catalog_path(), &catalog)?;

        // Record the checkpoint in the physical WAL, then truncate before it.
        self.checkpoint_id += 1;
        let lsn = self
            .wal
            .append(RecordKind::Checkpoint, self.checkpoint_id, 0, &[])?;
        self.wal.sync()?;
        self.wal.truncate_before(lsn)?;
        Ok(lsn)
    }

    /// Rebuild a [`Database`] from the on-disk catalog + heap files, then replay
    /// the physical-WAL tail. Tables with no catalog entry are skipped.
    pub fn recover(&self) -> io::Result<Database> {
        let mut db = Database::new();
        let catalog = read_catalog(&self.catalog_path())?;

        for tc in &catalog {
            let columns: Vec<Column> = tc
                .columns
                .iter()
                .map(|(n, dt, nn)| Column::basic(n.clone(), *dt, *nn))
                .collect();
            let mut table = Table::new(tc.name.clone(), columns);

            let path = self.heap_path(&tc.name);
            if path.exists() {
                let mut heap = Heap::open(&path)?;
                for (_, _, bytes) in heap.iter_tuples()? {
                    if let Some(row) = decode_row(&bytes) {
                        table.push_row(row);
                    }
                }
            }
            // Best-effort: a duplicate name shouldn't abort recovery.
            let _ = db.create_table(table);
        }

        // Replay the physical-WAL tail. The checkpoint already materialized all
        // committed state into the heaps, so post-checkpoint records (page
        // images / slot writes) are the only ones that could carry newer data;
        // we surface them for completeness but the heap is the source of truth
        // here, so replay is a no-op application in this checkpoint-only mode.
        self.wal.replay(|_rec| {})?;

        Ok(db)
    }

    /// Take a base backup: checkpoint `db` so the on-disk files are current,
    /// then copy the entire data directory (heap files, catalog, and physical
    /// WAL segments) into `dest`. A fresh server can be started from `dest`
    /// with [`DiskStore::open`] + [`DiskStore::recover`], reproducing the state
    /// at backup time — the physical analogue of `pg_basebackup`.
    pub fn base_backup(&mut self, db: &Database, dest: impl AsRef<Path>) -> io::Result<()> {
        self.checkpoint(db)?;
        self.wal.sync()?;
        let dest = dest.as_ref();
        std::fs::create_dir_all(dest)?;
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                std::fs::copy(entry.path(), dest.join(entry.file_name()))?;
            }
        }
        Ok(())
    }
}

/// Append one tuple via the buffer manager, allocating a new buffered page when
/// the tail page is full. Keeps all writes going through the cache so eviction
/// and `flush_all` exercise the real page path.
fn append_via_buffer(buf: &mut BufferManager, tuple: &[u8]) -> io::Result<()> {
    let pages = buf.page_count();
    if pages > 0 {
        let last = pages - 1;
        let page = buf.get_page(last)?;
        if page.insert_tuple(tuple).is_some() {
            buf.mark_dirty(last);
            return Ok(());
        }
    }
    let new_no = buf.new_page()?;
    let page = buf.get_page(new_no)?;
    if page.insert_tuple(tuple).is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tuple larger than a page",
        ));
    }
    buf.mark_dirty(new_no);
    Ok(())
}

// --- per-test unique temp directory ------------------------------------------

/// A unique, freshly created temp directory under the OS temp dir, tagged for
/// readability. Uses an atomic counter (Date/random are unavailable) plus the
/// pid so concurrent test binaries don't collide. The caller removes it.
#[cfg(test)]
pub(crate) fn test_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("pgrs_disk_{tag}_{pid}_{n}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_codec_round_trips_all_value_kinds() {
        let row = vec![
            Value::Null,
            Value::Int(-42),
            Value::Float(3.5),
            Value::Float(f64::INFINITY),
            Value::Text("héllo\tworld".into()),
            Value::Bool(true),
            Value::Bool(false),
            Value::Text(String::new()),
        ];
        let bytes = encode_row(&row);
        assert_eq!(decode_row(&bytes), Some(row));
    }

    #[test]
    fn decode_rejects_truncated_tuple() {
        let bytes = encode_row(&[Value::Int(7), Value::Text("abc".into())]);
        assert!(decode_row(&bytes[..bytes.len() - 2]).is_none());
    }

    #[test]
    fn catalog_round_trips() {
        let dir = test_dir("catalog");
        let path = dir.join("catalog");
        let tables = vec![
            TableCatalog {
                name: "weird\tname".into(),
                columns: vec![
                    ("id".into(), DataType::Int8, true),
                    ("label".into(), DataType::Text, false),
                ],
            },
            TableCatalog {
                name: "flags".into(),
                columns: vec![("ok".into(), DataType::Bool, false)],
            },
        ];
        write_catalog(&path, &tables).unwrap();
        assert_eq!(read_catalog(&path).unwrap(), tables);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// End-to-end: build a Database, checkpoint it to disk, load a fresh
    /// Database from that dir, and assert tables + rows match exactly.
    #[test]
    fn checkpoint_recovery_round_trip() {
        let dir = test_dir("e2e");

        let mut db = Database::new();
        let users = Table::new(
            "users".into(),
            vec![
                Column::basic("id".into(), DataType::Int8, true),
                Column::basic("name".into(), DataType::Text, false),
                Column::basic("score".into(), DataType::Float8, false),
                Column::basic("active".into(), DataType::Bool, false),
            ],
        );
        db.create_table(users).unwrap();
        // Enough rows to span multiple pages.
        for i in 0..500i64 {
            db.table_mut("users").unwrap().push_row(vec![
                Value::Int(i),
                Value::Text(format!("user-{i}-{}", "x".repeat(40))),
                Value::Float(i as f64 * 1.5),
                Value::Bool(i % 2 == 0),
            ]);
        }
        let empty = Table::new(
            "empty".into(),
            vec![Column::basic("c".into(), DataType::Int4, false)],
        );
        db.create_table(empty).unwrap();

        // Checkpoint to disk.
        {
            let mut store = DiskStore::open(&dir).unwrap();
            let lsn = store.checkpoint(&db).unwrap();
            assert!(lsn >= 1);
        }

        // Load a fresh database from the same directory.
        let store = DiskStore::open(&dir).unwrap();
        let recovered = store.recover().unwrap();

        assert_eq!(recovered.table_names(), vec!["empty", "users"]);

        let orig = db.table("users").unwrap();
        let got = recovered.table("users").unwrap();
        assert_eq!(got.columns.len(), orig.columns.len());
        for (a, b) in got.columns.iter().zip(&orig.columns) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.data_type, b.data_type);
            assert_eq!(a.not_null, b.not_null);
        }
        // Rows match exactly, in order.
        let orig_rows: Vec<Vec<Value>> =
            orig.rows.iter().map(|r| orig.detoast_row(r)).collect();
        let got_rows: Vec<Vec<Value>> =
            got.rows.iter().map(|r| got.detoast_row(r)).collect();
        assert_eq!(got_rows, orig_rows);

        // The empty table survives with its schema and no rows.
        assert!(recovered.table("empty").unwrap().rows.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A second checkpoint of the same store reflects the updated data (rewrite
    /// semantics) rather than appending stale rows.
    #[test]
    fn second_checkpoint_rewrites_heap() {
        let dir = test_dir("e2e_rewrite");
        let mut db = Database::new();
        db.create_table(Table::new(
            "t".into(),
            vec![Column::basic("v".into(), DataType::Int8, false)],
        ))
        .unwrap();
        for i in 0..10 {
            db.table_mut("t").unwrap().push_row(vec![Value::Int(i)]);
        }

        let mut store = DiskStore::open(&dir).unwrap();
        store.checkpoint(&db).unwrap();

        // Delete half the rows, then checkpoint again.
        db.table_mut("t").unwrap().delete_rows(&[0, 1, 2, 3, 4]);
        store.checkpoint(&db).unwrap();

        let recovered = DiskStore::open(&dir).unwrap().recover().unwrap();
        let rows = &recovered.table("t").unwrap().rows;
        assert_eq!(rows.len(), 5);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn heap_file_name_is_sanitized() {
        assert_eq!(heap_file_name("users"), "users.heap");
        assert_eq!(heap_file_name("my table!"), "my_table_.heap");
    }

    /// A base backup copies the data directory so a fresh store opened on the
    /// copy recovers the exact state at backup time.
    #[test]
    fn base_backup_round_trips_to_a_fresh_dir() {
        let src = test_dir("bb_src");
        let dst = test_dir("bb_dst");

        let mut db = Database::new();
        db.create_table(Table::new(
            "t".into(),
            vec![
                Column::basic("id".into(), DataType::Int8, false),
                Column::basic("name".into(), DataType::Text, false),
            ],
        ))
        .unwrap();
        for i in 0..120i64 {
            db.table_mut("t")
                .unwrap()
                .push_row(vec![Value::Int(i), Value::Text(format!("n{i}"))]);
        }

        // Take the backup into a separate directory.
        {
            let mut store = DiskStore::open(&src).unwrap();
            store.base_backup(&db, &dst).unwrap();
        }

        // A brand-new store opened on the backup recovers the data.
        let recovered = DiskStore::open(&dst).unwrap().recover().unwrap();
        let rows = &recovered.table("t").unwrap().rows;
        assert_eq!(rows.len(), 120);
        assert_eq!(rows[0], vec![Value::Int(0), Value::Text("n0".into())]);
        assert_eq!(rows[119], vec![Value::Int(119), Value::Text("n119".into())]);

        std::fs::remove_dir_all(&src).ok();
        std::fs::remove_dir_all(&dst).ok();
    }
}
