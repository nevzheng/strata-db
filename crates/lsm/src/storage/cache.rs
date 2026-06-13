//! The page cache — the read path for on-disk SSTable data.
//!
//! Headers and data blocks are cached separately: a table has one header
//! (keyed by [`SsTableId`]) and many data blocks (keyed by [`PageId`]). Both
//! are read-through; on a miss the caller's loader reads from disk and the
//! cache memoizes the result. Single-threaded.
//!
//! Internally this is two independent [`filesystem::Cache`] instances sharing one
//! generic read-through implementation — one for headers, one for blocks.
//!
//! The [`PageCacheConfig`] gives the size budget ([`SizeConfig`]) and eviction
//! policy ([`CachePolicy`]); when an insert pushes a cache over budget, entries
//! are evicted accordingly. `prefetch_*` are no-ops for now.

use std::io;

use super::page::{Page, PageId};
use crate::SsTableId;
use crate::config::{CachePolicy, PageCacheConfig, SizeConfig};

impl filesystem::Weight for Page {
    fn weight(&self) -> usize {
        self.bytes().len()
    }
}

/// Translate the LSM-facing [`SizeConfig`] into a [`filesystem::Budget`].
fn budget(size: SizeConfig) -> filesystem::Budget {
    match size {
        SizeConfig::Unbounded => filesystem::Budget::Unbounded,
        SizeConfig::Pages(n) => filesystem::Budget::Entries(n),
        SizeConfig::Bytes(n) => filesystem::Budget::Bytes(n),
    }
}

pub struct SstPageCache {
    /// Table headers, keyed by [`SsTableId`].
    headers: filesystem::Cache<SsTableId, Page, filesystem::policies::Lru<SsTableId>>,
    /// Data blocks, keyed by [`PageId`].
    blocks: filesystem::Cache<PageId, Page, filesystem::policies::Lru<PageId>>,
    config: PageCacheConfig,
}

impl Default for SstPageCache {
    fn default() -> Self {
        Self::new(PageCacheConfig::default())
    }
}

impl SstPageCache {
    /// Build the cache from `config`.
    ///
    /// Headers and blocks are separate caches, and each is given the full
    /// configured budget *independently* — the budget is now per-cache, not a
    /// single shared pool. This is deliberate: a large block scan can churn the
    /// block cache without evicting hot headers (and vice versa), since the two
    /// no longer compete for one budget.
    pub fn new(config: PageCacheConfig) -> Self {
        // One `match` per cache because the two policies are distinct types
        // (`Lru<SsTableId>` vs `Lru<PageId>`); a shared closure can't return both.
        let headers = match config.policy {
            CachePolicy::Lru => {
                filesystem::Cache::new(budget(config.size), filesystem::policies::Lru::new())
            }
        };
        let blocks = match config.policy {
            CachePolicy::Lru => {
                filesystem::Cache::new(budget(config.size), filesystem::policies::Lru::new())
            }
        };
        Self {
            headers,
            blocks,
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
        self.headers.get_or_load(id, load)
    }

    /// Return the cached data block for `id`, or load it via `load` and cache it.
    pub fn fetch_block(
        &self,
        id: PageId,
        load: impl FnOnce() -> io::Result<Page>,
    ) -> io::Result<Page> {
        self.blocks.get_or_load(id, load)
    }

    /// Hint that a table's header will be needed soon. No-op for now.
    pub fn prefetch_header(&self, _id: SsTableId) {}

    /// Hint that a data block will be needed soon. No-op for now.
    pub fn prefetch_block(&self, _id: PageId) {}

    /// Total pages cached (headers + data blocks).
    pub fn len(&self) -> usize {
        self.headers.len() + self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop everything cached.
    pub fn clear(&self) {
        self.headers.clear();
        self.blocks.clear();
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
        // Budgets are now per-cache, so `Pages(2)` caps the block cache at two
        // blocks on its own — independent of any headers.
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
    fn header_budget_is_independent_of_blocks() {
        // With per-cache budgets, filling the block cache past its `Pages(2)`
        // budget must not evict a header: the two caches don't share a pool.
        let cache = SstPageCache::new(PageCacheConfig {
            size: SizeConfig::Pages(2),
            policy: CachePolicy::Lru,
        });
        let header_loads = Cell::new(0);
        let header_load = || {
            header_loads.set(header_loads.get() + 1);
            Ok(page(8))
        };
        cache.fetch_header(SsTableId(1), header_load).unwrap();

        // Churn the block cache well past its budget.
        for i in 0..10 {
            cache.fetch_block(block_id(1, i), || Ok(page(8))).unwrap();
        }
        // The header is still cached — fetching it again does not reload.
        cache.fetch_header(SsTableId(1), header_load).unwrap();
        assert_eq!(header_loads.get(), 1, "block churn must not evict headers");
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
