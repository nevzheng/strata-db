//! A read-through *memo* cache: maps keys to cheaply-cloneable value handles,
//! under a size budget and a pluggable [`EvictionPolicy`].
//!
//! Unlike the buffer pool it owns no frames and pins nothing — it memoizes
//! immutable values a loader produces on a miss (SSTable blocks, headers, …).
//! Crucially it hands back an **owned** handle (a clone, e.g. an `Arc`), never a
//! borrow tied to the cache: the caller holds the bytes without holding the
//! cache, which keeps the door open for a thread-safe version and avoids
//! decoding on the hot path (`V` can be the raw bytes handle).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::Hash;

use crate::policies::EvictionPolicy;

/// A value's cache cost, for the byte budget. A bytes handle weighs its length;
/// a decoded value can weigh its in-memory footprint.
pub trait Weight {
    fn weight(&self) -> usize;
}

/// How much a [`Cache`] may hold before it evicts.
#[derive(Debug, Clone, Copy)]
pub enum Budget {
    /// No limit — never evicts.
    Unbounded,
    /// At most this many entries.
    Entries(usize),
    /// At most this many bytes, summed over [`Weight::weight`].
    Bytes(usize),
}

/// A read-through cache of `K → V` with a size `budget` and an eviction policy
/// `P`. Single-threaded (`RefCell`); the handles it returns are owned, so they
/// outlive any borrow of the cache.
pub struct Cache<K, V, P> {
    entries: RefCell<HashMap<K, V>>,
    policy: RefCell<P>,
    bytes: Cell<usize>,
    budget: Budget,
}

impl<K, V, P> Cache<K, V, P>
where
    K: Copy + Eq + Hash,
    V: Weight + Clone,
    P: EvictionPolicy<K>,
{
    /// A cache with the given size `budget` and eviction `policy`.
    pub fn new(budget: Budget, policy: P) -> Self {
        Self {
            entries: RefCell::new(HashMap::new()),
            policy: RefCell::new(policy),
            bytes: Cell::new(0),
            budget,
        }
    }

    /// Return the handle for `key`, loading and caching it on a miss. The
    /// returned handle is **owned** (a clone), so once this returns the caller
    /// no longer borrows the cache.
    pub fn get_or_load<E>(
        &self,
        key: K,
        load: impl FnOnce() -> Result<V, E>,
    ) -> Result<V, E> {
        if let Some(v) = self.entries.borrow().get(&key).cloned() {
            self.policy.borrow_mut().record_access(key);
            return Ok(v);
        }
        let v = load()?;
        self.bytes.set(self.bytes.get() + v.weight());
        self.entries.borrow_mut().insert(key, v.clone());
        self.policy.borrow_mut().record_access(key);
        self.evict_to_budget();
        Ok(v)
    }

    /// The cached handle for `key`, if present — recording the access. `None`
    /// on a miss (no load).
    pub fn get(&self, key: K) -> Option<V> {
        let v = self.entries.borrow().get(&key).cloned();
        if v.is_some() {
            self.policy.borrow_mut().record_access(key);
        }
        v
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total weight of everything cached.
    pub fn bytes(&self) -> usize {
        self.bytes.get()
    }

    /// Drop everything cached.
    pub fn clear(&self) {
        self.entries.borrow_mut().clear();
        self.bytes.set(0);
    }

    fn over_budget(&self) -> bool {
        match self.budget {
            Budget::Unbounded => false,
            Budget::Entries(max) => self.len() > max,
            Budget::Bytes(max) => self.bytes.get() > max,
        }
    }

    /// Evict victims (per the policy) until back within budget.
    fn evict_to_budget(&self) {
        while self.over_budget() {
            let victim = self.policy.borrow().evict_candidate(&|_| true);
            let Some(key) = victim else { break };
            if let Some(v) = self.entries.borrow_mut().remove(&key) {
                self.bytes.set(self.bytes.get().saturating_sub(v.weight()));
            }
            self.policy.borrow_mut().remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policies::Lru;
    use std::cell::Cell;

    impl Weight for Vec<u8> {
        fn weight(&self) -> usize {
            self.len()
        }
    }

    fn cache(budget: Budget) -> Cache<u32, Vec<u8>, Lru<u32>> {
        Cache::new(budget, Lru::new())
    }

    #[test]
    fn loads_on_miss_then_hits() {
        let c = cache(Budget::Unbounded);
        let loads = Cell::new(0);
        let load = || {
            loads.set(loads.get() + 1);
            Ok::<_, ()>(vec![1, 2, 3])
        };
        c.get_or_load(7, load).unwrap();
        c.get_or_load(7, load).unwrap();
        assert_eq!(loads.get(), 1, "second fetch must hit the cache");
    }

    #[test]
    fn evicts_lru_over_entry_budget() {
        let c = cache(Budget::Entries(2));
        c.get_or_load(0, || Ok::<_, ()>(vec![0])).unwrap();
        c.get_or_load(1, || Ok::<_, ()>(vec![1])).unwrap();
        c.get_or_load(2, || Ok::<_, ()>(vec![2])).unwrap(); // over budget → evict key 0
        assert_eq!(c.len(), 2);

        let reloaded = Cell::new(false);
        c.get_or_load(0, || {
            reloaded.set(true);
            Ok::<_, ()>(vec![0])
        })
        .unwrap();
        assert!(reloaded.get(), "the LRU entry (key 0) should have been evicted");
    }

    #[test]
    fn evicts_to_byte_budget() {
        let c = cache(Budget::Bytes(4));
        c.get_or_load(0, || Ok::<_, ()>(vec![0u8; 3])).unwrap();
        c.get_or_load(1, || Ok::<_, ()>(vec![0u8; 3])).unwrap(); // 6 > 4 → evict
        assert!(c.bytes() <= 4, "byte budget should hold after eviction");
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn unbounded_never_evicts() {
        let c = cache(Budget::Unbounded);
        for i in 0..100 {
            c.get_or_load(i, || Ok::<_, ()>(vec![0u8; 8])).unwrap();
        }
        assert_eq!(c.len(), 100);
    }
}
