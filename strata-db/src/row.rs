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

use crate::ids::{DatasetId, ProjectId, TableId};

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
    pub fn encode(&self) -> Vec<u8> {
        todo!()
    }

    /// Decode a row key from engine bytes.
    pub fn decode(_bytes: &[u8]) -> Result<Self, EncodingError> {
        todo!()
    }
}
