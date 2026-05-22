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

use std::ops::{Bound, RangeBounds};

use strata_store::StorageEngine;
use strata_store::memstore::BTreeMapStore;

use crate::catalog::ids::{DatasetId, ProjectId, TableId};
use crate::catalog::schema::Schema;
use crate::catalog::tables::Table;
use crate::query::QueryError;
use crate::storage::row::RowKey;
use crate::storage::types::{Tuple, Value};

pub type Predicate = Box<dyn Fn(&Tuple) -> bool>;

pub struct ScanOptions {
    pub range: (Bound<Vec<u8>>, Bound<Vec<u8>>),
    pub predicate: Option<Predicate>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            range: (Bound::Unbounded, Bound::Unbounded),
            predicate: None,
        }
    }
}

impl ScanOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn range<R: RangeBounds<Vec<u8>>>(mut self, range: R) -> Self {
        self.range = (range.start_bound().cloned(), range.end_bound().cloned());
        self
    }

    /// Restrict the scan to keys starting with `prefix`. Empty prefix
    /// is a no-op (full scan).
    pub fn prefix(self, prefix: &[u8]) -> Self {
        if prefix.is_empty() {
            return self;
        }
        let start = Bound::Included(prefix.to_vec());
        let end = match next_after_prefix(prefix) {
            Some(k) => Bound::Excluded(k),
            None => Bound::Unbounded,
        };
        self.range((start, end))
    }

    pub fn predicate(mut self, pred: Predicate) -> Self {
        self.predicate = Some(pred);
        self
    }
}

/// Smallest byte string strictly greater than every key starting with
/// `prefix`. `None` if `prefix` is all `0xff` (no successor).
fn next_after_prefix(prefix: &[u8]) -> Option<Vec<u8>> {
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
    pub fn scan(
        self,
        options: ScanOptions,
    ) -> impl Iterator<Item = Result<Tuple, QueryError>> + 'engine {
        let Self {
            engine,
            project_id,
            dataset_id,
            table_id,
            schema,
        } = self;
        let table_prefix = RowKey::table_prefix(project_id, dataset_id, table_id);

        // Prepend the table prefix to each user-key bound, defaulting
        // unbounded sides to the table boundary so the scan stays
        // inside this table.
        let with_prefix = |user_key: &[u8]| {
            let mut k = table_prefix.clone();
            k.extend_from_slice(user_key);
            k
        };
        let (user_start, user_end) = options.range;
        let start = match user_start {
            Bound::Included(k) => Bound::Included(with_prefix(&k)),
            Bound::Excluded(k) => Bound::Excluded(with_prefix(&k)),
            Bound::Unbounded => Bound::Included(table_prefix.clone()),
        };
        let end = match user_end {
            Bound::Included(k) => Bound::Included(with_prefix(&k)),
            Bound::Excluded(k) => Bound::Excluded(with_prefix(&k)),
            Bound::Unbounded => match next_after_prefix(&table_prefix) {
                Some(k) => Bound::Excluded(k),
                None => Bound::Unbounded,
            },
        };

        engine.scan((start, end)).map(move |kv| {
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

#[cfg(test)]
mod tests {
    use super::*;

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
