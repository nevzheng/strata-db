use crate::catalog::{Catalog, CatalogError, ResourceKind};
use crate::db::SharedEngine;
use crate::ids::{DatasetId, ProjectId};
use crate::schema::Schema;
use crate::tables::Table;

pub struct Dataset {
    engine: SharedEngine,
    project_id: ProjectId,
    id: DatasetId,
    name: String,
}

impl Dataset {
    pub(crate) fn new(
        engine: SharedEngine,
        project_id: ProjectId,
        id: DatasetId,
        name: String,
    ) -> Self {
        Self {
            engine,
            project_id,
            id,
            name,
        }
    }

    pub fn id(&self) -> DatasetId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    pub fn create_table(&self, name: &str, schema: Schema) -> Result<Table, CatalogError> {
        let meta = Catalog::new(self.engine.clone())
            .project(self.project_id)
            .dataset(self.id)
            .create_table(name, schema)?;
        Ok(Table::new(
            self.engine.clone(),
            self.project_id,
            self.id,
            meta.id,
            meta.name,
            meta.schema,
        ))
    }

    pub fn table(&self, name: &str) -> Result<Table, CatalogError> {
        let meta = Catalog::new(self.engine.clone())
            .project(self.project_id)
            .dataset(self.id)
            .open_table(name)?
            .ok_or_else(|| CatalogError::NotFound {
                kind: ResourceKind::Table,
                name: name.to_string(),
            })?;
        Ok(Table::new(
            self.engine.clone(),
            self.project_id,
            self.id,
            meta.id,
            meta.name,
            meta.schema,
        ))
    }

    pub fn drop_table(&self, name: &str) -> Result<(), CatalogError> {
        Catalog::new(self.engine.clone())
            .project(self.project_id)
            .dataset(self.id)
            .drop_table(name)
    }

    pub fn list_tables(&self) -> Result<Vec<String>, CatalogError> {
        let metas = Catalog::new(self.engine.clone())
            .project(self.project_id)
            .dataset(self.id)
            .list_tables()?;
        Ok(metas.into_iter().map(|m| m.name).collect())
    }
}
