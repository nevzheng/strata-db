//! A [`MemStore`] backed by a [`BTreeMap`].

use std::collections::BTreeMap;
use std::ops::{Bound, RangeBounds};

use crate::error::{ReadError, WriteError};
use crate::key::{InternalKey, OpType};
use crate::store::{MemStore, ReadStore, WriteStore};

/// Default capacity before the memtable should be flushed: 4 MiB.
const DEFAULT_CAPACITY: usize = 4 * 1024 * 1024;

/// A memtable backed by a [`BTreeMap`] keyed by [`InternalKey`], so entries
/// stay in user-key ascending / seq descending order.
pub struct BTreeMemtable {
    store: BTreeMap<InternalKey, Box<[u8]>>,
    capacity: usize,
    size: usize,
}

impl Default for BTreeMemtable {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl BTreeMemtable {
    /// A memtable with the default capacity.
    pub fn new() -> Self {
        Self::default()
    }

    /// Hold up to `capacity` bytes before signalling [`WriteError::StoreFull`].
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            store: BTreeMap::new(),
            capacity,
            size: 0,
        }
    }
}

impl WriteStore for BTreeMemtable {
    fn put(&mut self, key: InternalKey, value: &[u8]) -> Result<(), WriteError> {
        let added = key.user_key.len() + value.len();
        // Accept the first entry even if it alone exceeds capacity, so a
        // single oversized record can never deadlock the flush loop.
        if !self.store.is_empty() && self.size + added > self.capacity {
            return Err(WriteError::StoreFull);
        }
        self.size += added;
        self.store.insert(key, value.into());
        Ok(())
    }
}

impl MemStore for BTreeMemtable {
    fn size(&self) -> usize {
        self.size
    }

    fn clear(&mut self) {
        self.store.clear();
        self.size = 0;
    }
}

impl ReadStore for BTreeMemtable {
    fn get_at(&self, key: &[u8], max_seq: u64) -> Result<Option<Vec<u8>>, ReadError> {
        // The first entry at or after (key, max_seq) is the newest version of
        // `key` visible at `max_seq`, thanks to the seq-descending order.
        let probe = InternalKey {
            user_key: key.to_vec(),
            seq: max_seq,
            op: OpType::Put,
        };
        if let Some((ik, value)) = self.store.range(probe..).next()
            && ik.user_key == key
        {
            return Ok(match ik.op {
                OpType::Put => Some(value.to_vec()),
                OpType::Delete => None,
            });
        }
        Ok(None)
    }

    fn scan_at(
        &self,
        range: impl RangeBounds<Vec<u8>>,
        max_seq: u64,
    ) -> impl Iterator<Item = Result<(InternalKey, Vec<u8>), ReadError>> + '_ {
        let start = match range.start_bound() {
            Bound::Included(k) => Bound::Included(InternalKey {
                user_key: k.clone(),
                seq: u64::MAX,
                op: OpType::Put,
            }),
            Bound::Excluded(k) => Bound::Excluded(InternalKey {
                user_key: k.clone(),
                seq: 0,
                op: OpType::Put,
            }),
            Bound::Unbounded => Bound::Unbounded,
        };
        let end = match range.end_bound() {
            Bound::Included(k) => Bound::Included(InternalKey {
                user_key: k.clone(),
                seq: 0,
                op: OpType::Put,
            }),
            Bound::Excluded(k) => Bound::Excluded(InternalKey {
                user_key: k.clone(),
                seq: u64::MAX,
                op: OpType::Put,
            }),
            Bound::Unbounded => Bound::Unbounded,
        };

        self.store
            .range((start, end))
            .filter(move |(ik, _)| ik.seq <= max_seq)
            .map(|(ik, v)| Ok((ik.clone(), v.to_vec())))
    }
}
