//! Read-path iterators: the generic k-way [`MergeIterator`], the [`KvStream`]
//! every scannable node yields, and the [`Scan`] trait that produces it.

use std::cmp::Ordering;

use crate::error::{LsmError, ReadError};
use crate::key::{KVPair, KeyValue, OpType};

/// A boxed, sorted stream of versioned records — what every scannable source
/// yields, and what merges and concats compose. Lazy: producers fault blocks
/// in as the consumer pulls.
pub type KvStream<'a> = Box<dyn Iterator<Item = Result<KeyValue, ReadError>> + 'a>;

/// K-way merge over several iterators with a caller-supplied ordering. Each
/// pull yields the smallest item across all sources; ties resolve in source
/// order.
///
/// Linear scan over the source heads per pull — O(k). Deliberate: an LSM read
/// touches only a handful of sources, where the linear path beats a heap.
pub struct MergeIterator<I: Iterator, F> {
    sources: Vec<Source<I>>,
    cmp: F,
}

struct Source<I: Iterator> {
    /// Next item to emit, or `None` if this source is exhausted.
    head: Option<I::Item>,
    iter: I,
}

impl<I, F> MergeIterator<I, F>
where
    I: Iterator,
    F: FnMut(&I::Item, &I::Item) -> Ordering,
{
    pub fn new(sources: Vec<I>, cmp: F) -> Self {
        let sources = sources
            .into_iter()
            .map(|mut iter| Source {
                head: iter.next(),
                iter,
            })
            .collect();
        Self { sources, cmp }
    }
}

impl<I, F> Iterator for MergeIterator<I, F>
where
    I: Iterator,
    F: FnMut(&I::Item, &I::Item) -> Ordering,
{
    type Item = I::Item;

    fn next(&mut self) -> Option<Self::Item> {
        let mut min_idx: Option<usize> = None;
        for i in 0..self.sources.len() {
            if self.sources[i].head.is_none() {
                continue;
            }
            match min_idx {
                None => min_idx = Some(i),
                Some(j) => {
                    let a = self.sources[i].head.as_ref().unwrap();
                    let b = self.sources[j].head.as_ref().unwrap();
                    if (self.cmp)(a, b) == Ordering::Less {
                        min_idx = Some(i);
                    }
                }
            }
        }
        let idx = min_idx?;
        let result = self.sources[idx].head.take();
        self.sources[idx].head = self.sources[idx].iter.next();
        result
    }
}

/// Collapses a sorted stream (user key ascending, seq descending) into one
/// [`KVPair`] per user key — the newest version — dropping tombstones.
pub struct VersionResolver<I> {
    inner: I,
    last_key: Option<Vec<u8>>,
}

impl<I> VersionResolver<I> {
    pub fn new(inner: I) -> Self {
        Self {
            inner,
            last_key: None,
        }
    }
}

impl<I> Iterator for VersionResolver<I>
where
    I: Iterator<Item = Result<KeyValue, ReadError>>,
{
    type Item = Result<KVPair, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.inner.next()? {
                Err(e) => return Some(Err(e)),
                Ok(kv) => {
                    // Sorted seq-descending, so the first entry for a key is
                    // the newest; skip any older versions that follow.
                    if self.last_key.as_deref() == Some(kv.key.user_key.as_slice()) {
                        continue;
                    }
                    self.last_key = Some(kv.key.user_key.clone());
                    match kv.key.op {
                        OpType::Put => return Some(Ok((kv.key.user_key, kv.value))),
                        OpType::Delete => continue, // newest is a tombstone → key gone
                    }
                }
            }
        }
    }
}

/// The resolved, user-facing scan result: a stream of [`KVPair`]s.
pub struct ScanIterator<'a> {
    inner: Box<dyn Iterator<Item = Result<KVPair, LsmError>> + 'a>,
}

impl<'a> ScanIterator<'a> {
    pub fn new<I>(iter: I) -> Self
    where
        I: Iterator<Item = Result<KVPair, LsmError>> + 'a,
    {
        Self {
            inner: Box::new(iter),
        }
    }
}

impl Iterator for ScanIterator<'_> {
    type Item = Result<KVPair, LsmError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_sorted_sources() {
        let a = vec![1, 4, 5].into_iter();
        let b = vec![2, 3, 6].into_iter();
        let c = vec![0, 7].into_iter();
        let merged: Vec<i32> =
            MergeIterator::new(vec![a, b, c], |x: &i32, y: &i32| x.cmp(y)).collect();
        assert_eq!(merged, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn handles_empty_sources() {
        let empty: Vec<std::vec::IntoIter<i32>> = vec![];
        let mut merge = MergeIterator::new(empty, |x: &i32, y: &i32| x.cmp(y));
        assert!(merge.next().is_none());
    }

    #[test]
    fn ties_resolve_in_source_order() {
        // Both sources head with 5; source 0 wins (first equal-minimum).
        let merged: Vec<(usize, i32)> = MergeIterator::new(
            vec![vec![(0, 5), (0, 7)].into_iter(), vec![(1, 5)].into_iter()],
            |a: &(usize, i32), b: &(usize, i32)| a.1.cmp(&b.1),
        )
        .collect();
        assert_eq!(merged, vec![(0, 5), (1, 5), (0, 7)]);
    }
}
