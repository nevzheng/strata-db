use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use strata_store::memstore::BTreeMapStore;
use strata_store::{LevelConfig, StorageEngine};

use crate::catalog::project::Project;
use crate::catalog::{Catalog, CatalogError, ResourceKind};
use crate::query::{QueryContext, QueryError};

/// Shared handle to the storage engine. Single-threaded (`Rc`/`RefCell`): the
/// engine is `!Send` (the pager is `Rc`-based), so a `Db` and everything it
/// shares the engine with live on one thread. The server pins the `Db` to a
/// dedicated engine thread rather than sharing it across the async runtime.
pub(crate) type SharedEngine = Rc<RefCell<StorageEngine<BTreeMapStore>>>;

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
            engine: Rc::new(RefCell::new(engine)),
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

    /// Open a per-query context, borrowing the storage engine for the returned
    /// context's lifetime. Drop the context to release.
    pub fn query_context(&self) -> QueryContext<'_> {
        QueryContext {
            engine: self.engine.borrow_mut(),
        }
    }
}
