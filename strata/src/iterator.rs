//! Iterators over storage scans.
//!
//! [`ScanIterator`] is the engine's scan return type. [`MergeIterator`]
//! is the k-way merge it uses to interleave a memtable scan with
//! per-level scans in sorted order.

use crate::{KVPair, StorageError};

/// Type-erased iterator over a range scan.
///
/// Boxing lets the engine return a stable type without leaking its
/// source composition (memtable + N levels + merge + resolver) into
/// every caller's signature. Errors are per-row and do not terminate
/// the iterator.
pub struct ScanIterator<'a> {
    inner: Box<dyn Iterator<Item = Result<KVPair, StorageError>> + 'a>,
}

impl<'a> ScanIterator<'a> {
    /// Wrap any fallible iterator of key-value pairs.
    pub fn new<I>(iter: I) -> Self
    where
        I: Iterator<Item = Result<KVPair, StorageError>> + 'a,
    {
        Self {
            inner: Box::new(iter),
        }
    }
}

impl Iterator for ScanIterator<'_> {
    type Item = Result<KVPair, StorageError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

/// K-way merge over multiple iterators with a user-supplied ordering.
/// Each pull yields the smallest item across all sources; ties resolve
/// in source order.
///
/// Linear scan over source heads per pull — O(k). Deliberate: at the
/// engine layer k is small (memtable plus a handful of levels), and
/// the linear path beats a heap on both complexity and constant
/// factor at this scale.
pub struct MergeIterator<I: Iterator, F> {
    sources: Vec<Source<I>>,
    cmp: F,
}

struct Source<I: Iterator> {
    /// Next item to emit from this source, or `None` if exhausted.
    head: Option<I::Item>,
    iter: I,
}

impl<I, F> MergeIterator<I, F>
where
    I: Iterator,
    F: FnMut(&I::Item, &I::Item) -> std::cmp::Ordering,
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
    F: FnMut(&I::Item, &I::Item) -> std::cmp::Ordering,
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
                    if (self.cmp)(a, b) == std::cmp::Ordering::Less {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_yields_items_in_order_then_none() {
        let items: Vec<Result<KVPair, StorageError>> = vec![
            Ok((b"a".to_vec(), b"1".to_vec())),
            Ok((b"b".to_vec(), b"2".to_vec())),
        ];
        let mut scan = ScanIterator::new(items.into_iter());

        assert_eq!(scan.next().unwrap().unwrap().0, b"a".to_vec());
        assert_eq!(scan.next().unwrap().unwrap().0, b"b".to_vec());
        assert!(scan.next().is_none());
    }

    #[test]
    fn scan_surfaces_errors_inline() {
        let items: Vec<Result<KVPair, StorageError>> = vec![
            Ok((b"a".to_vec(), b"1".to_vec())),
            Err(StorageError::InternalError("boom".into())),
            Ok((b"b".to_vec(), b"2".to_vec())),
        ];
        let mut scan = ScanIterator::new(items.into_iter());

        assert!(scan.next().unwrap().is_ok());
        assert!(scan.next().unwrap().is_err());
        // Errors don't terminate — pulling again yields the next item.
        assert!(scan.next().unwrap().is_ok());
        assert!(scan.next().is_none());
    }

    #[test]
    fn merge_yields_globally_sorted_order_across_sources() {
        let a = vec![1, 4, 5].into_iter();
        let b = vec![2, 3, 6].into_iter();
        let c = vec![0, 7].into_iter();
        let merged: Vec<i32> =
            MergeIterator::new(vec![a, b, c], |x: &i32, y: &i32| x.cmp(y)).collect();
        assert_eq!(merged, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn merge_handles_empty_sources() {
        let empty: Vec<std::vec::IntoIter<i32>> = vec![];
        let mut merge = MergeIterator::new(empty, |x: &i32, y: &i32| x.cmp(y));
        assert!(merge.next().is_none());

        let merged: Vec<i32> = MergeIterator::new(
            vec![
                vec![].into_iter(),
                vec![1, 2].into_iter(),
                vec![].into_iter(),
            ],
            |x: &i32, y: &i32| x.cmp(y),
        )
        .collect();
        assert_eq!(merged, vec![1, 2]);
    }

    #[test]
    fn merge_preserves_source_order_on_ties() {
        // When source 0 and source 1 both have a 5 at the head, source 0
        // wins (linear scan picks the first equal-minimum).
        let merged: Vec<(usize, i32)> = MergeIterator::new(
            vec![
                vec![(0, 5), (0, 7)].into_iter(),
                vec![(1, 5), (1, 6)].into_iter(),
            ],
            |a: &(usize, i32), b: &(usize, i32)| a.1.cmp(&b.1),
        )
        .collect();
        assert_eq!(merged, vec![(0, 5), (1, 5), (1, 6), (0, 7)]);
    }
}
