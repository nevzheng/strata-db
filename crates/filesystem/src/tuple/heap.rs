//! The tuple heap — stores tuples in [`TuplePage`]s over the [`PageCache`] and
//! addresses them by [`TupleLoc`].
//!
//! This is the layer an index (the LSM) sits on top of: [`insert`](Heap::insert)
//! parks a tuple's bytes in a page and returns its `TupleLoc`; the index records
//! `row key → TupleLoc`; [`get`](Heap::get) resolves a `TupleLoc` back to the
//! tuple's bytes.
//!
//! Reads and writes return **views that own the page pin**, so the tuple bytes
//! they expose are borrows straight into the cached frame — no copy. The pin is
//! released when the view drops. (See [`TupleRef`] / [`TupleMut`].)
//!
//! Free-space management is deliberately minimal for v1: tuples are appended to
//! a single "current" page until one doesn't fit, then a fresh page is
//! allocated. Freed slots are not reused yet — a free-space map is future work.

use std::cell::{Cell, Ref, RefMut};
use std::path::Path;

use super::page::{TuplePage, TuplePageMut};
use crate::error::Error;
use crate::{
    BlockId, BlockStore, FileBlockStore, PageCache, ReadPage, Result, TupleLoc, WritePage,
};

/// A heap of tuples over a page cache. The block-store backend defaults to
/// [`FileBlockStore`]; tests use [`MemBlockStore`](crate::MemBlockStore).
pub struct Heap<V: BlockStore = FileBlockStore> {
    cache: PageCache<V>,
    /// The page currently being filled. `None` until the first insert.
    current: Cell<Option<BlockId>>,
}

impl Heap<FileBlockStore> {
    /// Open a file-backed heap rooted at `dir`: tuple pages live in
    /// `dir/tuples.db`, made durable by the journal at `dir/tuples.journal`,
    /// fronted by a `frames`-frame buffer pool. Creates `dir` if absent.
    ///
    /// The block store and page cache are built and owned internally — callers
    /// work with tuples and never touch the storage plumbing. Tuning their
    /// behavior (cache policy, separate paths) can be exposed later.
    pub fn open(dir: &Path, frames: usize) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let cache = PageCache::with_journal(
            FileBlockStore::open(dir.join("tuples.db"))?,
            frames,
            dir.join("tuples.journal"),
        )?;
        Ok(Self::new(cache))
    }
}

impl<V: BlockStore> Heap<V> {
    /// Build a heap over an existing `cache` (e.g. an in-memory one for tests).
    pub fn new(cache: PageCache<V>) -> Self {
        Self {
            cache,
            current: Cell::new(None),
        }
    }

    /// The underlying page cache — e.g. for `TEXT` overflow pages, which the
    /// heap stores by pointer but does not manage.
    pub fn cache(&self) -> &PageCache<V> {
        &self.cache
    }

    /// Store `tuple`'s bytes and return where they landed. Errors only if the
    /// tuple is too large for any page (see [`Error::TupleTooLarge`]).
    pub fn insert(&self, tuple: &[u8]) -> Result<TupleLoc> {
        // Fast path: append to the page we're already filling.
        if let Some(page_id) = self.current.get()
            && let Some(slot) = self.try_append(page_id, tuple)?
        {
            return Ok(TupleLoc::new(page_id, slot));
        }
        // Either no current page or it's full: start a fresh one.
        let (page_id, slot) = self.append_to_new_page(tuple)?;
        self.current.set(Some(page_id));
        Ok(TupleLoc::new(page_id, slot))
    }

    /// A read view of the tuple at `loc`, holding its page pinned.
    pub fn get(&self, loc: TupleLoc) -> Result<TupleRef<V>> {
        let page = self.cache.read(loc.page_id)?;
        Ok(TupleRef {
            page,
            slot: loc.slot_id,
        })
    }

    /// A write view of the tuple at `loc`, holding its page pinned exclusively.
    /// For in-place, same-length edits.
    pub fn get_mut(&self, loc: TupleLoc) -> Result<TupleMut<V>> {
        let page = self.cache.write(loc.page_id)?;
        Ok(TupleMut {
            page,
            slot: loc.slot_id,
        })
    }

    /// Pin a whole page for reading many of its tuples — the scan primitive.
    ///
    /// The returned [`PageTuples`] holds **one** pin; borrow a [`TupleView`] per
    /// tuple from it. A scan that stays on a page thus costs one handle, not one
    /// per row.
    pub fn page(&self, page_id: BlockId) -> Result<PageTuples<V>> {
        Ok(PageTuples {
            page: self.cache.read(page_id)?,
        })
    }

    /// Mark the tuple at `loc` deleted. Returns `false` if the slot is out of
    /// range. The bytes are not reclaimed here — that is compaction's job.
    pub fn delete(&self, loc: TupleLoc) -> Result<bool> {
        let page = self.cache.write(loc.page_id)?;
        let mut buf = page.bytes_mut();
        let mut tuple_page = TuplePageMut::open(&mut buf)?;
        Ok(tuple_page.delete(loc.slot_id))
    }

    /// Commit all dirty pages durably (delegates to the cache).
    pub fn flush(&self) -> Result<()> {
        self.cache.flush()
    }

    /// Try to append to an existing page; `None` if it's full.
    fn try_append(&self, page_id: BlockId, tuple: &[u8]) -> Result<Option<u16>> {
        let page = self.cache.write(page_id)?;
        let mut buf = page.bytes_mut();
        let mut tuple_page = TuplePageMut::open(&mut buf)?;
        Ok(tuple_page.insert(tuple))
    }

    /// Allocate a fresh page and append to it. Errors if `tuple` can't fit an
    /// empty page.
    fn append_to_new_page(&self, tuple: &[u8]) -> Result<(BlockId, u16)> {
        let (page_id, page) = self.cache.allocate()?;
        let mut buf = page.bytes_mut();
        let mut tuple_page = TuplePageMut::init(&mut buf);
        let max = tuple_page.free_space();
        let slot = tuple_page.insert(tuple).ok_or(Error::TupleTooLarge {
            len: tuple.len(),
            max,
        })?;
        Ok((page_id, slot))
    }
}

/// A read view of one tuple, owning the pin on its page. The bytes it exposes
/// borrow straight into the cached frame.
pub struct TupleRef<V: BlockStore = FileBlockStore> {
    page: ReadPage<V>,
    slot: u16,
}

impl<V: BlockStore> TupleRef<V> {
    /// This tuple's location.
    pub fn loc(&self) -> TupleLoc {
        TupleLoc::new(self.page.page_id(), self.slot)
    }

    /// The tuple's bytes, borrowed from the pinned frame. `None` if the slot is
    /// deleted or the page isn't a tuple page (a corrupt pointer).
    pub fn bytes(&self) -> Option<Ref<'_, [u8]>> {
        Ref::filter_map(self.page.bytes(), |page| {
            TuplePage::open(page).ok()?.get(self.slot)
        })
        .ok()
    }
}

/// A write view of one tuple, owning the exclusive pin on its page. Allows
/// in-place, same-length mutation of the tuple's bytes.
pub struct TupleMut<V: BlockStore = FileBlockStore> {
    page: WritePage<V>,
    slot: u16,
}

impl<V: BlockStore> TupleMut<V> {
    /// This tuple's location.
    pub fn loc(&self) -> TupleLoc {
        TupleLoc::new(self.page.page_id(), self.slot)
    }

    /// The tuple's bytes, borrowed read-only from the pinned frame.
    pub fn bytes(&self) -> Option<Ref<'_, [u8]>> {
        Ref::filter_map(self.page.bytes(), |page| {
            TuplePage::open(page).ok()?.get(self.slot)
        })
        .ok()
    }

    /// The tuple's bytes, borrowed mutably from the pinned frame, for an
    /// in-place same-length edit. `None` if the slot is deleted or the page
    /// isn't a tuple page.
    pub fn bytes_mut(&self) -> Option<RefMut<'_, [u8]>> {
        RefMut::filter_map(self.page.bytes_mut(), |page| {
            TuplePageMut::open(page)
                .ok()?
                .into_slot_bytes_mut(self.slot)
        })
        .ok()
    }
}

/// A pinned tuple page, held to read many of its tuples. Owns the single
/// [`ReadPage`] (one pin); every [`TupleView`] borrowed from it shares that pin.
pub struct PageTuples<V: BlockStore = FileBlockStore> {
    page: ReadPage<V>,
}

impl<V: BlockStore> PageTuples<V> {
    /// The page's id.
    pub fn page_id(&self) -> BlockId {
        self.page.page_id()
    }

    /// Iterate this page's tuples as borrowed views, sharing the one pin — the
    /// block read path. Yields a view per slot in order; a view's `bytes()` is
    /// `None` for a deleted slot (a workspace never deletes, so its views are
    /// always live).
    pub fn iter(&self) -> impl Iterator<Item = TupleView<'_, V>> {
        let count = TuplePage::open(&self.page.bytes())
            .map(|p| p.slot_count())
            .unwrap_or(0);
        (0..count).map(move |slot| self.tuple(slot))
    }

    /// A borrowed view of the tuple at `slot`. Cheap — no new pin, no I/O.
    pub fn tuple(&self, slot: u16) -> TupleView<'_, V> {
        TupleView {
            page: &self.page,
            slot,
        }
    }
}

/// A borrowed view of one tuple, sharing its page's single pin via the
/// [`PageTuples`] it came from. Holds no pin of its own.
pub struct TupleView<'p, V: BlockStore = FileBlockStore> {
    page: &'p ReadPage<V>,
    slot: u16,
}

impl<V: BlockStore> TupleView<'_, V> {
    /// The slot this view addresses.
    pub fn slot(&self) -> u16 {
        self.slot
    }

    /// The tuple's bytes, borrowed from the shared pinned frame. `None` if the
    /// slot is deleted or the page isn't a tuple page (a corrupt pointer).
    pub fn bytes(&self) -> Option<Ref<'_, [u8]>> {
        Ref::filter_map(self.page.bytes(), |page| {
            TuplePage::open(page).ok()?.get(self.slot)
        })
        .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemBlockStore;

    fn heap() -> Heap<MemBlockStore> {
        Heap::new(PageCache::new(MemBlockStore::new(), 8))
    }

    #[test]
    fn insert_then_read_back() {
        let heap = heap();
        let a = heap.insert(b"alice").unwrap();
        let b = heap.insert(b"bob").unwrap();

        assert_eq!(&*heap.get(a).unwrap().bytes().unwrap(), b"alice");
        assert_eq!(&*heap.get(b).unwrap().bytes().unwrap(), b"bob");
    }

    #[test]
    fn small_tuples_share_a_page() {
        let heap = heap();
        let a = heap.insert(b"x").unwrap();
        let b = heap.insert(b"y").unwrap();
        // Same page, consecutive stable slots.
        assert_eq!(a.page_id, b.page_id);
        assert_eq!((a.slot_id, b.slot_id), (0, 1));
    }

    #[test]
    fn in_place_update_through_write_view() {
        let heap = heap();
        let loc = heap.insert(b"AAAA").unwrap();
        {
            let view = heap.get_mut(loc).unwrap();
            view.bytes_mut().unwrap().copy_from_slice(b"ZZZZ");
        }
        assert_eq!(&*heap.get(loc).unwrap().bytes().unwrap(), b"ZZZZ");
    }

    #[test]
    fn delete_hides_the_tuple() {
        let heap = heap();
        let loc = heap.insert(b"gone").unwrap();
        assert!(heap.delete(loc).unwrap());
        assert!(heap.get(loc).unwrap().bytes().is_none());
    }

    #[test]
    fn survives_eviction_and_flush() {
        // Tiny pool so inserts spill across pages and get evicted/written back.
        let heap = Heap::new(PageCache::new(MemBlockStore::new(), 2));
        let mut locs = Vec::new();
        for i in 0..500u32 {
            locs.push(heap.insert(&i.to_be_bytes()).unwrap());
        }
        heap.flush().unwrap();
        for (i, loc) in locs.into_iter().enumerate() {
            let view = heap.get(loc).unwrap();
            assert_eq!(&*view.bytes().unwrap(), &(i as u32).to_be_bytes()[..]);
        }
    }

    #[test]
    fn one_pinned_page_serves_many_views() {
        let heap = heap();
        let a = heap.insert(b"a").unwrap();
        let b = heap.insert(b"bb").unwrap();
        let c = heap.insert(b"ccc").unwrap();
        assert_eq!(
            (a.page_id, b.page_id, c.page_id),
            (a.page_id, a.page_id, a.page_id)
        );

        // One pinned page, three borrowed views into it — no per-tuple handle.
        let page = heap.page(a.page_id).unwrap();
        assert_eq!(&*page.tuple(a.slot_id).bytes().unwrap(), b"a");
        assert_eq!(&*page.tuple(b.slot_id).bytes().unwrap(), b"bb");
        assert_eq!(&*page.tuple(c.slot_id).bytes().unwrap(), b"ccc");
        assert_eq!(heap.cache().resident(), 1, "only one page resident/pinned");
    }

    #[test]
    fn oversized_tuple_is_rejected() {
        let heap = heap();
        let huge = vec![0u8; crate::PAGE_SIZE];
        assert!(matches!(
            heap.insert(&huge),
            Err(Error::TupleTooLarge { .. })
        ));
    }
}
