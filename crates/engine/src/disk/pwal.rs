//! Physical write-ahead log: page-level records with monotonic LSNs, split
//! across fixed-size segment files.
//!
//! Distinct from the *logical* SQL WAL ([`crate::wal`]). Each record is a
//! page-granularity change ("page P slot S = bytes", "page P written", or a
//! checkpoint marker) carrying a monotonically increasing LSN. Records are
//! length-prefixed binary so a torn/partial trailing record (from a crash) is
//! detected and the well-formed prefix still replays.
//!
//! ## On-disk format
//!
//! The log lives in a directory as numbered segment files
//! `seg-00000000.wal`, `seg-00000001.wal`, ... Each appended record is:
//!
//! ```text
//! [u32 payload_len][u32 crc][u64 lsn][u8 kind][u64 page_no][u32 slot][payload bytes]
//! ```
//!
//! `payload_len` covers everything after the `payload_len`+`crc` prefix (the
//! fixed header fields plus the trailing payload), and `crc` is a checksum over
//! those same bytes. On replay a record whose declared length runs past the
//! file end, or whose crc mismatches, terminates that segment's replay (the
//! torn tail), and replay stops.
//!
//! When appending would overflow the current segment past [`SEGMENT_SIZE`], a
//! new segment file is started. [`PhysicalWal::truncate_before`] deletes whole
//! segments that lie entirely before a checkpoint LSN.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Segment rollover size. Small so tests can span several segments cheaply.
pub const SEGMENT_SIZE: u64 = 1 << 20; // 1 MiB

/// The kind of a physical WAL record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    /// A tuple was written to a page slot (payload = tuple bytes).
    PageSlotWrite,
    /// A whole page image was written (payload = page bytes).
    PageWrite,
    /// A checkpoint marker (payload empty); `page_no` carries the checkpoint id.
    Checkpoint,
}

impl RecordKind {
    fn to_byte(self) -> u8 {
        match self {
            RecordKind::PageSlotWrite => 1,
            RecordKind::PageWrite => 2,
            RecordKind::Checkpoint => 3,
        }
    }
    fn from_byte(b: u8) -> Option<RecordKind> {
        Some(match b {
            1 => RecordKind::PageSlotWrite,
            2 => RecordKind::PageWrite,
            3 => RecordKind::Checkpoint,
            _ => return None,
        })
    }
}

/// A decoded physical WAL record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub lsn: u64,
    pub kind: RecordKind,
    pub page_no: u64,
    pub slot: u32,
    pub payload: Vec<u8>,
}

/// An append-only physical WAL over a directory of segment files.
pub struct PhysicalWal {
    dir: PathBuf,
    /// Currently open tail segment for appends.
    current: File,
    current_seg: u64,
    current_bytes: u64,
    next_lsn: u64,
}

impl PhysicalWal {
    /// Open (creating if needed) the physical WAL in `dir`. Resumes appending at
    /// the highest existing segment and the next LSN after the last good record.
    pub fn open(dir: impl AsRef<Path>) -> io::Result<PhysicalWal> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let segments = list_segments(&dir)?;
        let (current_seg, next_lsn) = match segments.last() {
            Some(&seg) => {
                // Recover the next LSN from the last good record across all segs.
                let mut max_lsn = 0u64;
                for &s in &segments {
                    for rec in read_segment(&segment_path(&dir, s))? {
                        max_lsn = max_lsn.max(rec.lsn);
                    }
                }
                (seg, max_lsn + 1)
            }
            None => (0, 1),
        };
        let path = segment_path(&dir, current_seg);
        let current = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)?;
        let current_bytes = current.metadata()?.len();
        Ok(PhysicalWal {
            dir,
            current,
            current_seg,
            current_bytes,
            next_lsn,
        })
    }

    /// The LSN that the next appended record will receive.
    pub fn next_lsn(&self) -> u64 {
        self.next_lsn
    }

    /// The current tail segment number.
    pub fn current_segment(&self) -> u64 {
        self.current_seg
    }

    /// Append a record, returning the assigned LSN. Rolls over to a new segment
    /// when the current one would exceed [`SEGMENT_SIZE`].
    pub fn append(
        &mut self,
        kind: RecordKind,
        page_no: u64,
        slot: u32,
        payload: &[u8],
    ) -> io::Result<u64> {
        let lsn = self.next_lsn;
        let bytes = encode_record(lsn, kind, page_no, slot, payload);

        // Roll over if this record would push us past the segment size (but
        // always allow at least one record per segment).
        if self.current_bytes > 0 && self.current_bytes + bytes.len() as u64 > SEGMENT_SIZE {
            self.roll_segment()?;
        }
        self.current.write_all(&bytes)?;
        self.current_bytes += bytes.len() as u64;
        self.next_lsn += 1;
        Ok(lsn)
    }

    /// fsync the current segment.
    pub fn sync(&mut self) -> io::Result<()> {
        self.current.flush()?;
        self.current.sync_all()
    }

    fn roll_segment(&mut self) -> io::Result<()> {
        self.current.sync_all()?;
        self.current_seg += 1;
        let path = segment_path(&self.dir, self.current_seg);
        self.current = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)?;
        self.current_bytes = 0;
        Ok(())
    }

    /// Replay every well-formed record in LSN order, invoking `apply` on each.
    /// A torn/corrupt trailing record stops replay (its prefix is preserved).
    pub fn replay<F>(&self, mut apply: F) -> io::Result<u64>
    where
        F: FnMut(&Record),
    {
        let mut count = 0;
        for seg in list_segments(&self.dir)? {
            for rec in read_segment(&segment_path(&self.dir, seg))? {
                apply(&rec);
                count += 1;
            }
        }
        Ok(count)
    }

    /// Segment numbers currently on disk, ascending.
    pub fn segments(&self) -> io::Result<Vec<u64>> {
        list_segments(&self.dir)
    }

    /// Delete WAL segments that lie entirely before the segment containing
    /// `lsn` (log truncation / compaction at a checkpoint). The segment holding
    /// `lsn` and everything after it is retained so the post-checkpoint tail
    /// still replays. Returns how many segments were removed.
    pub fn truncate_before(&mut self, lsn: u64) -> io::Result<usize> {
        let segments = list_segments(&self.dir)?;
        // Find the first segment whose record set includes an LSN >= `lsn`; keep
        // it and everything after. Never remove the current tail segment.
        let mut keep_from = self.current_seg;
        for &seg in &segments {
            let recs = read_segment(&segment_path(&self.dir, seg))?;
            let has_at_or_after = recs.iter().any(|r| r.lsn >= lsn);
            if has_at_or_after {
                keep_from = keep_from.min(seg);
                break;
            }
        }
        let mut removed = 0;
        for &seg in &segments {
            if seg < keep_from && seg != self.current_seg {
                fs::remove_file(segment_path(&self.dir, seg))?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

fn segment_path(dir: &Path, seg: u64) -> PathBuf {
    dir.join(format!("seg-{seg:08}.wal"))
}

/// List existing segment numbers in ascending order.
fn list_segments(dir: &Path) -> io::Result<Vec<u64>> {
    let mut segs = Vec::new();
    if !dir.exists() {
        return Ok(segs);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(num) = name.strip_prefix("seg-").and_then(|s| s.strip_suffix(".wal"))
            && let Ok(n) = num.parse::<u64>()
        {
            segs.push(n);
        }
    }
    segs.sort_unstable();
    Ok(segs)
}

fn encode_record(lsn: u64, kind: RecordKind, page_no: u64, slot: u32, payload: &[u8]) -> Vec<u8> {
    // Body = fixed header tail (lsn..slot) + payload; len & crc cover the body.
    let body_len = (8 + 1 + 8 + 4 + payload.len()) as u32;
    let mut body = Vec::with_capacity(body_len as usize);
    body.extend_from_slice(&lsn.to_le_bytes());
    body.push(kind.to_byte());
    body.extend_from_slice(&page_no.to_le_bytes());
    body.extend_from_slice(&slot.to_le_bytes());
    body.extend_from_slice(payload);
    let crc = crc32(&body);

    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&body_len.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Read every well-formed record from one segment file, stopping at the first
/// torn/corrupt record (returning the good prefix).
fn read_segment(path: &Path) -> io::Result<Vec<Record>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let body_len =
            u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
                as usize;
        let crc = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]);
        let body_start = pos + 8;
        let body_end = body_start + body_len;
        // Torn: declared body runs past EOF.
        if body_end > bytes.len() || body_len < (8 + 1 + 8 + 4) {
            break;
        }
        let body = &bytes[body_start..body_end];
        if crc32(body) != crc {
            break; // corrupt record — stop here.
        }
        let lsn = u64::from_le_bytes(body[0..8].try_into().unwrap());
        let Some(kind) = RecordKind::from_byte(body[8]) else {
            break;
        };
        let page_no = u64::from_le_bytes(body[9..17].try_into().unwrap());
        let slot = u32::from_le_bytes(body[17..21].try_into().unwrap());
        let payload = body[21..].to_vec();
        out.push(Record {
            lsn,
            kind,
            page_no,
            slot,
            payload,
        });
        pos = body_end;
    }
    Ok(out)
}

/// Standard CRC-32 (IEEE 802.3 polynomial) computed without a precomputed
/// table — small and dependency-free.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Append a single record to a segment file by raw bytes (test helper for
/// simulating a torn write). Not part of the public API.
#[cfg(test)]
fn raw_append(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = OpenOptions::new().append(true).create(true).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::test_dir;

    #[test]
    fn append_reopen_replay_in_order() {
        let dir = test_dir("pwal_order");
        {
            let mut wal = PhysicalWal::open(&dir).unwrap();
            assert_eq!(wal.append(RecordKind::PageSlotWrite, 0, 0, b"a").unwrap(), 1);
            assert_eq!(wal.append(RecordKind::PageSlotWrite, 0, 1, b"bb").unwrap(), 2);
            assert_eq!(wal.append(RecordKind::PageWrite, 1, 0, b"ccc").unwrap(), 3);
            wal.sync().unwrap();
        }
        let wal = PhysicalWal::open(&dir).unwrap();
        assert_eq!(wal.next_lsn(), 4); // resumed after lsn 3
        let mut seen = Vec::new();
        wal.replay(|r| seen.push((r.lsn, r.kind, r.payload.clone()))).unwrap();
        assert_eq!(
            seen,
            vec![
                (1, RecordKind::PageSlotWrite, b"a".to_vec()),
                (2, RecordKind::PageSlotWrite, b"bb".to_vec()),
                (3, RecordKind::PageWrite, b"ccc".to_vec()),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn torn_trailing_record_is_dropped() {
        let dir = test_dir("pwal_torn");
        {
            let mut wal = PhysicalWal::open(&dir).unwrap();
            wal.append(RecordKind::PageSlotWrite, 0, 0, b"good1").unwrap();
            wal.append(RecordKind::PageSlotWrite, 0, 1, b"good2").unwrap();
            wal.sync().unwrap();
        }
        // Simulate a crash mid-write: a full record encoding truncated halfway.
        let full = encode_record(3, RecordKind::PageSlotWrite, 0, 2, b"torn-tail");
        let half = &full[..full.len() / 2];
        raw_append(&segment_path(&dir, 0), half).unwrap();

        let wal = PhysicalWal::open(&dir).unwrap();
        let mut seen = Vec::new();
        wal.replay(|r| seen.push(r.payload.clone())).unwrap();
        assert_eq!(seen, vec![b"good1".to_vec(), b"good2".to_vec()]);
        // The good prefix's LSNs are recovered; next append continues cleanly.
        assert_eq!(wal.next_lsn(), 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn records_span_multiple_segments() {
        let dir = test_dir("pwal_segments");
        let mut wal = PhysicalWal::open(&dir).unwrap();
        // ~4 KB payloads; SEGMENT_SIZE is 1 MiB, so ~256 fill one segment.
        let payload = vec![0xABu8; 4096];
        let n = 600;
        for _ in 0..n {
            wal.append(RecordKind::PageWrite, 0, 0, &payload).unwrap();
        }
        wal.sync().unwrap();
        let segs = wal.segments().unwrap();
        assert!(segs.len() >= 2, "expected >= 2 segments, got {segs:?}");

        let wal2 = PhysicalWal::open(&dir).unwrap();
        let mut count = 0u64;
        wal2.replay(|_| count += 1).unwrap();
        assert_eq!(count, n);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncate_before_removes_old_segments_and_tail_replays() {
        let dir = test_dir("pwal_truncate");
        let mut wal = PhysicalWal::open(&dir).unwrap();
        let payload = vec![1u8; 4096];
        // Fill at least 3 segments.
        let mut checkpoint_lsn = 0;
        for i in 0..800 {
            let lsn = wal.append(RecordKind::PageWrite, 0, 0, &payload).unwrap();
            if i == 500 {
                checkpoint_lsn = lsn;
            }
        }
        wal.sync().unwrap();
        let before = wal.segments().unwrap();
        assert!(before.len() >= 3);

        let removed = wal.truncate_before(checkpoint_lsn).unwrap();
        assert!(removed >= 1, "expected to drop >= 1 segment");
        let after = wal.segments().unwrap();
        assert!(after.len() < before.len());

        // Everything from the checkpoint LSN onward still replays.
        let wal2 = PhysicalWal::open(&dir).unwrap();
        let mut min_lsn = u64::MAX;
        let mut max_lsn = 0;
        wal2.replay(|r| {
            min_lsn = min_lsn.min(r.lsn);
            max_lsn = max_lsn.max(r.lsn);
        })
        .unwrap();
        assert!(min_lsn <= checkpoint_lsn, "tail must still cover checkpoint");
        assert_eq!(max_lsn, 800);
        std::fs::remove_dir_all(&dir).ok();
    }
}
