//! `MemoryPool` — the engine's single source of raw memory.
//!
//! Hands out [`Slab`]s: contiguous, owned spans of bytes. The pool enforces one
//! global byte cap and tracks live usage, so the whole engine's footprint is
//! bounded and observable from one place. Consumers — the block store, the
//! buffer pool, caches — carve structure out of a `Slab`; the slab itself is
//! "just a rock," purpose-agnostic raw memory.
//!
//! v1 is pure RAM. Placement knobs (mmap/spill backing) are a future addition
//! behind the same API; persistence of *data* is a storage concern — the
//! [`BlockStore`](crate::BlockStore) owns the file — never the pool's.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Allocator facade over the OS heap. Cheap to clone — every clone shares one
/// cap and one usage counter, so all `Slab`s drawn from it (or its clones)
/// count against the same budget.
#[derive(Clone)]
pub struct MemoryPool {
    inner: Arc<Inner>,
}

struct Inner {
    /// Global byte ceiling; `0` means unbounded.
    cap: usize,
    /// Bytes currently live across all outstanding slabs.
    in_use: AtomicUsize,
}

/// An allocation that would push the pool past its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfMemory {
    /// Bytes the failed allocation asked for.
    pub requested: usize,
    /// Bytes already in use when it was attempted.
    pub in_use: usize,
    /// The pool's cap.
    pub cap: usize,
}

impl std::fmt::Display for OutOfMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "memory pool exhausted: requested {} with {} of {} bytes in use",
            self.requested, self.in_use, self.cap
        )
    }
}

impl std::error::Error for OutOfMemory {}

impl MemoryPool {
    /// A pool capped at `cap` bytes. `cap == 0` means unbounded.
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                cap,
                in_use: AtomicUsize::new(0),
            }),
        }
    }

    /// Allocate a zeroed `size`-byte [`Slab`], or fail if it would exceed the cap.
    ///
    /// The reservation is taken before the allocation and released on the
    /// slab's drop, so `in_use` always reflects live slabs — and a rejected
    /// allocation rolls its reservation back, leaving the pool unchanged.
    pub fn allocate(&self, size: usize) -> Result<Slab, OutOfMemory> {
        let prev = self.inner.in_use.fetch_add(size, Ordering::Relaxed);
        if self.inner.cap != 0 && prev + size > self.inner.cap {
            self.inner.in_use.fetch_sub(size, Ordering::Relaxed);
            return Err(OutOfMemory {
                requested: size,
                in_use: prev,
                cap: self.inner.cap,
            });
        }
        Ok(Slab {
            bytes: vec![0u8; size].into_boxed_slice(),
            pool: self.inner.clone(),
        })
    }

    /// Bytes currently live across all outstanding slabs.
    pub fn in_use(&self) -> usize {
        self.inner.in_use.load(Ordering::Relaxed)
    }

    /// The pool's byte cap (`0` = unbounded).
    pub fn cap(&self) -> usize {
        self.inner.cap
    }
}

/// A contiguous, owned span of raw bytes from a [`MemoryPool`] — "just a rock."
/// It owns its memory and imposes no structure; consumers interpret it. On drop
/// it returns its bytes to the pool's accounting.
pub struct Slab {
    bytes: Box<[u8]>,
    pool: Arc<Inner>,
}

impl Slab {
    /// The slab's size in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Read-only view of the bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Mutable view of the bytes.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}

impl std::fmt::Debug for Slab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slab").field("len", &self.len()).finish()
    }
}

impl Drop for Slab {
    fn drop(&mut self) {
        self.pool
            .in_use
            .fetch_sub(self.bytes.len(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_zeroed_and_tracks_usage() {
        let pool = MemoryPool::new(1024);
        let slab = pool.allocate(256).unwrap();
        assert_eq!(slab.len(), 256);
        assert!(slab.as_slice().iter().all(|&b| b == 0), "slabs come zeroed");
        assert_eq!(pool.in_use(), 256);
    }

    #[test]
    fn drop_returns_bytes_to_the_pool() {
        let pool = MemoryPool::new(0);
        {
            let _a = pool.allocate(100).unwrap();
            let _b = pool.allocate(50).unwrap();
            assert_eq!(pool.in_use(), 150);
        }
        assert_eq!(pool.in_use(), 0, "drops release the reservation");
    }

    #[test]
    fn allocation_past_cap_fails_without_leaking_reservation() {
        let pool = MemoryPool::new(128);
        let _a = pool.allocate(100).unwrap();
        let err = pool.allocate(50).unwrap_err();
        assert_eq!(err.requested, 50);
        assert_eq!(err.cap, 128);
        // The rejected reservation rolled back, so the remaining room is intact.
        assert_eq!(pool.in_use(), 100);
        assert!(pool.allocate(28).is_ok());
    }

    #[test]
    fn unbounded_pool_never_rejects() {
        let pool = MemoryPool::new(0);
        let big = pool.allocate(8 * 1024 * 1024).unwrap();
        assert_eq!(big.len(), 8 * 1024 * 1024);
    }

    #[test]
    fn clones_share_one_budget() {
        let pool = MemoryPool::new(0);
        let clone = pool.clone();
        let _s = clone.allocate(64).unwrap();
        assert_eq!(pool.in_use(), 64, "a clone draws from the same counter");
    }

    #[test]
    fn mutating_a_slab_writes_through() {
        let pool = MemoryPool::new(0);
        let mut slab = pool.allocate(4).unwrap();
        slab.as_mut_slice().copy_from_slice(b"MARK");
        assert_eq!(slab.as_slice(), b"MARK");
    }
}
