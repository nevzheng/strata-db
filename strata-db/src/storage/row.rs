//! Logical row keys.
//!
//! A [`RowKey`] is the tuple that uniquely addresses a single row in the
//! database: `(project_id, dataset_id, table_id, user_key)`. It is the
//! input/output of every read and write on a [`Table`](crate::Table) —
//! the table layer constructs a `RowKey` and asks it to [`encode`](RowKey::encode)
//! itself into bytes for the underlying KV engine.
//!
//! # Why a logical type
//!
//! The byte encoding of a row key is an implementation detail that may
//! change (prefix compression, varint scopes, schema-aware encoding,
//! etc.). Call sites build a `RowKey` and call `encode` / `decode` —
//! the encoding format can evolve without touching them.
//!
//! # Size note
//!
//! In the current encoding the namespace prefix is 48 bytes
//! (three 16-byte UUIDs) plus the user key. This is intentionally
//! "heavy" — we trade compactness for stable, content-addressable IDs
//! that don't require a central allocator. Block-level prefix
//! compression and block compression (future SSTable work) will
//! amortize the on-disk cost.
//!
//! # Range scans
//!
//! Every row in one table shares the same 48-byte prefix. To list all
//! rows in a table, compute that prefix with [`RowKey::table_prefix`]
//! and scan `[prefix .. next_after_prefix(prefix))`.
//!
//! [`crate::TypedStore::scan`] returns a `Vec<KVPair>`. The underlying
//! engine cursor borrows from the `MutexGuard` acquired to read it, and
//! that guard is released at the end of the lock-acquisition
//! expression — so rows are drained into a `Vec` before the cursor
//! escapes the lock's scope.

use uuid::Uuid;

use crate::catalog::ids::{DatasetId, ProjectId, TableId};

const ID_BYTES: usize = 16;
const PREFIX_BYTES: usize = ID_BYTES * 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowKey {
    pub project_id: ProjectId,
    pub dataset_id: DatasetId,
    pub table_id: TableId,
    pub user_key: Vec<u8>,
}

#[derive(Debug)]
pub enum EncodingError {
    Invalid(String),
}

impl RowKey {
    pub fn new(
        project_id: ProjectId,
        dataset_id: DatasetId,
        table_id: TableId,
        user_key: Vec<u8>,
    ) -> Self {
        Self {
            project_id,
            dataset_id,
            table_id,
            user_key,
        }
    }

    /// Encode this row key into bytes for the underlying engine.
    ///
    /// Layout: `| project_id (16) | dataset_id (16) | table_id (16) | user_key (variable) |`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(PREFIX_BYTES + self.user_key.len());
        out.extend_from_slice(self.project_id.as_bytes());
        out.extend_from_slice(self.dataset_id.as_bytes());
        out.extend_from_slice(self.table_id.as_bytes());
        out.extend_from_slice(&self.user_key);
        out
    }

    /// Build the 48-byte key prefix shared by every row in one table.
    ///
    /// Pair with [`next_after_prefix`] to range-scan one table.
    pub fn table_prefix(
        project_id: ProjectId,
        dataset_id: DatasetId,
        table_id: TableId,
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(PREFIX_BYTES);
        out.extend_from_slice(project_id.as_bytes());
        out.extend_from_slice(dataset_id.as_bytes());
        out.extend_from_slice(table_id.as_bytes());
        out
    }

    /// Decode a row key from engine bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, EncodingError> {
        if bytes.len() < PREFIX_BYTES {
            return Err(EncodingError::Invalid(format!(
                "row key shorter than {PREFIX_BYTES}-byte prefix (got {} bytes)",
                bytes.len()
            )));
        }
        let project_id = ProjectId(
            Uuid::from_slice(&bytes[..ID_BYTES])
                .map_err(|e| EncodingError::Invalid(e.to_string()))?,
        );
        let dataset_id = DatasetId(
            Uuid::from_slice(&bytes[ID_BYTES..ID_BYTES * 2])
                .map_err(|e| EncodingError::Invalid(e.to_string()))?,
        );
        let table_id = TableId(
            Uuid::from_slice(&bytes[ID_BYTES * 2..PREFIX_BYTES])
                .map_err(|e| EncodingError::Invalid(e.to_string()))?,
        );
        let user_key = bytes[PREFIX_BYTES..].to_vec();
        Ok(Self {
            project_id,
            dataset_id,
            table_id,
            user_key,
        })
    }
}

/// Given a key prefix, return the smallest key that is strictly greater
/// than every key starting with that prefix.
///
/// Used to build a half-open range for prefix scans:
/// `engine.scan(prefix..next_after_prefix(&prefix).unwrap_or(...))`.
///
/// Returns `None` if `prefix` is all `0xff` bytes (no greater key
/// exists; the caller should use an unbounded upper end instead).
pub fn next_after_prefix(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    for i in (0..out.len()).rev() {
        if out[i] < 0xff {
            out[i] += 1;
            out.truncate(i + 1);
            return Some(out);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let original = RowKey::new(
            ProjectId::new(),
            DatasetId::new(),
            TableId::new(),
            b"some-user-key".to_vec(),
        );
        let encoded = original.encode();
        assert_eq!(encoded.len(), PREFIX_BYTES + b"some-user-key".len());
        let decoded = RowKey::decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn decode_rejects_short_input() {
        let short = vec![0u8; PREFIX_BYTES - 1];
        assert!(RowKey::decode(&short).is_err());
    }

    #[test]
    fn next_after_prefix_increments_last_byte() {
        assert_eq!(next_after_prefix(&[1, 2, 3]), Some(vec![1, 2, 4]));
    }

    #[test]
    fn next_after_prefix_carries_through_trailing_ffs() {
        assert_eq!(next_after_prefix(&[1, 0xff, 0xff]), Some(vec![2]));
    }

    #[test]
    fn next_after_prefix_all_ffs_returns_none() {
        assert!(next_after_prefix(&[0xff, 0xff]).is_none());
    }

    #[test]
    fn next_after_prefix_empty_returns_none() {
        assert!(next_after_prefix(&[]).is_none());
    }
}
