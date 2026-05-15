use crate::catalog::CatalogError;
use crate::db::SharedEngine;
use crate::ids::{DatasetId, ProjectId, TableId};
use crate::row::RowKey;
use crate::schema::Schema;

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

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn put(&self, key: &[u8], value: serde_json::Value) -> Result<(), CatalogError> {
        let row_key = self.row_key(key);
        let value_bytes =
            serde_json::to_vec(&value).map_err(|e| CatalogError::InternalError(e.to_string()))?;
        self.engine
            .lock()
            .unwrap()
            .put(&row_key.encode(), &value_bytes)
            .map_err(|e| CatalogError::InternalError(e.to_string()))?;
        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<serde_json::Value>, CatalogError> {
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
                let value = serde_json::from_slice(&bytes)
                    .map_err(|e| CatalogError::InternalError(e.to_string()))?;
                Ok(Some(value))
            }
        }
    }

    pub fn delete(&self, key: &[u8]) -> Result<(), CatalogError> {
        let row_key = self.row_key(key);
        self.engine
            .lock()
            .unwrap()
            .delete(&row_key.encode())
            .map_err(|e| CatalogError::InternalError(e.to_string()))?;
        Ok(())
    }

    fn row_key(&self, user_key: &[u8]) -> RowKey {
        RowKey::new(self.project_id, self.dataset_id, self.id, user_key.to_vec())
    }
}
