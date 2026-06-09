//! The engine's scan return type.
//!
//! [`ScanIterator`] is a type-erased stream of resolved key-value pairs.
//! The k-way merge it is built on top of is the generic
//! [`lsm::MergeIterator`].

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

#[cfg(test)]
mod tests {
    use super::*;
    use lsm::LsmError;

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
            Err(LsmError::InternalError("boom".into()).into()),
            Ok((b"b".to_vec(), b"2".to_vec())),
        ];
        let mut scan = ScanIterator::new(items.into_iter());

        assert!(scan.next().unwrap().is_ok());
        assert!(scan.next().unwrap().is_err());
        // Errors don't terminate — pulling again yields the next item.
        assert!(scan.next().unwrap().is_ok());
        assert!(scan.next().is_none());
    }
}
