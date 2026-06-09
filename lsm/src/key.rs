//! Keys, values, and key ranges — the atoms the tree is built from.

use std::cmp::Ordering;

/// Whether a record stores a value (`Put`) or marks a deletion (`Delete`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpType {
    Put,
    Delete,
}

/// A user key tagged with its version and operation.
///
/// Ordering is the backbone of the tree: user key ascending, then sequence
/// **descending**, so the newest version of a key sorts first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalKey {
    pub user_key: Vec<u8>,
    pub seq: u64,
    pub op: OpType,
}

impl Ord for InternalKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.user_key
            .cmp(&other.user_key)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for InternalKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// One versioned record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyValue {
    pub key: InternalKey,
    pub value: Vec<u8>, // empty for a tombstone
}

/// A resolved user key/value pair, with versioning already applied.
pub type KVPair = (Vec<u8>, Vec<u8>);

/// Inclusive `[min, max]` user-key span, carried at every tree node so work
/// outside it can be skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRange {
    pub min: Vec<u8>,
    pub max: Vec<u8>,
}

impl KeyRange {
    pub fn contains(&self, key: &[u8]) -> bool {
        self.min.as_slice() <= key && key <= self.max.as_slice()
    }

    /// Smallest range covering both — rolls file ranges up into run/level ranges.
    pub fn union(&self, other: &KeyRange) -> KeyRange {
        KeyRange {
            min: self.min.clone().min(other.min.clone()),
            max: self.max.clone().max(other.max.clone()),
        }
    }
}
