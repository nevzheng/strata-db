//! Eviction policy — pluggable so it can be swapped for benchmarking.
//!
//! The default is **LRU-K with K=2**: evict the frame whose K-th-most-recent
//! access is furthest in the past. A frame accessed fewer than K times has no
//! K-th access and is treated as infinitely old, so it is evicted first. This
//! resists scan pollution — a big sequential scan touches each page once, so
//! those single-touch frames are reclaimed before twice-touched hot pages.

use std::collections::HashMap;
use std::collections::VecDeque;

/// Index of a frame in the cache's pool.
pub type FrameId = usize;

/// Tracks access recency and picks eviction victims. The cache calls
/// [`record_access`](EvictionPolicy::record_access) on every fetch and
/// [`remove`](EvictionPolicy::remove) when a frame is evicted or freed.
pub trait EvictionPolicy {
    /// Note that `frame` was just accessed.
    fn record_access(&mut self, frame: FrameId);

    /// Choose a frame to evict among those for which `evictable` returns true
    /// (i.e. currently unpinned). Returns `None` if none qualify.
    ///
    /// The design's sketch took no predicate; the real cache needs one, because
    /// the coldest frame may be pinned and thus un-evictable.
    fn evict_candidate(&self, evictable: &dyn Fn(FrameId) -> bool) -> Option<FrameId>;

    /// Drop all history for `frame` (it left the pool).
    fn remove(&mut self, frame: FrameId);
}

/// LRU-K. `hist[f]` holds up to K recent access ticks, newest at the front.
pub struct LruK {
    k: usize,
    clock: u64,
    hist: HashMap<FrameId, VecDeque<u64>>,
}

impl LruK {
    /// An LRU-K policy with the given K (K=2 is the design default).
    pub fn new(k: usize) -> Self {
        assert!(k >= 1, "LRU-K needs K >= 1");
        Self {
            k,
            clock: 0,
            hist: HashMap::new(),
        }
    }
}

impl EvictionPolicy for LruK {
    fn record_access(&mut self, frame: FrameId) {
        self.clock += 1;
        let ticks = self.hist.entry(frame).or_default();
        ticks.push_front(self.clock);
        ticks.truncate(self.k);
    }

    fn evict_candidate(&self, evictable: &dyn Fn(FrameId) -> bool) -> Option<FrameId> {
        // Rank by (K-th access tick, most-recent tick), smallest first. A frame
        // with < K accesses scores 0 for the K-th tick, sorting it ahead of any
        // fully-warmed frame — exactly the "evict cold pages first" rule.
        self.hist
            .iter()
            .filter(|(f, _)| evictable(**f))
            .min_by_key(|(_, ticks)| {
                let kth = if ticks.len() < self.k {
                    0
                } else {
                    ticks[self.k - 1]
                };
                (kth, ticks[0])
            })
            .map(|(f, _)| *f)
    }

    fn remove(&mut self, frame: FrameId) {
        self.hist.remove(&frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn always(_: FrameId) -> bool {
        true
    }

    #[test]
    fn cold_frame_evicted_before_warm_one() {
        let mut p = LruK::new(2);
        // Frame 0 accessed twice (warm), frame 1 once (cold).
        p.record_access(0);
        p.record_access(1);
        p.record_access(0);
        assert_eq!(p.evict_candidate(&always), Some(1));
    }

    #[test]
    fn among_warm_frames_oldest_kth_loses() {
        let mut p = LruK::new(2);
        p.record_access(0);
        p.record_access(1);
        p.record_access(0); // 0's 2nd-most-recent is older than 1's will be
        p.record_access(1);
        // 0's K-th (2nd) access predates 1's, so 0 is evicted.
        assert_eq!(p.evict_candidate(&always), Some(0));
    }

    #[test]
    fn pinned_frames_are_skipped() {
        let mut p = LruK::new(2);
        p.record_access(0);
        p.record_access(1);
        // Frame 0 is the coldest but pinned; the policy must pick frame 1.
        assert_eq!(p.evict_candidate(&|f| f != 0), Some(1));
    }
}
