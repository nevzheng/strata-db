//! The page cache — the read path for on-disk SSTable data.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;

use super::page::{Page, PageId};
use crate::config::PageCacheConfig;

/// A read-through cache of SSTable pages, keyed by [`PageId`].
///
/// On a miss the caller's loader reads the page (it knows the file layout);
/// the cache just memoizes the result. Single-threaded (`RefCell`). The
/// [`PageCacheConfig`] sets the size budget and eviction policy; enforcing
/// them is not wired up yet, so the cache is currently unbounded.
#[derive(Debug)]
pub struct SstPageCache {
    pages: RefCell<HashMap<PageId, Page>>,
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
            pages: RefCell::new(HashMap::new()),
            config,
        }
    }

    /// The cache's size budget and eviction policy.
    pub fn config(&self) -> PageCacheConfig {
        self.config
    }

    /// Return the cached page for `id`, or load it via `load` and cache it.
    pub fn fetch(&self, id: PageId, load: impl FnOnce() -> io::Result<Page>) -> io::Result<Page> {
        if let Some(page) = self.pages.borrow().get(&id) {
            return Ok(page.clone());
        }
        let page = load()?;
        self.pages.borrow_mut().insert(id, page.clone());
        Ok(page)
    }

    /// Number of pages currently cached.
    pub fn len(&self) -> usize {
        self.pages.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.borrow().is_empty()
    }

    /// Drop all cached pages.
    pub fn clear(&self) {
        self.pages.borrow_mut().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SsTableId;
    use std::cell::Cell;

    fn page_id(sst: u64, page: u32) -> PageId {
        PageId {
            table: SsTableId(sst),
            page_index: page,
        }
    }

    #[test]
    fn loads_on_miss_then_serves_from_cache() {
        let cache = SstPageCache::default();
        let id = page_id(7, 0);
        let loads = Cell::new(0);
        let load = || {
            loads.set(loads.get() + 1);
            Ok(Page::new(b"hello".to_vec()))
        };

        let first = cache.fetch(id, load).unwrap();
        let second = cache.fetch(id, load).unwrap();

        assert_eq!(first.bytes(), b"hello");
        assert_eq!(second.bytes(), b"hello");
        assert_eq!(loads.get(), 1, "second fetch must hit the cache");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn distinct_ids_are_cached_separately() {
        let cache = SstPageCache::default();
        cache
            .fetch(page_id(1, 0), || Ok(Page::new(b"a".to_vec())))
            .unwrap();
        cache
            .fetch(page_id(1, 1), || Ok(Page::new(b"b".to_vec())))
            .unwrap();
        assert_eq!(cache.len(), 2);
    }
}
