use crate::catalog::CatalogError;
use crate::db::SharedEngine;
use crate::ids::{DatasetId, ProjectId, TableId};
use crate::schema::Schema;

pub struct Table {
    // Used once put/get/delete bodies are filled in.
    #[allow(dead_code)]
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

    pub fn put(&self, _key: &[u8], _value: serde_json::Value) -> Result<(), CatalogError> {
        todo!()
    }

    pub fn get(&self, _key: &[u8]) -> Result<Option<serde_json::Value>, CatalogError> {
        todo!()
    }

    pub fn delete(&self, _key: &[u8]) -> Result<(), CatalogError> {
        todo!()
    }
}
