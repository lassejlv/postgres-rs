//! Fixed-size on-disk page format (PostgreSQL "slotted page" layout).
//!
//! Each page is exactly [`PAGE_SIZE`] (8192) bytes:
//!
//! ```text
//! +-----------------------------------------------------------+
//! | PageHeader (24 bytes)                                      |
//! +-----------------------------------------------------------+
//! | ItemId[0] | ItemId[1] | ... ->  (grows forward, "lower")   |
//! |                                                            |
//! |                  ... free space ...                        |
//! |                                                            |
//! |  <- tuple data grows backward ("upper") ... tuple1 tuple0  |
//! +-----------------------------------------------------------+
//! ```
//!
//! Line pointers ([`ItemId`], 4 bytes each: a 16-bit offset + 16-bit length)
//! grow forward from just after the header; tuple bytes grow backward from the
//! end. `lower` is the offset of the end of the line-pointer array, `upper` is
//! the offset of the start of the lowest tuple. Free space is `upper - lower`.
//!
//! The header carries a page LSN (for WAL ordering) and a checksum over the
//! page body so corruption can be detected on read.

/// The fixed page size, matching PostgreSQL's default `BLCKSZ`.
pub const PAGE_SIZE: usize = 8192;

/// Size of the page header in bytes.
pub const HEADER_SIZE: usize = 24;

/// Size of one line pointer (item id) in bytes.
pub const ITEM_ID_SIZE: usize = 4;

/// A line pointer: where a tuple lives within the page and how long it is.
/// A length of 0 marks a *dead* slot (its tuple was deleted); the slot number
/// is preserved so later slots keep their identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ItemId {
    offset: u16,
    length: u16,
}

/// The 24-byte page header.
#[derive(Debug, Clone, Copy)]
struct PageHeader {
    /// Log-sequence number of the last WAL record that modified this page.
    lsn: u64,
    /// Bit flags (reserved; currently always 0).
    flags: u16,
    /// Offset to the end of the line-pointer array (start of free space).
    lower: u16,
    /// Offset to the start of tuple data (end of free space).
    upper: u16,
    /// Offset to the special space at the very end of the page (always
    /// `PAGE_SIZE` here, since we have no index special space).
    special: u16,
}

/// An in-memory, mutable page. Serialize with [`Page::to_bytes`] and read back
/// with [`Page::from_bytes`].
#[derive(Debug, Clone)]
pub struct Page {
    header: PageHeader,
    ids: Vec<ItemId>,
    /// Tuple payloads, parallel to `ids`. A dead slot keeps an empty `Vec`.
    tuples: Vec<Vec<u8>>,
}

impl Page {
    /// A fresh empty page.
    pub fn new() -> Self {
        Page {
            header: PageHeader {
                lsn: 0,
                flags: 0,
                lower: HEADER_SIZE as u16,
                upper: PAGE_SIZE as u16,
                special: PAGE_SIZE as u16,
            },
            ids: Vec::new(),
            tuples: Vec::new(),
        }
    }

    /// The page's LSN.
    pub fn lsn(&self) -> u64 {
        self.header.lsn
    }

    /// Set the page's LSN (called when a WAL record modifies the page).
    pub fn set_lsn(&mut self, lsn: u64) {
        self.header.lsn = lsn;
    }

    /// Number of slots (live or dead) ever allocated on this page.
    pub fn slot_count(&self) -> usize {
        self.ids.len()
    }

    /// Bytes currently free for a *new* tuple, accounting for the line pointer
    /// the insert would also consume.
    pub fn free_space(&self) -> usize {
        let lower = self.header.lower as usize;
        let upper = self.header.upper as usize;
        upper.saturating_sub(lower)
    }

    /// Whether a tuple of `len` bytes fits (needs `len` for data + one item id).
    pub fn can_fit(&self, len: usize) -> bool {
        self.free_space() >= len + ITEM_ID_SIZE
    }

    /// Insert a tuple, returning its slot number, or `None` if it doesn't fit.
    pub fn insert_tuple(&mut self, data: &[u8]) -> Option<usize> {
        if !self.can_fit(data.len()) {
            return None;
        }
        let new_upper = self.header.upper as usize - data.len();
        let slot = self.ids.len();
        self.ids.push(ItemId {
            offset: new_upper as u16,
            length: data.len() as u16,
        });
        self.tuples.push(data.to_vec());
        self.header.upper = new_upper as u16;
        self.header.lower += ITEM_ID_SIZE as u16;
        Some(slot)
    }

    /// Read the live tuple at `slot`, or `None` if the slot is out of range or
    /// dead (deleted).
    pub fn get_tuple(&self, slot: usize) -> Option<&[u8]> {
        let id = self.ids.get(slot)?;
        if id.length == 0 {
            return None;
        }
        Some(&self.tuples[slot])
    }

    /// Mark the tuple at `slot` dead. Returns whether a live tuple was removed.
    /// The slot id is retained (length set to 0) so later slot numbers are
    /// stable; the freed data bytes are reclaimed on the next [`Page::compact`].
    pub fn delete_tuple(&mut self, slot: usize) -> bool {
        match self.ids.get_mut(slot) {
            Some(id) if id.length != 0 => {
                id.length = 0;
                id.offset = 0;
                self.tuples[slot] = Vec::new();
                true
            }
            _ => false,
        }
    }

    /// Number of live (non-deleted) tuples.
    pub fn live_count(&self) -> usize {
        self.ids.iter().filter(|i| i.length != 0).count()
    }

    /// Iterate `(slot, bytes)` over all live tuples in slot order.
    pub fn iter_live(&self) -> impl Iterator<Item = (usize, &[u8])> {
        self.ids.iter().enumerate().filter_map(move |(slot, id)| {
            if id.length == 0 {
                None
            } else {
                Some((slot, self.tuples[slot].as_slice()))
            }
        })
    }

    /// Serialize the page to exactly [`PAGE_SIZE`] bytes, computing the checksum.
    pub fn to_bytes(&self) -> [u8; PAGE_SIZE] {
        let mut buf = [0u8; PAGE_SIZE];
        // Header (checksum written last, once the body is in place).
        buf[0..8].copy_from_slice(&self.header.lsn.to_le_bytes());
        // bytes 8..12 reserved for checksum
        buf[12..14].copy_from_slice(&self.header.flags.to_le_bytes());
        buf[14..16].copy_from_slice(&self.header.lower.to_le_bytes());
        buf[16..18].copy_from_slice(&self.header.upper.to_le_bytes());
        buf[18..20].copy_from_slice(&self.header.special.to_le_bytes());
        // bytes 20..24 reserved (slot count, for robustness on read)
        buf[20..24].copy_from_slice(&(self.ids.len() as u32).to_le_bytes());

        // Line-pointer array.
        for (i, id) in self.ids.iter().enumerate() {
            let base = HEADER_SIZE + i * ITEM_ID_SIZE;
            buf[base..base + 2].copy_from_slice(&id.offset.to_le_bytes());
            buf[base + 2..base + 4].copy_from_slice(&id.length.to_le_bytes());
        }
        // Tuple data (each live tuple at its recorded offset).
        for (i, id) in self.ids.iter().enumerate() {
            if id.length == 0 {
                continue;
            }
            let off = id.offset as usize;
            buf[off..off + id.length as usize].copy_from_slice(&self.tuples[i]);
        }

        let checksum = page_checksum(&buf);
        buf[8..12].copy_from_slice(&checksum.to_le_bytes());
        buf
    }

    /// Parse a page from exactly [`PAGE_SIZE`] bytes, verifying the checksum.
    /// Returns `Err` if the length is wrong or the checksum does not match
    /// (corruption).
    pub fn from_bytes(buf: &[u8]) -> Result<Page, PageError> {
        if buf.len() != PAGE_SIZE {
            return Err(PageError::BadLength(buf.len()));
        }
        let stored = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let computed = page_checksum(buf);
        if stored != computed {
            return Err(PageError::Checksum {
                stored,
                computed,
            });
        }
        let lsn = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let flags = u16::from_le_bytes([buf[12], buf[13]]);
        let lower = u16::from_le_bytes([buf[14], buf[15]]);
        let upper = u16::from_le_bytes([buf[16], buf[17]]);
        let special = u16::from_le_bytes([buf[18], buf[19]]);
        let n_slots = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]) as usize;

        let mut ids = Vec::with_capacity(n_slots);
        let mut tuples = Vec::with_capacity(n_slots);
        for i in 0..n_slots {
            let base = HEADER_SIZE + i * ITEM_ID_SIZE;
            if base + ITEM_ID_SIZE > PAGE_SIZE {
                return Err(PageError::Malformed);
            }
            let offset = u16::from_le_bytes([buf[base], buf[base + 1]]);
            let length = u16::from_le_bytes([buf[base + 2], buf[base + 3]]);
            if length == 0 {
                ids.push(ItemId { offset: 0, length: 0 });
                tuples.push(Vec::new());
                continue;
            }
            let off = offset as usize;
            let end = off + length as usize;
            if end > PAGE_SIZE || off < HEADER_SIZE {
                return Err(PageError::Malformed);
            }
            ids.push(ItemId { offset, length });
            tuples.push(buf[off..end].to_vec());
        }

        Ok(Page {
            header: PageHeader {
                lsn,
                flags,
                lower,
                upper,
                special,
            },
            ids,
            tuples,
        })
    }
}

impl Default for Page {
    fn default() -> Self {
        Page::new()
    }
}

/// Errors from reading a page off disk.
#[derive(Debug, PartialEq, Eq)]
pub enum PageError {
    /// The buffer was not exactly [`PAGE_SIZE`] bytes.
    BadLength(usize),
    /// The stored checksum did not match the recomputed one (corruption).
    Checksum { stored: u32, computed: u32 },
    /// The line-pointer array referenced bytes outside the page.
    Malformed,
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PageError::BadLength(n) => write!(f, "page is {n} bytes, expected {PAGE_SIZE}"),
            PageError::Checksum { stored, computed } => {
                write!(f, "page checksum mismatch: stored {stored:#x}, computed {computed:#x}")
            }
            PageError::Malformed => write!(f, "page line pointers are malformed"),
        }
    }
}

impl std::error::Error for PageError {}

/// A simple FNV-1a checksum over the page, treating the 4 checksum bytes
/// (offsets 8..12) as zero so it is stable regardless of what is stored there.
fn page_checksum(buf: &[u8]) -> u32 {
    const OFFSET: u32 = 0x811c_9dc5;
    const PRIME: u32 = 0x0100_0193;
    let mut hash = OFFSET;
    for (i, &b) in buf.iter().enumerate() {
        let b = if (8..12).contains(&i) { 0 } else { b };
        hash ^= b as u32;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_read_delete() {
        let mut page = Page::new();
        let a = page.insert_tuple(b"alpha").unwrap();
        let b = page.insert_tuple(b"beta").unwrap();
        assert_eq!(page.get_tuple(a), Some(&b"alpha"[..]));
        assert_eq!(page.get_tuple(b), Some(&b"beta"[..]));
        assert_eq!(page.live_count(), 2);

        assert!(page.delete_tuple(a));
        assert_eq!(page.get_tuple(a), None);
        // The surviving slot keeps its number and value.
        assert_eq!(page.get_tuple(b), Some(&b"beta"[..]));
        assert_eq!(page.live_count(), 1);
        // Deleting again is a no-op.
        assert!(!page.delete_tuple(a));
    }

    #[test]
    fn fills_to_capacity() {
        let mut page = Page::new();
        let tuple = [7u8; 100];
        let mut count = 0;
        while page.insert_tuple(&tuple).is_some() {
            count += 1;
        }
        // Each tuple needs 100 + 4 bytes; the usable area is PAGE_SIZE - HEADER.
        let usable = PAGE_SIZE - HEADER_SIZE;
        assert_eq!(count, usable / (100 + ITEM_ID_SIZE));
        // A further insert genuinely fails, and free space is below the bar.
        assert!(page.insert_tuple(&tuple).is_none());
        assert!(!page.can_fit(tuple.len()));
    }

    #[test]
    fn byte_round_trip() {
        let mut page = Page::new();
        page.set_lsn(42);
        page.insert_tuple(b"one").unwrap();
        let mid = page.insert_tuple(b"two-deleted").unwrap();
        page.insert_tuple(b"three").unwrap();
        page.delete_tuple(mid);

        let bytes = page.to_bytes();
        assert_eq!(bytes.len(), PAGE_SIZE);
        let back = Page::from_bytes(&bytes).unwrap();
        assert_eq!(back.lsn(), 42);
        assert_eq!(back.slot_count(), 3);
        assert_eq!(back.get_tuple(0), Some(&b"one"[..]));
        assert_eq!(back.get_tuple(1), None); // still dead
        assert_eq!(back.get_tuple(2), Some(&b"three"[..]));
        let live: Vec<&[u8]> = back.iter_live().map(|(_, b)| b).collect();
        assert_eq!(live, vec![&b"one"[..], &b"three"[..]]);
    }

    #[test]
    fn checksum_detects_corruption() {
        let mut page = Page::new();
        page.insert_tuple(b"important").unwrap();
        let mut bytes = page.to_bytes();
        // A clean read succeeds.
        assert!(Page::from_bytes(&bytes).is_ok());
        // Flip a byte in the tuple data region; the checksum must catch it.
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        match Page::from_bytes(&bytes) {
            Err(PageError::Checksum { .. }) => {}
            other => panic!("expected checksum error, got {other:?}"),
        }
    }

    #[test]
    fn bad_length_rejected() {
        let short = vec![0u8; PAGE_SIZE - 1];
        assert_eq!(Page::from_bytes(&short).unwrap_err(), PageError::BadLength(PAGE_SIZE - 1));
    }
}
