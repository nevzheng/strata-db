//! Concrete [`EvictionPolicy`] implementations, all generic over the tracked
//! id so they work for both the buffer pool ([`FrameId`](super::FrameId)) and
//! the read-through [`Cache`](super::Cache).
//!
//! These are deliberately a menu to experiment with — swap one in via
//! `with_policy` / [`Cache::new`](super::Cache::new) and benchmark:
//!
//! - [`Lru`]   — evict the least-recently-used. Simple, cheap, scan-fragile.
//! - [`LruK`]  — LRU-K (K=2 default): rank by the K-th-most-recent access, so a
//!   single-touch scan can't evict twice-touched hot entries. The pool default.
//! - [`Clock`] — second-chance approximation of LRU over a ring; O(1)-ish, no
//!   per-access bookkeeping beyond a reference bit.
//! - [`Lfu`]   — evict the least-frequently-used, ties broken by recency.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

use super::EvictionPolicy;

/// Least-recently-used: evict the entry whose last access is furthest in the past.
pub struct Lru<Id> {
    clock: u64,
    last: HashMap<Id, u64>,
}

impl<Id> Default for Lru<Id> {
    fn default() -> Self {
        Self {
            clock: 0,
            last: HashMap::new(),
        }
    }
}

impl<Id> Lru<Id> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<Id: Copy + Eq + Hash> EvictionPolicy<Id> for Lru<Id> {
    fn record_access(&mut self, id: Id) {
        self.clock += 1;
        self.last.insert(id, self.clock);
    }

    fn evict_candidate(&self, evictable: &dyn Fn(Id) -> bool) -> Option<Id> {
        self.last
            .iter()
            .filter(|(id, _)| evictable(**id))
            .min_by_key(|(_, tick)| **tick)
            .map(|(id, _)| *id)
    }

    fn remove(&mut self, id: Id) {
        self.last.remove(&id);
    }
}

/// LRU-K: rank by the K-th-most-recent access. An entry accessed fewer than K
/// times has no K-th access and is treated as infinitely old, so cold
/// single-touch entries are reclaimed before twice-touched hot ones.
pub struct LruK<Id> {
    k: usize,
    clock: u64,
    hist: HashMap<Id, VecDeque<u64>>,
}

impl<Id> LruK<Id> {
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

impl<Id: Copy + Eq + Hash> EvictionPolicy<Id> for LruK<Id> {
    fn record_access(&mut self, id: Id) {
        self.clock += 1;
        let ticks = self.hist.entry(id).or_default();
        ticks.push_front(self.clock);
        ticks.truncate(self.k);
    }

    fn evict_candidate(&self, evictable: &dyn Fn(Id) -> bool) -> Option<Id> {
        // Rank by (K-th access tick, most-recent tick), smallest first. Fewer
        // than K accesses scores 0 for the K-th tick, sorting it ahead of any
        // fully-warmed entry — the "evict cold first" rule.
        self.hist
            .iter()
            .filter(|(id, _)| evictable(**id))
            .min_by_key(|(_, ticks)| {
                let kth = if ticks.len() < self.k {
                    0
                } else {
                    ticks[self.k - 1]
                };
                (kth, ticks[0])
            })
            .map(|(id, _)| *id)
    }

    fn remove(&mut self, id: Id) {
        self.hist.remove(&id);
    }
}

/// CLOCK: a second-chance approximation of LRU. Entries sit on a ring with a
/// reference bit set on access; the hand sweeps, clearing set bits (a second
/// chance) and evicting the first entry it finds already clear.
///
/// `evict_candidate` mutates the ring (the sweep) through interior mutability,
/// so it stays `&self` like the other policies.
pub struct Clock<Id> {
    ring: RefCell<VecDeque<Id>>,
    referenced: RefCell<HashMap<Id, bool>>,
}

impl<Id> Default for Clock<Id> {
    fn default() -> Self {
        Self {
            ring: RefCell::new(VecDeque::new()),
            referenced: RefCell::new(HashMap::new()),
        }
    }
}

impl<Id> Clock<Id> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<Id: Copy + Eq + Hash> EvictionPolicy<Id> for Clock<Id> {
    fn record_access(&mut self, id: Id) {
        let mut referenced = self.referenced.borrow_mut();
        if !referenced.contains_key(&id) {
            self.ring.borrow_mut().push_back(id);
        }
        referenced.insert(id, true);
    }

    fn evict_candidate(&self, evictable: &dyn Fn(Id) -> bool) -> Option<Id> {
        let mut ring = self.ring.borrow_mut();
        let mut referenced = self.referenced.borrow_mut();
        // At most two sweeps: one to clear every set reference bit, one to evict.
        let max_steps = ring.len() * 2 + 1;
        for _ in 0..max_steps {
            let id = ring.pop_front()?;
            ring.push_back(id);
            if !evictable(id) {
                continue;
            }
            match referenced.get(&id).copied().unwrap_or(false) {
                true => {
                    referenced.insert(id, false); // second chance
                }
                false => return Some(id),
            }
        }
        None
    }

    fn remove(&mut self, id: Id) {
        self.referenced.borrow_mut().remove(&id);
        self.ring.borrow_mut().retain(|x| *x != id);
    }
}

/// LFU: evict the least-frequently-used; ties broken by least-recently-used.
pub struct Lfu<Id> {
    clock: u64,
    freq: HashMap<Id, (u64, u64)>, // (access count, last-access tick)
}

impl<Id> Default for Lfu<Id> {
    fn default() -> Self {
        Self {
            clock: 0,
            freq: HashMap::new(),
        }
    }
}

impl<Id> Lfu<Id> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<Id: Copy + Eq + Hash> EvictionPolicy<Id> for Lfu<Id> {
    fn record_access(&mut self, id: Id) {
        self.clock += 1;
        let e = self.freq.entry(id).or_insert((0, 0));
        e.0 += 1;
        e.1 = self.clock;
    }

    fn evict_candidate(&self, evictable: &dyn Fn(Id) -> bool) -> Option<Id> {
        self.freq
            .iter()
            .filter(|(id, _)| evictable(**id))
            .min_by_key(|(_, (count, tick))| (*count, *tick))
            .map(|(id, _)| *id)
    }

    fn remove(&mut self, id: Id) {
        self.freq.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn always(_: usize) -> bool {
        true
    }

    #[test]
    fn lru_evicts_least_recently_used() {
        let mut p: Lru<usize> = Lru::new();
        p.record_access(0);
        p.record_access(1);
        p.record_access(0); // 1 is now the least-recently-used
        assert_eq!(p.evict_candidate(&always), Some(1));
    }

    #[test]
    fn lruk_evicts_cold_before_warm() {
        let mut p: LruK<usize> = LruK::new(2);
        p.record_access(0);
        p.record_access(1);
        p.record_access(0); // 0 warm (2 touches), 1 cold (1 touch)
        assert_eq!(p.evict_candidate(&always), Some(1));
    }

    #[test]
    fn lruk_skips_pinned() {
        let mut p: LruK<usize> = LruK::new(2);
        p.record_access(0);
        p.record_access(1);
        // 0 is coldest but pinned; must pick 1.
        assert_eq!(p.evict_candidate(&|f| f != 0), Some(1));
    }

    #[test]
    fn clock_gives_referenced_entry_a_second_chance() {
        let mut p: Clock<usize> = Clock::new();
        p.record_access(0); // ref bit set
        p.record_access(1); // ref bit set
        // Both referenced: the first sweep clears bits, then 0 (oldest) evicts.
        assert_eq!(p.evict_candidate(&always), Some(0));
        // Touch 0 again so it survives over 1.
        p.record_access(0);
        assert_eq!(p.evict_candidate(&always), Some(1));
    }

    #[test]
    fn lfu_evicts_least_frequent() {
        let mut p: Lfu<usize> = Lfu::new();
        p.record_access(0);
        p.record_access(0);
        p.record_access(1); // 1 used once, 0 used twice
        assert_eq!(p.evict_candidate(&always), Some(1));
    }
}
