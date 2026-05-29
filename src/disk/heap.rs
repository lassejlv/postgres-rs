//! Disk-backed heap file: a sequence of [`Page`]s stored in one file.
//!
//! A heap file is a flat array of [`PAGE_SIZE`]-byte pages. Page `n` lives at
//! byte offset `n * PAGE_SIZE`. Tuples are appended into the last page that has
//! room; a new page is allocated (and zero-initialized via a written page) when
//! the current tail is full. Each tuple is addressed by a `(page_no, slot)`
//! pair which is stable across reopens.
//!
//! This layer does its own direct `seek`/`read`/`write` at page granularity;
//! the [`crate::disk::buffer`] buffer manager sits on top of the same on-disk
//! format for cached, batched access.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::page::{Page, PAGE_SIZE};

/// A heap file open for reading and writing.
pub struct Heap {
    file: File,
    path: PathBuf,
    /// Number of pages currently in the file.
    page_count: usize,
}

impl Heap {
    /// Open the heap at `path`, creating an empty file if it does not exist.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Heap> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let len = file.metadata()?.len() as usize;
        let page_count = len / PAGE_SIZE;
        Ok(Heap {
            file,
            path,
            page_count,
        })
    }

    /// Number of pages in the file.
    pub fn page_count(&self) -> usize {
        self.page_count
    }

    /// Read page `page_no` from disk.
    pub fn read_page(&mut self, page_no: usize) -> io::Result<Page> {
        if page_no >= self.page_count {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("page {page_no} out of range ({} pages)", self.page_count),
            ));
        }
        let mut buf = [0u8; PAGE_SIZE];
        self.file.seek(SeekFrom::Start((page_no * PAGE_SIZE) as u64))?;
        self.file.read_exact(&mut buf)?;
        Page::from_bytes(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Write `page` back to slot `page_no`, extending the file if needed.
    pub fn write_page(&mut self, page_no: usize, page: &Page) -> io::Result<()> {
        let bytes = page.to_bytes();
        self.file.seek(SeekFrom::Start((page_no * PAGE_SIZE) as u64))?;
        self.file.write_all(&bytes)?;
        if page_no >= self.page_count {
            self.page_count = page_no + 1;
        }
        Ok(())
    }

    /// Append `tuple`, allocating a new page if the tail page is full (or the
    /// file is empty). Returns the `(page_no, slot)` address.
    pub fn append_tuple(&mut self, tuple: &[u8]) -> io::Result<(usize, usize)> {
        // Try the current tail page first.
        if self.page_count > 0 {
            let last = self.page_count - 1;
            let mut page = self.read_page(last)?;
            if let Some(slot) = page.insert_tuple(tuple) {
                self.write_page(last, &page)?;
                return Ok((last, slot));
            }
        }
        // Allocate a fresh page.
        let mut page = Page::new();
        let slot = page.insert_tuple(tuple).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "tuple larger than a page",
            )
        })?;
        let page_no = self.page_count;
        self.write_page(page_no, &page)?;
        Ok((page_no, slot))
    }

    /// Read the live tuple at `(page_no, slot)`, or `None` if absent/deleted.
    pub fn read_tuple(&mut self, page_no: usize, slot: usize) -> io::Result<Option<Vec<u8>>> {
        if page_no >= self.page_count {
            return Ok(None);
        }
        let page = self.read_page(page_no)?;
        Ok(page.get_tuple(slot).map(|b| b.to_vec()))
    }

    /// Delete the tuple at `(page_no, slot)`. Returns whether one was removed.
    pub fn delete_tuple(&mut self, page_no: usize, slot: usize) -> io::Result<bool> {
        if page_no >= self.page_count {
            return Ok(false);
        }
        let mut page = self.read_page(page_no)?;
        if page.delete_tuple(slot) {
            self.write_page(page_no, &page)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Collect every live tuple in the heap, in `(page_no, slot)` order.
    pub fn iter_tuples(&mut self) -> io::Result<Vec<(usize, usize, Vec<u8>)>> {
        let mut out = Vec::new();
        for page_no in 0..self.page_count {
            let page = self.read_page(page_no)?;
            for (slot, bytes) in page.iter_live() {
                out.push((page_no, slot, bytes.to_vec()));
            }
        }
        Ok(out)
    }

    /// fsync the heap file's data and metadata.
    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }

    /// The file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::test_dir;

    #[test]
    fn append_spans_multiple_pages_and_reads_back() {
        let dir = test_dir("heap_span");
        let path = dir.join("t.heap");
        let mut heap = Heap::open(&path).unwrap();

        // ~400-byte tuples force allocation of several pages.
        let n = 100;
        let mut addrs = Vec::new();
        for i in 0..n {
            let tuple = format!("row-{i:04}-{}", "x".repeat(380));
            addrs.push((heap.append_tuple(tuple.as_bytes()).unwrap(), tuple));
        }
        assert!(heap.page_count() >= 2, "expected multiple pages");

        for ((page_no, slot), expected) in &addrs {
            let got = heap.read_tuple(*page_no, *slot).unwrap().unwrap();
            assert_eq!(got, expected.as_bytes());
        }
        // iter_tuples returns every live tuple.
        assert_eq!(heap.iter_tuples().unwrap().len(), n);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_then_iterate_skips_dead() {
        let dir = test_dir("heap_delete");
        let path = dir.join("t.heap");
        let mut heap = Heap::open(&path).unwrap();
        let mut addrs = Vec::new();
        for i in 0..20 {
            addrs.push(heap.append_tuple(format!("v{i}").as_bytes()).unwrap());
        }
        // Delete every other tuple.
        for (page_no, slot) in addrs.iter().step_by(2) {
            assert!(heap.delete_tuple(*page_no, *slot).unwrap());
        }
        let live = heap.iter_tuples().unwrap();
        assert_eq!(live.len(), 10);
        for (_, _, bytes) in &live {
            let s = String::from_utf8(bytes.clone()).unwrap();
            let n: usize = s.trim_start_matches('v').parse().unwrap();
            assert_eq!(n % 2, 1, "only odd-indexed survive");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopen_persists_data() {
        let dir = test_dir("heap_reopen");
        let path = dir.join("t.heap");
        let addr;
        {
            let mut heap = Heap::open(&path).unwrap();
            addr = heap.append_tuple(b"durable").unwrap();
            heap.append_tuple(b"second").unwrap();
            heap.sync().unwrap();
        }
        // Reopen a fresh handle and the data is still there.
        let mut heap = Heap::open(&path).unwrap();
        assert_eq!(heap.read_tuple(addr.0, addr.1).unwrap().unwrap(), b"durable");
        assert_eq!(heap.iter_tuples().unwrap().len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
