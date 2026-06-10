use crate::catalog::dataset::Dataset;
use crate::catalog::db::TableApi;
use crate::catalog::ids::ProjectId;
use crate::catalog::{Catalog, CatalogError, ResourceKind};
use crate::query::QueryError;

pub struct Project<'db> {
    api: TableApi<'db>,
    id: ProjectId,
    name: String,
}

impl<'db> Project<'db> {
    pub(crate) fn new(api: TableApi<'db>, id: ProjectId, name: String) -> Self {
        Self { api, id, name }
    }

    pub fn id(&self) -> ProjectId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn create_dataset(&self, name: &str) -> Result<Dataset<'db>, QueryError> {
        let meta = Catalog::new(self.api).create_dataset(self.id, name)?;
        Ok(Dataset::new(self.api, self.id, meta.id, meta.name))
    }

    pub fn dataset(&self, name: &str) -> Result<Dataset<'db>, QueryError> {
        let meta = Catalog::new(self.api)
            .open_dataset(self.id, name)?
            .ok_or_else(|| CatalogError::NotFound {
                kind: ResourceKind::Dataset,
                name: name.to_string(),
            })?;
        Ok(Dataset::new(self.api, self.id, meta.id, meta.name))
    }

    pub fn drop_dataset(&self, name: &str) -> Result<(), QueryError> {
        Catalog::new(self.api).drop_dataset(self.id, name)
    }

    pub fn list_datasets(&self) -> Result<Vec<String>, QueryError> {
        let metas = Catalog::new(self.api).list_datasets(self.id)?;
        Ok(metas.into_iter().map(|m| m.name).collect())
    }
}
