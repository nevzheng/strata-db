//! The Page Cache — a fixed pool of in-memory frames over a [`BlockStore`].
//!
//! Callers [`read`](PageCache::read), [`write`](PageCache::write), or
//! [`allocate`](PageCache::allocate) a page by [`BlockId`] and get back a RAII
//! handle that pins the frame; the pin is released on drop. A miss evicts an
//! unpinned frame (writing it back first if dirty) and reads the page in.
//! Dirty pages reach disk on eviction or [`flush`](PageCache::flush).
//!
//! The cache owns page integrity: it finalizes the CRC32c on writeback and
//! verifies it on load, so page-type code never touches the checksum.
//!
//! ## v1 scope
//! Single-threaded ([`Rc`]/[`RefCell`]); the per-frame read/write latch is
//! modeled as a count, and a conflicting acquire fails with [`Error::Busy`]
//! rather than blocking. Concurrency, prefetch, and a separate scan-buffer pool
//! are deferred (see the Page Cache design doc); [`prefetch`](PageCache::prefetch)
//! is a no-op placeholder.

mod memo;
pub mod policies;

pub use memo::{Budget, Cache, Weight};

use std::cell::{Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use crate::block::journal::{BlockJournal, JournalOp};
use crate::cache::policies::{EvictionPolicy, FrameId, LruK};
use crate::error::Error;
use crate::page::{finalize_checksum, verify_checksum};
use crate::{Block, BlockId, BlockStore, HEADER_LEN, PageHeader, Result};

/// A frame's page-sized, aligned buffer. Shared (`Rc`) so a handle can keep
/// reading the bytes independently of the pool's own borrow; mutable
/// (`RefCell`) so reads and writes both go through it.  [`Block`]'s alignment
/// means the kernel can DMA directly to/from it for direct I/O.
type FrameBuf = Rc<RefCell<Block>>;

/// One slot in the pool: a page-sized buffer plus its bookkeeping.
///
/// `buf` is an `Rc<RefCell<…>>` so a handle can keep accessing the bytes
/// independently of the pool's own borrow. A frame is only ever evicted when
/// unpinned (`readers == 0 && !writer`), at which point no handle holds a clone,
/// so reusing the same allocation in place is safe.
struct Frame {
    page_id: Option<BlockId>,
    buf: FrameBuf,
    readers: u32,
    writer: bool,
    dirty: bool,
}

impl Frame {
    fn new() -> Self {
        Self {
            page_id: None,
            buf: Rc::new(RefCell::new(Block::zeroed())),
            readers: 0,
            writer: false,
            dirty: false,
        }
    }
}

/// The cache's mutable interior, shared with every outstanding handle.
struct Inner<V: BlockStore> {
    block: V,
    frames: Vec<Frame>,
    table: HashMap<BlockId, FrameId>,
    free: Vec<FrameId>,
    policy: Box<dyn EvictionPolicy<FrameId>>,
    /// The redo journal. `None` disables journaling (ephemeral/in-memory
    /// stores); `flush` then just writes through to the block store, with no crash
    /// atomicity beyond what the block store itself provides.
    journal: Option<BlockJournal>,
}

impl<V: BlockStore> Inner<V> {
    /// A free frame, evicting an unpinned one if the pool is full. The returned
    /// frame is clean and absent from the page table.
    ///
    /// No-steal: a dirty page is never written to the block store here — only in
    /// [`flush`](Inner::flush), behind the journal. So we evict a clean frame, and if
    /// none is available we flush first to clean the dirty ones.
    fn victim_frame(&mut self) -> Result<FrameId> {
        if let Some(f) = self.free.pop() {
            return Ok(f);
        }
        if let Some(f) = self.pick_evictable(true) {
            return Ok(self.reclaim(f));
        }
        // No clean victim. If there's a dirty unpinned frame, flushing makes it
        // clean and evictable; otherwise every frame is pinned and we're stuck.
        let has_dirty_unpinned = self
            .frames
            .iter()
            .any(|fr| fr.readers == 0 && !fr.writer && fr.dirty);
        if !has_dirty_unpinned {
            return Err(Error::PoolExhausted(self.frames.len()));
        }
        self.flush()?;
        let f = self
            .pick_evictable(false)
            .ok_or(Error::PoolExhausted(self.frames.len()))?;
        Ok(self.reclaim(f))
    }

    /// The eviction policy's victim among unpinned frames. With `clean_only`,
    /// also require the frame to be clean (no pending writeback).
    fn pick_evictable(&self, clean_only: bool) -> Option<FrameId> {
        let frames = &self.frames;
        self.policy.evict_candidate(&|f| {
            let fr = &frames[f];
            fr.readers == 0 && !fr.writer && (!clean_only || !fr.dirty)
        })
    }

    /// Evict frame `f` from the page table and reset it. The caller guarantees
    /// it is clean and unpinned, so there is nothing to write back.
    fn reclaim(&mut self, f: FrameId) -> FrameId {
        if let Some(old) = self.frames[f].page_id.take() {
            self.table.remove(&old);
        }
        self.policy.remove(f);
        let frame = &mut self.frames[f];
        frame.readers = 0;
        frame.writer = false;
        frame.dirty = false;
        f
    }

    /// Read page `id` from the block store into frame `f` and verify its checksum.
    fn load(&mut self, f: FrameId, id: BlockId) -> Result<()> {
        let buf = self.frames[f].buf.clone();
        self.block.read(id, &mut buf.borrow_mut())?;
        verify_checksum(&buf.borrow(), id)
    }

    fn acquire_read(&mut self, id: BlockId) -> Result<(FrameId, FrameBuf)> {
        if let Some(&f) = self.table.get(&id) {
            if self.frames[f].writer {
                return Err(Error::Busy(id));
            }
            self.frames[f].readers += 1;
            self.policy.record_access(f);
            return Ok((f, self.frames[f].buf.clone()));
        }
        let f = self.victim_frame()?;
        if let Err(e) = self.load(f, id) {
            self.free.push(f); // never entered the table; hand it back clean
            return Err(e);
        }
        let frame = &mut self.frames[f];
        frame.page_id = Some(id);
        frame.readers = 1;
        self.table.insert(id, f);
        self.policy.record_access(f);
        Ok((f, self.frames[f].buf.clone()))
    }

    fn acquire_write(&mut self, id: BlockId) -> Result<(FrameId, FrameBuf)> {
        if let Some(&f) = self.table.get(&id) {
            if self.frames[f].writer || self.frames[f].readers > 0 {
                return Err(Error::Busy(id));
            }
            self.frames[f].writer = true;
            self.policy.record_access(f);
            return Ok((f, self.frames[f].buf.clone()));
        }
        let f = self.victim_frame()?;
        if let Err(e) = self.load(f, id) {
            self.free.push(f);
            return Err(e);
        }
        let frame = &mut self.frames[f];
        frame.page_id = Some(id);
        frame.writer = true;
        self.table.insert(id, f);
        self.policy.record_access(f);
        Ok((f, self.frames[f].buf.clone()))
    }

    fn allocate(&mut self) -> Result<(BlockId, FrameId, FrameBuf)> {
        let id = self.block.allocate()?;
        let f = self.victim_frame()?;
        self.frames[f].buf.borrow_mut().fill(0);
        let frame = &mut self.frames[f];
        frame.page_id = Some(id);
        frame.writer = true;
        frame.dirty = true; // a fresh page should reach disk even if left untouched
        self.table.insert(id, f);
        self.policy.record_access(f);
        Ok((id, f, self.frames[f].buf.clone()))
    }

    fn release_read(&mut self, f: FrameId) {
        self.frames[f].readers -= 1;
    }

    fn release_write(&mut self, f: FrameId) {
        let frame = &mut self.frames[f];
        frame.writer = false;
        frame.dirty = true;
    }

    /// Commit all dirty pages durably. This is the journal commit point:
    /// log every after-image plus a `Commit` marker (the durability point),
    /// then write the pages to the block store, sync, and discard the now-redundant log.
    /// A crash before the `Commit` marker rolls the whole flush back on recovery.
    fn flush(&mut self) -> Result<()> {
        let dirty: Vec<FrameId> = (0..self.frames.len())
            .filter(|&f| self.frames[f].dirty && self.frames[f].page_id.is_some())
            .collect();
        if dirty.is_empty() {
            return Ok(());
        }

        // Finalize each checksum first, so the image we log is byte-identical to
        // the image we persist (and to what recovery will rewrite).
        for &f in &dirty {
            let buf = self.frames[f].buf.clone();
            finalize_checksum(&mut buf.borrow_mut());
        }

        // Journal the records durably before any page reaches the block store. Collect
        // the after-images first so we don't hold a `frames` borrow across the
        // journal borrow.
        let images: Vec<(u64, Vec<u8>)> = if self.journal.is_some() {
            dirty
                .iter()
                .map(|&f| {
                    let id = self.frames[f].page_id.unwrap().0;
                    (id, self.frames[f].buf.borrow().to_vec())
                })
                .collect()
        } else {
            Vec::new()
        };
        if let Some(journal) = self.journal.as_mut() {
            for (page_id, image) in images {
                journal.append(&JournalOp::Write { page_id, image })?;
            }
            journal.append(&JournalOp::Commit)?;
        }

        // Now safe to write the pages through and make them durable.
        for &f in &dirty {
            let id = self.frames[f].page_id.unwrap();
            let buf = self.frames[f].buf.clone();
            self.block.write(id, &buf.borrow())?;
            self.frames[f].dirty = false;
        }
        self.block.sync()?;

        // The pages are on disk; their log records are no longer needed.
        if let Some(journal) = self.journal.as_mut() {
            journal.truncate()?;
        }
        Ok(())
    }
}

/// The page cache. Cheap to clone the handle to (it shares one pool); v1 is
/// single-threaded, so don't send it across threads.
pub struct PageCache<V: BlockStore> {
    inner: Rc<RefCell<Inner<V>>>,
}

impl<V: BlockStore> PageCache<V> {
    /// A cache over `block` with a pool of `frames` slots and the default LRU-K
    /// (K=2) policy.
    pub fn new(block: V, frames: usize) -> Self {
        Self::with_policy(block, frames, Box::new(LruK::<FrameId>::new(2)))
    }

    /// A cache with an explicit eviction policy — for benchmarking alternatives.
    /// Not journaled (see [`with_journal`](Self::with_journal)).
    pub fn with_policy(block: V, frames: usize, policy: Box<dyn EvictionPolicy<FrameId>>) -> Self {
        Self::build(block, frames, policy, None)
    }

    /// A journaled cache: recover from `journal_path` (replaying the last
    /// committed flush into `block`), then run with write-ahead logging so a crash
    /// loses nothing committed by [`flush`](Self::flush). Uses the default LRU-K.
    pub fn with_journal(
        mut block: V,
        frames: usize,
        journal_path: impl AsRef<Path>,
    ) -> Result<Self> {
        let mut journal = BlockJournal::open(journal_path)?;
        Self::recover(&mut block, &journal)?;
        // Recovery is now durably applied to the block store; the journal starts empty.
        journal.truncate()?;
        Ok(Self::build(
            block,
            frames,
            Box::new(LruK::<FrameId>::new(2)),
            Some(journal),
        ))
    }

    fn build(
        block: V,
        frames: usize,
        policy: Box<dyn EvictionPolicy<FrameId>>,
        journal: Option<BlockJournal>,
    ) -> Self {
        assert!(frames >= 1, "page cache needs at least one frame");
        let inner = Inner {
            block,
            frames: (0..frames).map(|_| Frame::new()).collect(),
            table: HashMap::new(),
            free: (0..frames).rev().collect(),
            policy,
            journal,
        };
        Self {
            inner: Rc::new(RefCell::new(inner)),
        }
    }

    /// Replay the journal into `block`: apply the after-images of the last fully
    /// committed flush (everything up to the final `Commit`), then sync. Records
    /// after the last `Commit` are a torn flush and are dropped.
    fn recover(block: &mut V, journal: &BlockJournal) -> Result<()> {
        let records = journal.replay()?;
        let Some(committed) = records
            .iter()
            .rposition(|op| matches!(op, JournalOp::Commit))
        else {
            return Ok(()); // nothing committed
        };
        for op in &records[..committed] {
            if let JournalOp::Write { page_id, image } = op {
                let id = BlockId(*page_id);
                block.ensure_allocated(id)?;
                let mut b = Block::zeroed();
                b[..image.len()].copy_from_slice(image);
                block.write(id, &b)?;
            }
        }
        block.sync()
    }

    /// Fetch `id` for shared reading. Fails with [`Error::Busy`] if a writer
    /// holds it.
    pub fn read(&self, id: BlockId) -> Result<ReadPage<V>> {
        let (frame, buf) = self.inner.borrow_mut().acquire_read(id)?;
        Ok(ReadPage {
            pool: self.inner.clone(),
            frame,
            page_id: id,
            buf,
        })
    }

    /// Fetch `id` for exclusive writing. Fails with [`Error::Busy`] if any
    /// other handle holds it.
    pub fn write(&self, id: BlockId) -> Result<WritePage<V>> {
        let (frame, buf) = self.inner.borrow_mut().acquire_write(id)?;
        Ok(WritePage {
            pool: self.inner.clone(),
            frame,
            page_id: id,
            buf,
        })
    }

    /// Allocate a brand-new page, returning its id and an exclusive, zeroed,
    /// dirty handle. The caller stamps a valid header before the page is read
    /// back (the page-type initializers do this).
    pub fn allocate(&self) -> Result<(BlockId, WritePage<V>)> {
        let (id, frame, buf) = self.inner.borrow_mut().allocate()?;
        Ok((
            id,
            WritePage {
                pool: self.inner.clone(),
                frame,
                page_id: id,
                buf,
            },
        ))
    }

    /// Write every dirty frame back and `sync` the block store — the durability point.
    pub fn flush(&self) -> Result<()> {
        self.inner.borrow_mut().flush()
    }

    /// Hint that `id` will be needed soon. A no-op in v1 (placeholder for the
    /// scan-buffer / prefetch path).
    pub fn prefetch(&self, _id: BlockId) {}

    /// Number of frames in the pool.
    pub fn frame_count(&self) -> usize {
        self.inner.borrow().frames.len()
    }

    /// Number of pages currently resident.
    pub fn resident(&self) -> usize {
        self.inner.borrow().table.len()
    }
}

/// A pinned, shared-read handle to a page. Unpins on drop. Multiple may coexist
/// on one page.
pub struct ReadPage<V: BlockStore> {
    pool: Rc<RefCell<Inner<V>>>,
    frame: FrameId,
    page_id: BlockId,
    buf: FrameBuf,
}

impl<V: BlockStore> ReadPage<V> {
    /// The page's id.
    pub fn page_id(&self) -> BlockId {
        self.page_id
    }

    /// The parsed page header.
    pub fn header(&self) -> Result<PageHeader> {
        let b = self.buf.borrow();
        PageHeader::parse(&b)
    }

    /// The whole page, header included.
    pub fn bytes(&self) -> Ref<'_, [u8]> {
        Ref::map(self.buf.borrow(), |b| &b[..])
    }

    /// The payload — bytes after the header.
    pub fn payload(&self) -> Ref<'_, [u8]> {
        Ref::map(self.buf.borrow(), |b| &b[HEADER_LEN..])
    }
}

impl<V: BlockStore> Drop for ReadPage<V> {
    fn drop(&mut self) {
        self.pool.borrow_mut().release_read(self.frame);
    }
}

/// A pinned, exclusive-write handle to a page. Marks the frame dirty and unpins
/// on drop.
pub struct WritePage<V: BlockStore> {
    pool: Rc<RefCell<Inner<V>>>,
    frame: FrameId,
    page_id: BlockId,
    buf: FrameBuf,
}

impl<V: BlockStore> WritePage<V> {
    /// The page's id.
    pub fn page_id(&self) -> BlockId {
        self.page_id
    }

    /// The parsed page header.
    pub fn header(&self) -> Result<PageHeader> {
        let b = self.buf.borrow();
        PageHeader::parse(&b)
    }

    /// Stamp `header`'s fields into the page (leaving the checksum for writeback).
    pub fn write_header(&self, header: &PageHeader) {
        let mut b = self.buf.borrow_mut();
        header.write(&mut b);
    }

    /// The whole page, header included.
    pub fn bytes(&self) -> Ref<'_, [u8]> {
        Ref::map(self.buf.borrow(), |b| &b[..])
    }

    /// The whole page, mutably.
    pub fn bytes_mut(&self) -> RefMut<'_, [u8]> {
        RefMut::map(self.buf.borrow_mut(), |b| &mut b[..])
    }

    /// The payload — bytes after the header.
    pub fn payload(&self) -> Ref<'_, [u8]> {
        Ref::map(self.buf.borrow(), |b| &b[HEADER_LEN..])
    }

    /// The payload, mutably.
    pub fn payload_mut(&self) -> RefMut<'_, [u8]> {
        RefMut::map(self.buf.borrow_mut(), |b| &mut b[HEADER_LEN..])
    }
}

impl<V: BlockStore> Drop for WritePage<V> {
    fn drop(&mut self) {
        self.pool.borrow_mut().release_write(self.frame);
    }
}
