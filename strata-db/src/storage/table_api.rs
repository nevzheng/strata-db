//! Row-level table API.
//!
//! The middle layer between the byte-level [`StorageEngine`] and
//! anything that thinks in rows + schemas:
//!
//! ```text
//! StorageEngine    ← byte kv
//!     ↓
//! Table API (this) ← typed row CRUD on a (engine, Table) pair
//!     ↓
//! Catalog          ← typed reads/writes on system tables (planned)
//! ```
//!
//! [`TableReader`] and [`TableWriter`] each own the table ids + a
//! cloned `Schema`, so once constructed they don't borrow from the
//! `Table` they were given — `scan` consumes the reader and returns
//! an iterator whose lifetime is tied to the engine alone.

use strata_store::StorageEngine;
use strata_store::memstore::BTreeMapStore;

use crate::catalog::ids::{DatasetId, ProjectId, TableId};
use crate::catalog::schema::Schema;
use crate::catalog::tables::Table;
use crate::query::QueryError;
use crate::storage::row::{RowKey, next_after_prefix};
use crate::storage::types::{Tuple, Value};

/// Read handle over a single table.
pub struct TableReader<'engine> {
    engine: &'engine StorageEngine<BTreeMapStore>,
    project_id: ProjectId,
    dataset_id: DatasetId,
    table_id: TableId,
    schema: Schema,
}

impl<'engine> TableReader<'engine> {
    pub fn new(engine: &'engine StorageEngine<BTreeMapStore>, table: &Table) -> Self {
        Self {
            engine,
            project_id: table.project_id(),
            dataset_id: table.dataset_id(),
            table_id: table.id(),
            schema: table.schema().clone(),
        }
    }

    pub fn get(&self, key: &Value) -> Result<Option<Tuple>, QueryError> {
        let row_key = self.row_key(&encode_key(key)?);
        match self.engine.get(&row_key)? {
            None => Ok(None),
            Some(bytes) => Ok(Some(self.schema.decode(&bytes)?)),
        }
    }

    /// Consumes the reader and returns a streaming row iterator. The
    /// reader's owned `Schema` moves into the closure; the only borrow
    /// left in the returned iterator is `&engine`.
    pub fn scan(self) -> impl Iterator<Item = Result<Tuple, QueryError>> + 'engine {
        let Self {
            engine,
            project_id,
            dataset_id,
            table_id,
            schema,
        } = self;
        let prefix = RowKey::table_prefix(project_id, dataset_id, table_id);
        let raw = match next_after_prefix(&prefix) {
            Some(end) => engine.scan(prefix..end),
            None => engine.scan(prefix..),
        };
        raw.map(move |kv| {
            let (_key, value) = kv?;
            Ok(schema.decode(&value)?)
        })
    }

    fn row_key(&self, user_key: &[u8]) -> Vec<u8> {
        RowKey::new(
            self.project_id,
            self.dataset_id,
            self.table_id,
            user_key.to_vec(),
        )
        .encode()
    }
}

/// Write handle over a single table.
pub struct TableWriter<'engine> {
    engine: &'engine mut StorageEngine<BTreeMapStore>,
    project_id: ProjectId,
    dataset_id: DatasetId,
    table_id: TableId,
    schema: Schema,
}

impl<'engine> TableWriter<'engine> {
    pub fn new(engine: &'engine mut StorageEngine<BTreeMapStore>, table: &Table) -> Self {
        Self {
            engine,
            project_id: table.project_id(),
            dataset_id: table.dataset_id(),
            table_id: table.id(),
            schema: table.schema().clone(),
        }
    }

    /// Insert `tuple`. The storage key is derived from the tuple's
    /// first column (PK-by-convention).
    pub fn put(&mut self, tuple: &Tuple) -> Result<(), QueryError> {
        let row_key = self.row_key(&primary_key_bytes(tuple)?);
        let value_bytes = self.schema.encode(tuple);
        self.engine.put(&row_key, &value_bytes)?;
        Ok(())
    }

    pub fn delete(&mut self, key: &Value) -> Result<(), QueryError> {
        let row_key = self.row_key(&encode_key(key)?);
        self.engine.delete(&row_key)?;
        Ok(())
    }

    fn row_key(&self, user_key: &[u8]) -> Vec<u8> {
        RowKey::new(
            self.project_id,
            self.dataset_id,
            self.table_id,
            user_key.to_vec(),
        )
        .encode()
    }
}

fn primary_key_bytes(tuple: &Tuple) -> Result<Vec<u8>, QueryError> {
    let pk = tuple
        .values
        .first()
        .ok_or_else(|| QueryError::Internal("tuple has no columns to use as primary key".into()))?;
    encode_key(pk)
}

fn encode_key(value: &Value) -> Result<Vec<u8>, QueryError> {
    if matches!(value, Value::Null) {
        return Err(QueryError::Internal("primary key cannot be null".into()));
    }
    let mut buf = Vec::with_capacity(value.encoded_size());
    value.encode(&mut buf);
    Ok(buf)
}
