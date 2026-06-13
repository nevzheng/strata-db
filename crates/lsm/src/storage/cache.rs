//! The page cache — the read path for on-disk SSTable data.
//!
//! This is a thin facade over two [`filesystem::Cache`] instances: one for
//! header pages (keyed by [`HeaderId`] — every table's root and child chunks)
//! and one for data blocks (keyed by [`PageId`]). The read-through,
//! memoization, eviction, and budget machinery all live once in
//! `filesystem::Cache` and are tested there; the only thing this layer adds is
//! the *split itself* — two caches with **independent** budgets, so churning
//! blocks can't evict hot headers (and vice versa) — plus the typed `fetch_*`
//! methods and the [`PageCacheConfig`] → [`filesystem::Budget`] adapter.
//! `prefetch_*` are no-ops for now. Single-threaded.

use std::io;

use super::page::{HeaderId, Page, PageId};
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
    /// Header pages — roots and child chunks of every table — keyed by
    /// [`HeaderId`]. One shared cache, so a large table's chunks and a small
    /// table's root coexist and compete for the same budget.
    headers: filesystem::Cache<HeaderId, Page, filesystem::policies::Lru<HeaderId>>,
    /// Data blocks of every table, keyed by [`PageId`].
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

    /// Return the cached header page for `id` (a root or a child chunk), or
    /// load it via `load` and cache it.
    pub fn fetch_header(
        &self,
        id: HeaderId,
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

    /// Hint that a header page will be needed soon. No-op for now.
    pub fn prefetch_header(&self, _id: HeaderId) {}

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
    use crate::SsTableId;
    use std::cell::Cell;

    fn block_id(sst: u64, offset: u64) -> PageId {
        PageId {
            table: SsTableId(sst),
            offset,
        }
    }

    fn root_id(sst: u64) -> HeaderId {
        HeaderId::Root(SsTableId(sst))
    }

    fn page(n: usize) -> Page {
        Page::new(vec![0u8; n])
    }

    #[test]
    fn headers_and_blocks_are_separate() {
        let cache = SstPageCache::default();
        cache.fetch_header(root_id(1), || Ok(page(8))).unwrap();
        cache.fetch_block(block_id(1, 0), || Ok(page(8))).unwrap();
        assert_eq!(cache.len(), 2);

        cache.prefetch_header(root_id(2));
        cache.prefetch_block(block_id(2, 0));
        assert_eq!(cache.len(), 2);
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
        cache.fetch_header(root_id(1), header_load).unwrap();

        // Churn the block cache well past its budget.
        for i in 0..10u64 {
            cache.fetch_block(block_id(1, i), || Ok(page(8))).unwrap();
        }
        // The header is still cached — fetching it again does not reload.
        cache.fetch_header(root_id(1), header_load).unwrap();
        assert_eq!(header_loads.get(), 1, "block churn must not evict headers");
    }
}
