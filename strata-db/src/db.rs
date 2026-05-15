use std::path::Path;
use std::sync::{Arc, Mutex};

use strata::StorageEngine;
use strata::memstore::BTreeMapStore;

use crate::catalog::{Catalog, CatalogError, ResourceKind};
use crate::project::Project;

pub(crate) type SharedEngine = Arc<Mutex<StorageEngine<BTreeMapStore>>>;

pub struct Db {
    engine: SharedEngine,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, CatalogError> {
        let engine = StorageEngine::new(path, BTreeMapStore::new())
            .map_err(|e| CatalogError::InternalError(e.to_string()))?;
        Ok(Self {
            engine: Arc::new(Mutex::new(engine)),
        })
    }

    pub fn create_project(&self, name: &str) -> Result<Project, CatalogError> {
        let meta = Catalog::new(self.engine.clone()).create_project(name)?;
        Ok(Project::new(self.engine.clone(), meta.id, meta.name))
    }

    pub fn project(&self, name: &str) -> Result<Project, CatalogError> {
        let meta = Catalog::new(self.engine.clone())
            .open_project(name)?
            .ok_or_else(|| CatalogError::NotFound {
                kind: ResourceKind::Project,
                name: name.to_string(),
            })?;
        Ok(Project::new(self.engine.clone(), meta.id, meta.name))
    }

    pub fn drop_project(&self, name: &str) -> Result<(), CatalogError> {
        Catalog::new(self.engine.clone()).drop_project(name)
    }

    pub fn list_projects(&self) -> Result<Vec<String>, CatalogError> {
        let metas = Catalog::new(self.engine.clone()).list_projects()?;
        Ok(metas.into_iter().map(|m| m.name).collect())
    }
}
