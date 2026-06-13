//! The engine's scan: a **lending** iterator that holds one pinned heap page at
//! a time and hands out borrowed tuple views into it.
//!
//! Resolving an index scan (key → tuple location) into tuple bytes means a heap
//! fetch per row. To avoid a fresh page handle per row, [`Scan`] keeps the
//! *current* page pinned and reuses it for consecutive rows that live on it —
//! one [`filesystem::ReadPage`] per page, not per tuple. Because each yielded
//! [`ScanRow`] borrows that held page, this is a lending iterator: finish with a
//! row before pulling the next.

use filesystem::{FileVfs, Heap, PageTuples, TupleView};
use lsm::KVPair;

use crate::StorageError;
use crate::engine::decode_loc;

/// One scanned row: its key, and a borrowed view of its tuple bytes that shares
/// the current page's single pin.
pub struct ScanRow<'p> {
    /// The row's key (the index key).
    pub key: Vec<u8>,
    /// A zero-copy view of the tuple's bytes. Copy out (decode) to materialize.
    pub tuple: TupleView<'p, FileVfs>,
}

/// A lending scan over the engine. Pulls index entries (key → location) and
/// resolves each through the heap, holding one pinned page at a time.
pub struct Scan<'e> {
    index: Box<dyn Iterator<Item = Result<KVPair, StorageError>> + 'e>,
    heap: &'e Heap<FileVfs>,
    current: Option<PageTuples<FileVfs>>,
}

impl<'e> Scan<'e> {
    pub(crate) fn new(
        index: Box<dyn Iterator<Item = Result<KVPair, StorageError>> + 'e>,
        heap: &'e Heap<FileVfs>,
    ) -> Self {
        Self {
            index,
            heap,
            current: None,
        }
    }

    /// Advance to the next row. The returned [`ScanRow`] borrows the held page,
    /// so it must be dropped before the next call (a lending iterator — hence
    /// the inherent `next`, not [`Iterator`]).
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<Result<ScanRow<'_>, StorageError>> {
        let (key, loc_bytes) = match self.index.next()? {
            Ok(kv) => kv,
            Err(e) => return Some(Err(e)),
        };
        let loc = match decode_loc(&loc_bytes) {
            Ok(loc) => loc,
            Err(e) => return Some(Err(e)),
        };
        // Reuse the pinned page if this row is on it; otherwise swap it in.
        if self.current.as_ref().map(PageTuples::page_id) != Some(loc.page_id) {
            match self.heap.page(loc.page_id) {
                Ok(page) => self.current = Some(page),
                Err(e) => return Some(Err(e.into())),
            }
        }
        let page = self.current.as_ref().expect("page just pinned");
        Some(Ok(ScanRow {
            key,
            tuple: page.tuple(loc.slot_id),
        }))
    }
}
