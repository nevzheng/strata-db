//! Eviction policy — the contract a cache uses to choose victims. Concrete
//! policies (LRU, LRU-K, CLOCK, LFU) live in
//! [`eviction_policies`](super::eviction_policies).
//!
//! The policy is generic over the *id* it tracks, so one contract serves both
//! caches: the buffer pool tracks frame slots ([`FrameId`]); the read-through
//! [`Cache`](super::Cache) tracks its own keys.

/// Index of a frame in the buffer pool's slot array.
pub type FrameId = usize;

/// Tracks access recency/frequency and picks eviction victims. The cache calls
/// [`record_access`](EvictionPolicy::record_access) on every fetch and
/// [`remove`](EvictionPolicy::remove) when an entry leaves the cache.
pub trait EvictionPolicy<Id> {
    /// Note that `id` was just accessed.
    fn record_access(&mut self, id: Id);

    /// Choose an id to evict among those for which `evictable` returns true
    /// (e.g. currently unpinned). Returns `None` if none qualify.
    ///
    /// Takes a predicate because the coldest entry may be un-evictable (pinned
    /// in the pool), in which case the next-coldest evictable one is chosen.
    fn evict_candidate(&self, evictable: &dyn Fn(Id) -> bool) -> Option<Id>;

    /// Drop all history for `id` (it left the cache).
    fn remove(&mut self, id: Id);
}
