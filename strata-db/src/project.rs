use crate::catalog::{Catalog, CatalogError, ResourceKind};
use crate::dataset::Dataset;
use crate::db::SharedEngine;
use crate::ids::ProjectId;

pub struct Project {
    engine: SharedEngine,
    id: ProjectId,
    name: String,
}

impl Project {
    pub(crate) fn new(engine: SharedEngine, id: ProjectId, name: String) -> Self {
        Self { engine, id, name }
    }

    pub fn id(&self) -> ProjectId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn create_dataset(&self, name: &str) -> Result<Dataset, CatalogError> {
        let meta = Catalog::new(self.engine.clone())
            .project(self.id)
            .create_dataset(name)?;
        Ok(Dataset::new(
            self.engine.clone(),
            self.id,
            meta.id,
            meta.name,
        ))
    }

    pub fn dataset(&self, name: &str) -> Result<Dataset, CatalogError> {
        let meta = Catalog::new(self.engine.clone())
            .project(self.id)
            .open_dataset(name)?
            .ok_or_else(|| CatalogError::NotFound {
                kind: ResourceKind::Dataset,
                name: name.to_string(),
            })?;
        Ok(Dataset::new(
            self.engine.clone(),
            self.id,
            meta.id,
            meta.name,
        ))
    }

    pub fn drop_dataset(&self, name: &str) -> Result<(), CatalogError> {
        Catalog::new(self.engine.clone())
            .project(self.id)
            .drop_dataset(name)
    }

    pub fn list_datasets(&self) -> Result<Vec<String>, CatalogError> {
        let metas = Catalog::new(self.engine.clone())
            .project(self.id)
            .list_datasets()?;
        Ok(metas.into_iter().map(|m| m.name).collect())
    }
}
