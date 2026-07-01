//! Workspaces — bounded, ephemeral scratch tuple storage for the engine.
//!
//! A workspace is a write-once, replay-many buffer of opaque tuple bytes (join
//! build sides, sort runs, materialized inners). Two rules shape the design:
//!
//! - **Always bounded.** Every workspace has a byte `capacity`; an append past
//!   it fails with [`Error::WorkspaceFull`] rather than growing without limit.
//!   [`used`](Workspace::used) reports current utilization.
//! - **Never journaled.** The data is scratch — durability is pure overhead, so
//!   the file backing uses [`PageCache::new`] (journal `None`), never
//!   [`Heap::open`].
//!
//! Two backings behind the [`Workspace`] trait; the operator picks the one it
//! wants (no builder):
//!
//! - [`MemoryWorkspace`] — pages are [`Slab`]s from a [`MemoryPool`] sized to the
//!   capacity. Zero-copy, no I/O. Cleanup is just drop (slabs return to the pool).
//! - [`FileWorkspace`] — pages live in a journal-less [`PageCache`] over a temp
//!   file; hot pages stay in frames, cold pages spill and page back. Cleanup
//!   deletes the temp dir on drop (and exposes its [`path`](FileWorkspace::path)
//!   so a future GC can sweep leftovers).
//!
//! Reads are block-at-a-time: [`blocks`](Workspace::blocks) streams blocks, each
//! block [`tuples`](WorkspaceBlock::tuples) streams its tuples as borrowed views.
//! A view's [`read`](TupleBytes::read) hands back the bytes in place — the engine
//! decides whether to decode, copy, or write them onward.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::tuple::{TuplePage, TuplePageMut};
use crate::{
    BlockId, BlockStore, DiskBlockStore, Error, Heap, MemoryPool, PAGE_SIZE, PageCache, PageTuples,
    Result, Slab, TupleView,
};

/// Where a tuple lives in a workspace: an opaque page handle plus a slot. Stable
/// for the workspace's lifetime, so the engine can stash it and resolve it later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceLoc {
    pub page: u64,
    pub slot: u16,
}

/// Bounded, append-then-replay scratch storage. See the module docs.
pub trait Workspace {
    /// Append a copy of `bytes`, returning where it landed. Errors with
    /// [`Error::WorkspaceFull`] when the capacity is reached.
    fn append(&mut self, bytes: &[u8]) -> Result<WorkspaceLoc>;

    /// The byte ceiling — the hard bound on this workspace's footprint.
    fn capacity(&self) -> usize;

    /// Bytes currently committed to backing storage.
    fn used(&self) -> usize;

    /// Bytes still available before [`Error::WorkspaceFull`].
    fn remaining(&self) -> usize {
        self.capacity().saturating_sub(self.used())
    }

    /// One block of the workspace, holding its own page pin / borrow.
    type Block<'w>: WorkspaceBlock
    where
        Self: 'w;

    /// Stream the blocks in insertion order. Each yielded block is independent
    /// (owns its pin or borrows the workspace), so a block-nested-loop join can
    /// hold a batch of them while it rescans the other input. Zero-copy: a
    /// block's tuples borrow it in place.
    fn blocks(&self) -> impl Iterator<Item = Self::Block<'_>> + '_;

    /// Convenience: every tuple in insertion order, doing the block navigation
    /// for you. Yields **owned** bytes — a flat borrowed stream can't outlive its
    /// per-block pin, so this copies. Reach for [`blocks`](Self::blocks) when you
    /// want zero copy.
    fn tuples(&self) -> impl Iterator<Item = Vec<u8>> + '_ {
        self.blocks().flat_map(|block| {
            // Copy each tuple out before the block (and its pin) drops.
            block
                .tuples()
                .filter_map(|t| t.read().map(|b| b.to_vec()))
                .collect::<Vec<_>>()
                .into_iter()
        })
    }
}

/// One block (page) of a workspace — a handle from which to stream tuples.
pub trait WorkspaceBlock {
    type Tuple<'b>: TupleBytes
    where
        Self: 'b;

    /// Stream this block's tuples as borrowed views, in slot order.
    fn tuples(&self) -> impl Iterator<Item = Self::Tuple<'_>> + '_;
}

/// A tuple's bytes, accessible in place. The caller makes the explicit choice to
/// decode, copy (`to_vec`), or write the bytes to a destination.
pub trait TupleBytes {
    /// The bytes, borrowed; `None` for a (never-in-a-workspace) deleted slot.
    fn read(&self) -> Option<impl Deref<Target = [u8]> + '_>;
}

// --- memory backing: pool-backed slabs, zero-copy ----------------------------

/// In-memory workspace: pages are slabs from a pool sized to the capacity.
pub struct MemoryWorkspace {
    pool: MemoryPool,
    /// One `PAGE_SIZE` slab per page, in fill order. The last is "current".
    blocks: Vec<Slab>,
}

impl MemoryWorkspace {
    /// A workspace bounded to `capacity` bytes (must be > 0 — workspaces are
    /// never unbounded). Backed by its own pool sized to that cap.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "a workspace must be bounded (capacity > 0)");
        Self {
            pool: MemoryPool::new(capacity),
            blocks: Vec::new(),
        }
    }
}

impl Workspace for MemoryWorkspace {
    fn append(&mut self, tuple: &[u8]) -> Result<WorkspaceLoc> {
        // Fast path: append to the page we're already filling.
        if let Some(i) = self.blocks.len().checked_sub(1) {
            let mut page = TuplePageMut::open(self.blocks[i].as_mut_slice())?;
            if let Some(slot) = page.insert(tuple) {
                return Ok(WorkspaceLoc {
                    page: i as u64,
                    slot,
                });
            }
        }
        // Full or none: take a fresh slab. The pool enforces the bound and rolls
        // its reservation back on failure, so this never overshoots `capacity`.
        let mut slab = self
            .pool
            .allocate(PAGE_SIZE)
            .map_err(|e| Error::WorkspaceFull {
                requested: e.requested,
                used: e.in_use,
                capacity: e.cap,
            })?;
        let mut page = TuplePageMut::init(slab.as_mut_slice());
        let max = page.free_space();
        let slot = page.insert(tuple).ok_or(Error::TupleTooLarge {
            len: tuple.len(),
            max,
        })?;
        self.blocks.push(slab);
        Ok(WorkspaceLoc {
            page: (self.blocks.len() - 1) as u64,
            slot,
        })
    }

    fn capacity(&self) -> usize {
        self.pool.cap()
    }

    fn used(&self) -> usize {
        self.pool.in_use()
    }

    type Block<'w> = MemBlock<'w>;

    fn blocks(&self) -> impl Iterator<Item = MemBlock<'_>> + '_ {
        self.blocks.iter().filter_map(|slab| {
            TuplePage::open(slab.as_slice())
                .ok()
                .map(|page| MemBlock { page })
        })
    }
}

/// A memory block: a borrowed view over one slab's page.
pub struct MemBlock<'w> {
    page: TuplePage<'w>,
}

impl WorkspaceBlock for MemBlock<'_> {
    type Tuple<'b>
        = MemTuple<'b>
    where
        Self: 'b;

    fn tuples(&self) -> impl Iterator<Item = MemTuple<'_>> + '_ {
        let count = self.page.slot_count();
        (0..count).filter_map(move |slot| self.page.get(slot).map(MemTuple))
    }
}

/// A tuple in a [`MemoryWorkspace`] — a direct slice into the slab (zero copy).
pub struct MemTuple<'b>(&'b [u8]);

impl TupleBytes for MemTuple<'_> {
    fn read(&self) -> Option<impl Deref<Target = [u8]> + '_> {
        Some(self.0)
    }
}

// --- file backing: journal-less spilling page cache over a temp file ----------

/// File-backed workspace: a journal-less page cache over a temp file. Spills hot
/// pages to disk under cache pressure; bounded by `capacity` bytes on disk.
pub struct FileWorkspace {
    heap: Heap<DiskBlockStore>,
    /// Page ids in insertion order, for sequential replay.
    pages: Vec<BlockId>,
    /// Hard cap on pages = `disk_budget / PAGE_SIZE`.
    max_pages: usize,
    /// Disk ceiling in bytes (the `capacity()` the trait reports).
    disk_budget: usize,
    /// In-RAM working set in bytes (whole pages).
    memory_budget: usize,
    /// Temp dir holding the data file; removed on drop, path kept for GC.
    dir: PathBuf,
}

impl FileWorkspace {
    /// A spilling workspace sized by two independent byte budgets:
    ///
    /// - `memory_budget` — the in-RAM working set. As pages fill they flush to
    ///   disk (blocking, single-threaded v1) and their RAM is reused, so live
    ///   memory stays at the working set no matter how much spills. Rounds down
    ///   to whole pages, floored at one. Size it to the most blocks a consumer
    ///   pins at once — e.g. a block-nested-loop batch; one page is the minimum.
    /// - `disk_budget` — the on-disk ceiling. Pages spill freely up to it, then
    ///   [`append`](Workspace::append) fails with [`Error::WorkspaceFull`]. Must
    ///   hold ≥ 1 page.
    pub fn new(memory_budget: usize, disk_budget: usize) -> Result<Self> {
        let frames = (memory_budget / PAGE_SIZE).max(1);
        let max_pages = disk_budget / PAGE_SIZE;
        assert!(max_pages >= 1, "disk_budget must hold at least one page");
        let dir = temp_dir()?;
        let store = DiskBlockStore::open(dir.join("workspace.db"))?;
        // `PageCache::new` (not `with_journal`) → no journal on any path. The
        // cache's frames *are* the working set; eviction is the flush-on-full.
        let heap = Heap::new(PageCache::new(store, frames));
        Ok(Self {
            heap,
            pages: Vec::new(),
            max_pages,
            disk_budget,
            memory_budget: frames * PAGE_SIZE,
            dir,
        })
    }

    /// The in-RAM working-set size in bytes (rounded to whole pages).
    pub fn memory_budget(&self) -> usize {
        self.memory_budget
    }

    /// The temp directory backing this workspace — kept so a GC can sweep it if
    /// drop-time cleanup ever fails.
    pub fn path(&self) -> &Path {
        &self.dir
    }
}

impl Workspace for FileWorkspace {
    fn append(&mut self, tuple: &[u8]) -> Result<WorkspaceLoc> {
        let loc = self.heap.insert(tuple)?;
        if self.pages.last() != Some(&loc.page_id) {
            // A new page was allocated — enforce the disk bound. (The stray page
            // the heap just made is unreferenced scratch, reclaimed on drop.)
            if self.pages.len() >= self.max_pages {
                return Err(Error::WorkspaceFull {
                    requested: PAGE_SIZE,
                    used: self.pages.len() * PAGE_SIZE,
                    capacity: self.disk_budget,
                });
            }
            self.pages.push(loc.page_id);
        }
        Ok(WorkspaceLoc {
            page: loc.page_id.0,
            slot: loc.slot_id,
        })
    }

    fn capacity(&self) -> usize {
        self.disk_budget
    }

    fn used(&self) -> usize {
        self.pages.len() * PAGE_SIZE
    }

    type Block<'w> = FileBlock;

    fn blocks(&self) -> impl Iterator<Item = FileBlock> + '_ {
        self.pages
            .iter()
            .filter_map(move |&pid| self.heap.page(pid).ok().map(|page| FileBlock { page }))
    }
}

impl FileWorkspace {
    /// Consume the workspace into an owned, block-buffered cursor over every
    /// tuple in insertion order. Unlike [`tuples`](Workspace::tuples) — which
    /// borrows `&self`, so the iterator can't outlive the borrow — this owns the
    /// workspace, so the cursor can be stored and driven independently. That's
    /// what lets a streaming join hold a probe cursor as operator state and pull
    /// one tuple at a time (no materializing the side into memory). One page is
    /// buffered at a time, same footprint as `tuples`.
    pub fn into_tuples(self) -> FileWorkspaceTuples {
        FileWorkspaceTuples {
            ws: self,
            next_page: 0,
            page: Vec::new().into_iter(),
        }
    }
}

/// Owned cursor over a [`FileWorkspace`]'s tuples (see
/// [`into_tuples`](FileWorkspace::into_tuples)). Buffers one page at a time.
pub struct FileWorkspaceTuples {
    ws: FileWorkspace,
    next_page: usize,
    page: std::vec::IntoIter<Vec<u8>>,
}

impl Iterator for FileWorkspaceTuples {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(tuple) = self.page.next() {
                return Some(tuple);
            }
            // Current page drained — load the next, copying its tuples out
            // before the page pin drops (same as the borrowing `tuples`).
            let &pid = self.ws.pages.get(self.next_page)?;
            self.next_page += 1;
            if let Ok(page) = self.ws.heap.page(pid) {
                let buf: Vec<Vec<u8>> = page
                    .iter()
                    .filter_map(|t| t.bytes().map(|b| b.to_vec()))
                    .collect();
                self.page = buf.into_iter();
            }
        }
    }
}

impl Drop for FileWorkspace {
    fn drop(&mut self) {
        // Best-effort cleanup of the scratch file; nothing to recover.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A file block: owns a pinned page for the duration of its tuple stream.
pub struct FileBlock {
    page: PageTuples<DiskBlockStore>,
}

impl WorkspaceBlock for FileBlock {
    type Tuple<'b>
        = TupleView<'b, DiskBlockStore>
    where
        Self: 'b;

    fn tuples(&self) -> impl Iterator<Item = TupleView<'_, DiskBlockStore>> + '_ {
        self.page.iter()
    }
}

impl<V: BlockStore> TupleBytes for TupleView<'_, V> {
    fn read(&self) -> Option<impl Deref<Target = [u8]> + '_> {
        self.bytes()
    }
}

/// A fresh, unique temp directory for one workspace's data file. Uniqueness from
/// pid + a process-wide counter — no RNG, no `tempfile` dep.
fn temp_dir() -> Result<PathBuf> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("strata-workspace-{}-{}", std::process::id(), seq));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replay a workspace to owned bytes via the block → tuple streams.
    fn drain<W: Workspace>(ws: &W) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for block in ws.blocks() {
            for tuple in block.tuples() {
                if let Some(bytes) = tuple.read() {
                    out.push(bytes.to_vec());
                }
            }
        }
        out
    }

    #[test]
    fn memory_appends_and_replays_in_order() {
        let mut ws = MemoryWorkspace::new(1 << 20);
        let inputs: Vec<Vec<u8>> = (0..200u32).map(|i| i.to_be_bytes().to_vec()).collect();
        for t in &inputs {
            ws.append(t).unwrap();
        }
        assert_eq!(drain(&ws), inputs);
        assert_eq!(ws.capacity(), 1 << 20);
        assert!(ws.used() > 0 && ws.used() <= ws.capacity());
    }

    #[test]
    fn memory_is_bounded_with_a_useful_oom() {
        // Room for exactly one page.
        let mut ws = MemoryWorkspace::new(PAGE_SIZE);
        let tuple = vec![0u8; 64];
        let err = loop {
            if let Err(e) = ws.append(&tuple) {
                break e;
            }
        };
        match err {
            Error::WorkspaceFull {
                requested,
                used,
                capacity,
            } => {
                assert_eq!(requested, PAGE_SIZE);
                assert_eq!(capacity, PAGE_SIZE);
                assert_eq!(used, PAGE_SIZE);
            }
            other => panic!("expected WorkspaceFull, got {other:?}"),
        }
        assert!(err_is_exhausted(&ws));
    }

    fn err_is_exhausted(_ws: &MemoryWorkspace) -> bool {
        // WorkspaceFull counts as resource exhaustion.
        Error::WorkspaceFull {
            requested: 1,
            used: 1,
            capacity: 1,
        }
        .is_exhausted()
    }

    #[test]
    fn file_spills_and_replays_in_order() {
        // 2-page working set forces eviction; disk ceiling well above the data.
        let mut ws = FileWorkspace::new(2 * PAGE_SIZE, 1 << 20).unwrap();
        let inputs: Vec<Vec<u8>> = (0..500u32).map(|i| i.to_be_bytes().to_vec()).collect();
        for t in &inputs {
            ws.append(t).unwrap();
        }
        assert_eq!(
            drain(&ws),
            inputs,
            "spilled data must replay intact, in order"
        );
    }

    #[test]
    fn file_single_page_working_set_flushes_and_reuses_ram() {
        // The minimal working set: one page in RAM. Each full page flushes to
        // disk and the single frame is reused for the next — yet replay is whole.
        let mut ws = FileWorkspace::new(PAGE_SIZE, 1 << 20).unwrap();
        assert_eq!(ws.memory_budget(), PAGE_SIZE);
        let inputs: Vec<Vec<u8>> = (0..500u32).map(|i| i.to_be_bytes().to_vec()).collect();
        for t in &inputs {
            ws.append(t).unwrap();
        }
        assert_eq!(drain(&ws), inputs);
    }

    #[test]
    fn file_is_bounded_with_a_useful_oom() {
        // Disk ceiling of two pages.
        let mut ws = FileWorkspace::new(PAGE_SIZE, 2 * PAGE_SIZE).unwrap();
        let tuple = vec![0u8; 64];
        let err = loop {
            if let Err(e) = ws.append(&tuple) {
                break e;
            }
        };
        assert!(matches!(err, Error::WorkspaceFull { capacity, .. } if capacity == 2 * PAGE_SIZE));
    }

    #[test]
    fn file_writes_no_journal_and_cleans_up_on_drop() {
        let dir;
        {
            let mut ws = FileWorkspace::new(2 * PAGE_SIZE, 1 << 20).unwrap();
            for i in 0..100u32 {
                ws.append(&i.to_be_bytes()).unwrap();
            }
            dir = ws.path().to_path_buf();
            let entries: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert!(
                entries.iter().all(|n| !n.contains("journal")),
                "workspace must not journal, found: {entries:?}"
            );
        }
        assert!(!dir.exists(), "temp dir should be removed on drop");
    }
}
