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
//! In the current encoding the namespace prefix is 56 bytes
//! (three 16-byte UUIDs plus an 8-byte truncation id) plus the user
//! key. This is intentionally "heavy" — we trade compactness for
//! stable, content-addressable IDs that don't require a central
//! allocator. Block-level prefix compression and block compression
//! (future SSTable work) will amortize the on-disk cost.
//!
//! Every row in one table *incarnation* shares the same 56-byte prefix;
//! the table API ([`crate::storage::table_api`]) builds range scans on
//! top of [`RowKey::table_prefix`].

use uuid::Uuid;

use crate::catalog::ids::{DatasetId, ProjectId, TableId, TruncationId};

const ID_BYTES: usize = 16;
const TRUNC_BYTES: usize = 8;
/// `project_id | dataset_id | table_id | truncation_id` — the fixed
/// segment every row in one table incarnation shares.
const PREFIX_BYTES: usize = ID_BYTES * 3 + TRUNC_BYTES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowKey {
    pub project_id: ProjectId,
    pub dataset_id: DatasetId,
    pub table_id: TableId,
    /// Which incarnation of the table this row belongs to. `CREATE OR
    /// REPLACE` mints a higher one; rows under lower ids are dead but
    /// retained (see the GC note in [`crate::catalog`]).
    pub truncation_id: TruncationId,
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
        truncation_id: TruncationId,
        user_key: Vec<u8>,
    ) -> Self {
        Self {
            project_id,
            dataset_id,
            table_id,
            truncation_id,
            user_key,
        }
    }

    /// Encode this row key into bytes for the underlying engine.
    ///
    /// Layout: `| project_id (16) | dataset_id (16) | table_id (16) | truncation_id (8) | user_key (variable) |`.
    /// The truncation id is big-endian so a table's incarnations sort in
    /// creation order.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(PREFIX_BYTES + self.user_key.len());
        out.extend_from_slice(self.project_id.as_bytes());
        out.extend_from_slice(self.dataset_id.as_bytes());
        out.extend_from_slice(self.table_id.as_bytes());
        out.extend_from_slice(&self.truncation_id.to_be_bytes());
        out.extend_from_slice(&self.user_key);
        out
    }

    /// Build the key prefix shared by every row in one table
    /// *incarnation*. Used by the table API to bound scans to the live
    /// incarnation of a single table.
    pub fn table_prefix(
        project_id: ProjectId,
        dataset_id: DatasetId,
        table_id: TableId,
        truncation_id: TruncationId,
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(PREFIX_BYTES);
        out.extend_from_slice(project_id.as_bytes());
        out.extend_from_slice(dataset_id.as_bytes());
        out.extend_from_slice(table_id.as_bytes());
        out.extend_from_slice(&truncation_id.to_be_bytes());
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
            Uuid::from_slice(&bytes[ID_BYTES * 2..ID_BYTES * 3])
                .map_err(|e| EncodingError::Invalid(e.to_string()))?,
        );
        let mut trunc = [0u8; TRUNC_BYTES];
        trunc.copy_from_slice(&bytes[ID_BYTES * 3..PREFIX_BYTES]);
        let truncation_id = TruncationId::from_be_bytes(trunc);
        let user_key = bytes[PREFIX_BYTES..].to_vec();
        Ok(Self {
            project_id,
            dataset_id,
            table_id,
            truncation_id,
            user_key,
        })
    }
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
            TruncationId(3),
            b"some-user-key".to_vec(),
        );
        let encoded = original.encode();
        assert_eq!(encoded.len(), PREFIX_BYTES + b"some-user-key".len());
        let decoded = RowKey::decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn truncation_id_sorts_incarnations_in_creation_order() {
        let (p, d, t) = (ProjectId::new(), DatasetId::new(), TableId::new());
        let older = RowKey::new(p, d, t, TruncationId(0), b"k".to_vec()).encode();
        let newer = RowKey::new(p, d, t, TruncationId(1), b"k".to_vec()).encode();
        assert!(older < newer, "higher truncation ids must sort after lower");
    }

    #[test]
    fn decode_rejects_short_input() {
        let short = vec![0u8; PREFIX_BYTES - 1];
        assert!(RowKey::decode(&short).is_err());
    }
}
