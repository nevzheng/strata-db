//! Per-query execution context.
//!
//! A [`QueryContext`] holds the storage-engine lock for the lifetime of
//! a query. Every read and write a query performs — scans, point
//! lookups, inserts, deletes — flows through this single handle, which
//! is what makes streaming end-to-end possible: cursors returned by
//! the engine borrow from the guard inside the context, so they're
//! valid for as long as the context is.

use std::sync::MutexGuard;

use strata::StorageEngine;
use strata::memstore::BTreeMapStore;

use crate::catalog::tables::Table;
use crate::storage::row::{RowKey, next_after_prefix};
use crate::storage::types::{Tuple, Value};

use super::QueryError;

/// The engine state visible during one query, with the storage lock
/// held. Opened via [`crate::Db::query_context`]; release the lock by
/// dropping it.
pub struct QueryContext<'db> {
    pub(crate) engine: MutexGuard<'db, StorageEngine<BTreeMapStore>>,
}

impl QueryContext<'_> {
    /// Insert a tuple into `table`. The storage key is derived from the
    /// tuple's first column — that's the "primary key by convention"
    /// stand-in until [`crate::Schema`] grows an explicit PK field.
    pub fn put(&mut self, table: &Table, tuple: &Tuple) -> Result<(), QueryError> {
        let user_key = primary_key_bytes(tuple)?;
        let row_key = RowKey::new(table.project_id(), table.dataset_id(), table.id(), user_key);
        let value_bytes = table.schema().encode(tuple);
        self.engine.put(&row_key.encode(), &value_bytes)?;
        Ok(())
    }

    /// Look up a row by its primary-key value.
    pub fn get(&self, table: &Table, key: &Value) -> Result<Option<Tuple>, QueryError> {
        let user_key = encode_key(key)?;
        let row_key = RowKey::new(table.project_id(), table.dataset_id(), table.id(), user_key);
        let raw = self.engine.get(&row_key.encode())?;
        match raw {
            None => Ok(None),
            Some(bytes) => Ok(Some(table.schema().decode(&bytes)?)),
        }
    }

    /// Delete the row identified by its primary-key value.
    pub fn delete(&mut self, table: &Table, key: &Value) -> Result<(), QueryError> {
        let user_key = encode_key(key)?;
        let row_key = RowKey::new(table.project_id(), table.dataset_id(), table.id(), user_key);
        self.engine.delete(&row_key.encode())?;
        Ok(())
    }

    /// Stream every row of `table` as decoded [`Tuple`]s. The returned
    /// iterator borrows from this context — it is valid as long as the
    /// context is.
    ///
    /// The schema is cloned into the iterator at call time so it
    /// doesn't borrow from `table`; callers can drop the table handle
    /// after constructing the iterator.
    pub fn scan<'a>(
        &'a self,
        table: &Table,
    ) -> impl Iterator<Item = Result<Tuple, QueryError>> + 'a {
        let schema = table.schema().clone();
        let prefix = RowKey::table_prefix(table.project_id(), table.dataset_id(), table.id());

        let raw = match next_after_prefix(&prefix) {
            Some(end) => self.engine.scan(prefix..end),
            None => self.engine.scan(prefix..),
        };

        raw.map(move |kv| {
            let (_key, value) = kv?;
            Ok(schema.decode(&value)?)
        })
    }
}

/// Encode the tuple's primary-key column (column 0 by convention) as
/// storage-key bytes. Errors if the tuple has no columns or its PK
/// value is `Null` — neither is a valid storage key.
fn primary_key_bytes(tuple: &Tuple) -> Result<Vec<u8>, QueryError> {
    let pk = tuple
        .values
        .first()
        .ok_or_else(|| QueryError::Internal("tuple has no columns to use as primary key".into()))?;
    encode_key(pk)
}

/// Encode a single value as storage-key bytes via the same codec the
/// schema uses for cells. Null is rejected.
fn encode_key(value: &Value) -> Result<Vec<u8>, QueryError> {
    if matches!(value, Value::Null) {
        return Err(QueryError::Internal("primary key cannot be null".into()));
    }
    let mut buf = Vec::with_capacity(value.encoded_size());
    value.encode(&mut buf);
    Ok(buf)
}
