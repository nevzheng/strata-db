use crate::catalog::db::TableApi;
use crate::catalog::ids::{DatasetId, ProjectId};
use crate::catalog::schema::Schema;
use crate::catalog::tables::Table;
use crate::catalog::{Catalog, CatalogError, ResourceKind};
use crate::query::QueryError;

pub struct Dataset<'db> {
    api: TableApi<'db>,
    project_id: ProjectId,
    id: DatasetId,
    name: String,
}

impl<'db> Dataset<'db> {
    pub(crate) fn new(
        api: TableApi<'db>,
        project_id: ProjectId,
        id: DatasetId,
        name: String,
    ) -> Self {
        Self {
            api,
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

    pub fn create_table(&self, name: &str, schema: Schema) -> Result<Table, QueryError> {
        let meta = Catalog::new(self.api)
            .project(self.project_id)
            .dataset(self.id)
            .create_table(name, schema)?;
        Ok(Table::new(
            self.project_id,
            self.id,
            meta.id,
            meta.name,
            meta.schema,
        ))
    }

    pub fn table(&self, name: &str) -> Result<Table, QueryError> {
        let meta = Catalog::new(self.api)
            .project(self.project_id)
            .dataset(self.id)
            .open_table(name)?
            .ok_or_else(|| CatalogError::NotFound {
                kind: ResourceKind::Table,
                name: name.to_string(),
            })?;
        Ok(Table::new(
            self.project_id,
            self.id,
            meta.id,
            meta.name,
            meta.schema,
        ))
    }

    pub fn drop_table(&self, name: &str) -> Result<(), QueryError> {
        Catalog::new(self.api)
            .project(self.project_id)
            .dataset(self.id)
            .drop_table(name)
    }

    pub fn list_tables(&self) -> Result<Vec<String>, QueryError> {
        let metas = Catalog::new(self.api)
            .project(self.project_id)
            .dataset(self.id)
            .list_tables()?;
        Ok(metas.into_iter().map(|m| m.name).collect())
    }
}
