use std::path::Path;
use std::sync::{Arc, Mutex};

use strata_store::memstore::BTreeMapStore;
use strata_store::{LevelConfig, StorageEngine};

use crate::catalog::project::Project;
use crate::catalog::{Catalog, CatalogError, ResourceKind};
use crate::query::{QueryContext, QueryError};

pub(crate) type SharedEngine = Arc<Mutex<StorageEngine<BTreeMapStore>>>;

pub struct Db {
    engine: SharedEngine,
}

/// Fluent configuration for opening a [`Db`].
///
/// Defaults match [`Db::open`]: the engine's built-in 7-level config and a
/// 4 MB memtable. Override either when you need to exercise compaction
/// (e.g. shrink the memtable so writes flush into L0 quickly).
#[derive(Default)]
pub struct DbBuilder {
    mem_capacity: Option<usize>,
    levels: Option<Vec<LevelConfig>>,
}

impl DbBuilder {
    /// Override the memtable capacity in bytes.
    pub fn mem_capacity(mut self, capacity: usize) -> Self {
        self.mem_capacity = Some(capacity);
        self
    }

    /// Override the per-level config used when constructing the engine.
    pub fn levels(mut self, configs: Vec<LevelConfig>) -> Self {
        self.levels = Some(configs);
        self
    }

    pub fn open(self, path: &Path) -> Result<Db, QueryError> {
        let mem = match self.mem_capacity {
            Some(c) => BTreeMapStore::with_capacity(c),
            None => BTreeMapStore::new(),
        };
        let engine = match self.levels {
            Some(configs) => StorageEngine::with_levels(path, mem, configs),
            None => StorageEngine::new(path, mem),
        }?;
        Ok(Db {
            engine: Arc::new(Mutex::new(engine)),
        })
    }
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, QueryError> {
        Self::builder().open(path)
    }

    pub fn builder() -> DbBuilder {
        DbBuilder::default()
    }

    pub fn create_project(&self, name: &str) -> Result<Project, QueryError> {
        let meta = Catalog::new(self.engine.clone()).create_project(name)?;
        Ok(Project::new(self.engine.clone(), meta.id, meta.name))
    }

    pub fn project(&self, name: &str) -> Result<Project, QueryError> {
        let meta = Catalog::new(self.engine.clone())
            .open_project(name)?
            .ok_or_else(|| CatalogError::NotFound {
                kind: ResourceKind::Project,
                name: name.to_string(),
            })?;
        Ok(Project::new(self.engine.clone(), meta.id, meta.name))
    }

    pub fn drop_project(&self, name: &str) -> Result<(), QueryError> {
        Catalog::new(self.engine.clone()).drop_project(name)
    }

    pub fn list_projects(&self) -> Result<Vec<String>, QueryError> {
        let metas = Catalog::new(self.engine.clone()).list_projects()?;
        Ok(metas.into_iter().map(|m| m.name).collect())
    }

    /// Open a per-query context, acquiring the storage-engine lock for
    /// the returned context's lifetime. Drop the context to release.
    pub fn query_context(&self) -> QueryContext<'_> {
        QueryContext {
            engine: self.engine.lock().unwrap(),
        }
    }
}
