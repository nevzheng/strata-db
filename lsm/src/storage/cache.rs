//! The page cache — the read path for on-disk SSTable data.
//!
//! Headers and data blocks are cached separately: a table has one header
//! (keyed by [`SsTableId`]) and many data blocks (keyed by [`PageId`]). Both
//! are read-through; on a miss the caller's loader reads from disk and the
//! cache memoizes the result. Single-threaded (`RefCell`).
//!
//! The [`PageCacheConfig`] gives the size budget ([`SizeConfig`]) and eviction
//! policy ([`CachePolicy`]); when an insert pushes the cache over budget,
//! entries are evicted accordingly. `prefetch_*` are no-ops for now.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;

use super::page::{Page, PageId};
use crate::SsTableId;
use crate::config::{CachePolicy, PageCacheConfig, SizeConfig};

/// A cached page plus the logical time it was last touched (for LRU).
#[derive(Debug)]
struct Cached {
    page: Page,
    tick: u64,
}

#[derive(Debug)]
pub struct SstPageCache {
    headers: RefCell<HashMap<SsTableId, Cached>>,
    blocks: RefCell<HashMap<PageId, Cached>>,
    clock: Cell<u64>,
    bytes: Cell<usize>,
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
            clock: Cell::new(0),
            bytes: Cell::new(0),
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
        {
            let mut headers = self.headers.borrow_mut();
            if let Some(cached) = headers.get_mut(&id) {
                cached.tick = self.next_tick();
                return Ok(cached.page.clone());
            }
        }
        let page = load()?;
        let tick = self.next_tick();
        self.add_bytes(page.bytes().len());
        self.headers.borrow_mut().insert(
            id,
            Cached {
                page: page.clone(),
                tick,
            },
        );
        self.evict_to_budget();
        Ok(page)
    }

    /// Return the cached data block for `id`, or load it via `load` and cache it.
    pub fn fetch_block(
        &self,
        id: PageId,
        load: impl FnOnce() -> io::Result<Page>,
    ) -> io::Result<Page> {
        {
            let mut blocks = self.blocks.borrow_mut();
            if let Some(cached) = blocks.get_mut(&id) {
                cached.tick = self.next_tick();
                return Ok(cached.page.clone());
            }
        }
        let page = load()?;
        let tick = self.next_tick();
        self.add_bytes(page.bytes().len());
        self.blocks.borrow_mut().insert(
            id,
            Cached {
                page: page.clone(),
                tick,
            },
        );
        self.evict_to_budget();
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
        self.bytes.set(0);
    }

    fn next_tick(&self) -> u64 {
        let t = self.clock.get() + 1;
        self.clock.set(t);
        t
    }

    fn add_bytes(&self, n: usize) {
        self.bytes.set(self.bytes.get() + n);
    }

    fn over_budget(&self) -> bool {
        match self.config.size {
            SizeConfig::Unbounded => false,
            SizeConfig::Pages(max) => self.len() > max,
            SizeConfig::Bytes(max) => self.bytes.get() > max,
        }
    }

    /// Evict entries until back within budget, per [`CachePolicy`].
    fn evict_to_budget(&self) {
        while self.over_budget() {
            if !self.evict_one() {
                break; // nothing left to evict
            }
        }
    }

    /// Evict a single entry; returns `false` if the cache is empty.
    fn evict_one(&self) -> bool {
        match self.config.policy {
            CachePolicy::Lru => self.evict_lru(),
        }
    }

    /// Remove the least-recently-used entry across both maps.
    fn evict_lru(&self) -> bool {
        let mut headers = self.headers.borrow_mut();
        let mut blocks = self.blocks.borrow_mut();
        let oldest_header = headers
            .iter()
            .min_by_key(|(_, c)| c.tick)
            .map(|(k, c)| (*k, c.tick));
        let oldest_block = blocks
            .iter()
            .min_by_key(|(_, c)| c.tick)
            .map(|(k, c)| (*k, c.tick));

        let evicted = match (oldest_header, oldest_block) {
            (Some((hk, ht)), Some((_, bt))) if ht <= bt => headers.remove(&hk),
            (Some((hk, _)), None) => headers.remove(&hk),
            (_, Some((bk, _))) => blocks.remove(&bk),
            (None, None) => None,
        };
        match evicted {
            Some(cached) => {
                self.bytes.set(self.bytes.get() - cached.page.bytes().len());
                true
            }
            None => false,
        }
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

    fn page(n: usize) -> Page {
        Page::new(vec![0u8; n])
    }

    #[test]
    fn block_loads_on_miss_then_hits_cache() {
        let cache = SstPageCache::default();
        let id = block_id(7, 0);
        let loads = Cell::new(0);
        let load = || {
            loads.set(loads.get() + 1);
            Ok(page(8))
        };

        cache.fetch_block(id, load).unwrap();
        cache.fetch_block(id, load).unwrap();
        assert_eq!(loads.get(), 1, "second fetch must hit the cache");
    }

    #[test]
    fn headers_and_blocks_are_separate() {
        let cache = SstPageCache::default();
        cache.fetch_header(SsTableId(1), || Ok(page(8))).unwrap();
        cache.fetch_block(block_id(1, 0), || Ok(page(8))).unwrap();
        assert_eq!(cache.len(), 2);

        cache.prefetch_header(SsTableId(2));
        cache.prefetch_block(block_id(2, 0));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn evicts_lru_when_over_budget() {
        let cache = SstPageCache::new(PageCacheConfig {
            size: SizeConfig::Pages(2),
            policy: CachePolicy::Lru,
        });
        cache.fetch_block(block_id(1, 0), || Ok(page(8))).unwrap();
        cache.fetch_block(block_id(1, 1), || Ok(page(8))).unwrap();
        // Third insert is over budget → the LRU (block 0) is evicted.
        cache.fetch_block(block_id(1, 2), || Ok(page(8))).unwrap();
        assert_eq!(cache.len(), 2);

        // Block 0 was evicted, so fetching it runs the loader again.
        let reloaded = Cell::new(false);
        cache
            .fetch_block(block_id(1, 0), || {
                reloaded.set(true);
                Ok(page(8))
            })
            .unwrap();
        assert!(
            reloaded.get(),
            "least-recently-used block should have been evicted"
        );
    }

    #[test]
    fn unbounded_never_evicts() {
        let cache = SstPageCache::new(PageCacheConfig {
            size: SizeConfig::Unbounded,
            policy: CachePolicy::Lru,
        });
        for i in 0..100 {
            cache.fetch_block(block_id(1, i), || Ok(page(8))).unwrap();
        }
        assert_eq!(cache.len(), 100);
    }
}
