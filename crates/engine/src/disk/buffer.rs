//! Buffer manager: an LRU page cache over a [`Heap`] file.
//!
//! Pages are read from disk on a miss and held in a pool of fixed `capacity`.
//! When the pool is full and a new page must be admitted, the least-recently
//! used page is evicted; if it is dirty it is flushed to disk first so no
//! modification is lost. Callers mutate a page in place and call
//! [`BufferManager::mark_dirty`]; [`BufferManager::flush_all`] writes every
//! dirty page back and fsyncs.
//!
//! Recency is tracked with a monotonically increasing tick stamped on each
//! access; eviction picks the smallest tick. This is a simple but correct LRU
//! that needs no intrusive linked list.

use std::collections::HashMap;
use std::io;

use super::heap::Heap;
use super::page::Page;

struct Frame {
    page: Page,
    dirty: bool,
    /// Last-access tick for LRU ordering.
    used: u64,
}

/// An LRU page cache over a heap file.
pub struct BufferManager {
    heap: Heap,
    pool: HashMap<usize, Frame>,
    capacity: usize,
    clock: u64,
    /// Stats for tests/observability.
    hits: u64,
    misses: u64,
}

impl BufferManager {
    /// Wrap `heap` with a pool of at most `capacity` pages (>= 1).
    pub fn new(heap: Heap, capacity: usize) -> BufferManager {
        BufferManager {
            heap,
            pool: HashMap::new(),
            capacity: capacity.max(1),
            clock: 0,
            hits: 0,
            misses: 0,
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Number of pages resident in the pool.
    pub fn resident(&self) -> usize {
        self.pool.len()
    }

    /// Cache-hit / miss counters since construction.
    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }

    /// Total pages in the underlying heap.
    pub fn page_count(&self) -> usize {
        self.heap.page_count()
    }

    /// Ensure the page is resident, then return a mutable reference. Reads from
    /// disk on a miss, evicting the LRU page (flushing it first if dirty) when
    /// the pool is full.
    pub fn get_page(&mut self, page_no: usize) -> io::Result<&mut Page> {
        let tick = self.tick();
        if let Some(frame) = self.pool.get_mut(&page_no) {
            frame.used = tick;
            self.hits += 1;
            return Ok(&mut self.pool.get_mut(&page_no).unwrap().page);
        }
        self.misses += 1;
        let page = self.heap.read_page(page_no)?;
        self.admit(page_no, page, false)?;
        Ok(&mut self.pool.get_mut(&page_no).unwrap().page)
    }

    /// Allocate a fresh empty page at the end of the heap, admit it to the pool
    /// (dirty), and return its page number.
    pub fn new_page(&mut self) -> io::Result<usize> {
        let page_no = self.heap.page_count();
        // Reserve the slot on disk so page_count advances and reads are valid.
        let blank = Page::new();
        self.heap.write_page(page_no, &blank)?;
        self.admit(page_no, blank, true)?;
        Ok(page_no)
    }

    /// Admit a page to the pool, evicting the LRU frame first if at capacity.
    fn admit(&mut self, page_no: usize, page: Page, dirty: bool) -> io::Result<()> {
        if self.pool.len() >= self.capacity && !self.pool.contains_key(&page_no) {
            self.evict_one()?;
        }
        let used = self.tick();
        self.pool.insert(page_no, Frame { page, dirty, used });
        Ok(())
    }

    /// Evict the least-recently-used frame, flushing it if dirty.
    fn evict_one(&mut self) -> io::Result<()> {
        let victim = self
            .pool
            .iter()
            .min_by_key(|(_, f)| f.used)
            .map(|(&no, _)| no);
        if let Some(no) = victim {
            let frame = self.pool.remove(&no).unwrap();
            if frame.dirty {
                self.heap.write_page(no, &frame.page)?;
            }
        }
        Ok(())
    }

    /// Mark a resident page dirty so it is flushed on eviction / `flush_all`.
    pub fn mark_dirty(&mut self, page_no: usize) {
        if let Some(frame) = self.pool.get_mut(&page_no) {
            frame.dirty = true;
        }
    }

    /// Write every dirty page back to disk and fsync.
    pub fn flush_all(&mut self) -> io::Result<()> {
        let dirty: Vec<usize> = self
            .pool
            .iter()
            .filter(|(_, f)| f.dirty)
            .map(|(&no, _)| no)
            .collect();
        for no in dirty {
            let page = self.pool.get(&no).unwrap().page.clone();
            self.heap.write_page(no, &page)?;
            self.pool.get_mut(&no).unwrap().dirty = false;
        }
        self.heap.sync()
    }

    /// Borrow the underlying heap (after a flush) for direct iteration.
    pub fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::test_dir;

    fn heap_with_pages(path: &std::path::Path, n: usize) -> Heap {
        let mut heap = Heap::open(path).unwrap();
        for i in 0..n {
            let mut page = Page::new();
            page.insert_tuple(format!("page-{i}").as_bytes()).unwrap();
            heap.write_page(i, &page).unwrap();
        }
        heap.sync().unwrap();
        heap
    }

    #[test]
    fn hit_and_miss_counters() {
        let dir = test_dir("buf_hits");
        let path = dir.join("h");
        let heap = heap_with_pages(&path, 3);
        let mut buf = BufferManager::new(heap, 4);

        buf.get_page(0).unwrap(); // miss
        buf.get_page(0).unwrap(); // hit
        buf.get_page(1).unwrap(); // miss
        buf.get_page(0).unwrap(); // hit
        let (hits, misses) = buf.stats();
        assert_eq!(hits, 2);
        assert_eq!(misses, 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pool_never_exceeds_capacity() {
        let dir = test_dir("buf_cap");
        let path = dir.join("h");
        let heap = heap_with_pages(&path, 10);
        let mut buf = BufferManager::new(heap, 3);
        for i in 0..10 {
            buf.get_page(i).unwrap();
            assert!(buf.resident() <= 3, "pool exceeded capacity at {i}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn eviction_flushes_dirty_pages() {
        let dir = test_dir("buf_evict");
        let path = dir.join("h");
        let heap = heap_with_pages(&path, 5);
        let mut buf = BufferManager::new(heap, 2);

        // Modify page 0, mark dirty, then touch enough other pages to force its
        // eviction (which must flush it).
        let page0 = buf.get_page(0).unwrap();
        page0.insert_tuple(b"new-tuple").unwrap();
        buf.mark_dirty(0);
        for i in 1..5 {
            buf.get_page(i).unwrap();
        }
        // Drop the cache entirely and reopen from disk: the edit must be there.
        drop(buf);
        let mut heap = Heap::open(&path).unwrap();
        let page0 = heap.read_page(0).unwrap();
        let tuples: Vec<&[u8]> = page0.iter_live().map(|(_, b)| b).collect();
        assert!(tuples.contains(&&b"new-tuple"[..]), "dirty edit was lost");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn flush_all_persists_without_eviction() {
        let dir = test_dir("buf_flush");
        let path = dir.join("h");
        let heap = heap_with_pages(&path, 2);
        let mut buf = BufferManager::new(heap, 8);
        let p = buf.get_page(1).unwrap();
        p.insert_tuple(b"flushed").unwrap();
        buf.mark_dirty(1);
        buf.flush_all().unwrap();

        let mut heap = Heap::open(&path).unwrap();
        let page1 = heap.read_page(1).unwrap();
        let tuples: Vec<&[u8]> = page1.iter_live().map(|(_, b)| b).collect();
        assert!(tuples.contains(&&b"flushed"[..]));
        std::fs::remove_dir_all(&dir).ok();
    }
}
