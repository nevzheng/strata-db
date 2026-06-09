//! The page cache — the read path for on-disk SSTable data.
//!
//! Headers and data blocks are cached separately: a table has one header
//! (keyed by [`SsTableId`]) and many data blocks (keyed by [`PageId`]). Both
//! are read-through; on a miss the caller's loader reads from disk and the
//! cache memoizes the result. Single-threaded (`RefCell`). The
//! [`PageCacheConfig`] sets the size budget and eviction policy — enforcing
//! them, and the `prefetch_*` hints, are no-ops for now.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;

use super::page::{Page, PageId};
use crate::SsTableId;
use crate::config::PageCacheConfig;

#[derive(Debug)]
pub struct SstPageCache {
    headers: RefCell<HashMap<SsTableId, Page>>,
    blocks: RefCell<HashMap<PageId, Page>>,
    config: PageCacheConfig,
}

impl Default for SstPageCache {
    fn default() -> Self {
        Self::new(PageCacheConfig::default())
    }
}

impl SstPageCache {
    pub fn new(config: PageCacheConfig) -> Self {
        Self {
            headers: RefCell::new(HashMap::new()),
            blocks: RefCell::new(HashMap::new()),
            config,
        }
    }

    /// The cache's size budget and eviction policy.
    pub fn config(&self) -> PageCacheConfig {
        self.config
    }

    /// Return the cached header for `id`, or load it via `load` and cache it.
    pub fn fetch_header(
        &self,
        id: SsTableId,
        load: impl FnOnce() -> io::Result<Page>,
    ) -> io::Result<Page> {
        if let Some(page) = self.headers.borrow().get(&id) {
            return Ok(page.clone());
        }
        let page = load()?;
        self.headers.borrow_mut().insert(id, page.clone());
        Ok(page)
    }

    /// Return the cached data block for `id`, or load it via `load` and cache it.
    pub fn fetch_block(
        &self,
        id: PageId,
        load: impl FnOnce() -> io::Result<Page>,
    ) -> io::Result<Page> {
        if let Some(page) = self.blocks.borrow().get(&id) {
            return Ok(page.clone());
        }
        let page = load()?;
        self.blocks.borrow_mut().insert(id, page.clone());
        Ok(page)
    }

    /// Hint that a table's header will be needed soon. No-op for now.
    pub fn prefetch_header(&self, _id: SsTableId) {}

    /// Hint that a data block will be needed soon. No-op for now.
    pub fn prefetch_block(&self, _id: PageId) {}

    /// Total pages cached (headers + data blocks).
    pub fn len(&self) -> usize {
        self.headers.borrow().len() + self.blocks.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop everything cached.
    pub fn clear(&self) {
        self.headers.borrow_mut().clear();
        self.blocks.borrow_mut().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn block_id(sst: u64, page: u32) -> PageId {
        PageId {
            table: SsTableId(sst),
            page_index: page,
        }
    }

    #[test]
    fn block_loads_on_miss_then_hits_cache() {
        let cache = SstPageCache::default();
        let id = block_id(7, 0);
        let loads = Cell::new(0);
        let load = || {
            loads.set(loads.get() + 1);
            Ok(Page::new(b"data".to_vec()))
        };

        let first = cache.fetch_block(id, load).unwrap();
        let second = cache.fetch_block(id, load).unwrap();

        assert_eq!(first.bytes(), b"data");
        assert_eq!(second.bytes(), b"data");
        assert_eq!(loads.get(), 1, "second fetch must hit the cache");
    }

    #[test]
    fn headers_and_blocks_are_separate() {
        let cache = SstPageCache::default();
        cache
            .fetch_header(SsTableId(1), || Ok(Page::new(b"h".to_vec())))
            .unwrap();
        cache
            .fetch_block(block_id(1, 0), || Ok(Page::new(b"d".to_vec())))
            .unwrap();
        assert_eq!(cache.len(), 2);

        // Prefetch is a no-op today.
        cache.prefetch_header(SsTableId(2));
        cache.prefetch_block(block_id(2, 0));
        assert_eq!(cache.len(), 2);
    }
}
