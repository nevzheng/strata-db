use crate::catalog::CatalogError;
use crate::db::SharedEngine;
use crate::ids::{DatasetId, ProjectId, TableId};
use crate::row::{RowKey, next_after_prefix};
use crate::schema::Schema;
use crate::types::Tuple;

/// Typed key-value store contract.
///
/// Anything implementing `TypedStore` can be the storage substrate for
/// catalog code or a future SQL executor: bytes-keyed addressing, but
/// `Tuple`s (not bytes) as values. Encoding and decoding are the
/// implementor's responsibility.
///
/// Concrete impl today: [`Table`]. Future impls might be in-memory
/// mocks for tests, remote-table proxies, materialized views over base
/// tables, or anything else with a schema and a put/get surface.
pub trait TypedStore {
    fn schema(&self) -> &Schema;
    fn put(&self, key: &[u8], tuple: &Tuple) -> Result<(), CatalogError>;
    fn get(&self, key: &[u8]) -> Result<Option<Tuple>, CatalogError>;
    fn delete(&self, key: &[u8]) -> Result<(), CatalogError>;
    fn scan(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Tuple)>, CatalogError>;
}

pub struct Table {
    engine: SharedEngine,
    project_id: ProjectId,
    dataset_id: DatasetId,
    id: TableId,
    name: String,
    schema: Schema,
}

impl Table {
    pub(crate) fn new(
        engine: SharedEngine,
        project_id: ProjectId,
        dataset_id: DatasetId,
        id: TableId,
        name: String,
        schema: Schema,
    ) -> Self {
        Self {
            engine,
            project_id,
            dataset_id,
            id,
            name,
            schema,
        }
    }

    pub fn id(&self) -> TableId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    pub fn dataset_id(&self) -> DatasetId {
        self.dataset_id
    }

    fn row_key(&self, user_key: &[u8]) -> RowKey {
        RowKey::new(self.project_id, self.dataset_id, self.id, user_key.to_vec())
    }
}

impl TypedStore for Table {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn put(&self, key: &[u8], tuple: &Tuple) -> Result<(), CatalogError> {
        let row_key = self.row_key(key);
        let value_bytes = self.schema.encode(tuple);
        self.engine
            .lock()
            .unwrap()
            .put(&row_key.encode(), &value_bytes)
            .map_err(|e| CatalogError::InternalError(e.to_string()))?;
        Ok(())
    }

    fn get(&self, key: &[u8]) -> Result<Option<Tuple>, CatalogError> {
        let row_key = self.row_key(key);
        let raw = self
            .engine
            .lock()
            .unwrap()
            .get(&row_key.encode())
            .map_err(|e| CatalogError::InternalError(e.to_string()))?;
        match raw {
            None => Ok(None),
            Some(bytes) => {
                let tuple = self
                    .schema
                    .decode(&bytes)
                    .map_err(|e| CatalogError::InternalError(format!("decode: {e:?}")))?;
                Ok(Some(tuple))
            }
        }
    }

    fn delete(&self, key: &[u8]) -> Result<(), CatalogError> {
        let row_key = self.row_key(key);
        self.engine
            .lock()
            .unwrap()
            .delete(&row_key.encode())
            .map_err(|e| CatalogError::InternalError(e.to_string()))?;
        Ok(())
    }

    fn scan(&self, user_key_prefix: &[u8]) -> Result<Vec<(Vec<u8>, Tuple)>, CatalogError> {
        let mut prefix = RowKey::table_prefix(self.project_id, self.dataset_id, self.id);
        let table_prefix_len = prefix.len();
        prefix.extend_from_slice(user_key_prefix);

        let entries = match next_after_prefix(&prefix) {
            Some(end) => self.engine.lock().unwrap().scan(prefix..end),
            None => self.engine.lock().unwrap().scan(prefix..),
        }
        .map_err(|e| CatalogError::InternalError(e.to_string()))?;

        entries
            .into_iter()
            .map(|(k, v)| {
                let user_key = k[table_prefix_len..].to_vec();
                let tuple = self
                    .schema
                    .decode(&v)
                    .map_err(|e| CatalogError::InternalError(format!("decode: {e:?}")))?;
                Ok((user_key, tuple))
            })
            .collect()
    }
}
