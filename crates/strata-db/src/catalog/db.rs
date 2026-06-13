use std::cell::{Ref, RefCell, RefMut};
use std::path::Path;

use strata_store::memstore::BTreeMapStore;
use strata_store::{LevelConfig, StorageEngine};

use crate::catalog::consts::{DEFAULT_DATASET_NAME, DEFAULT_PROJECT_NAME};
use crate::catalog::project::Project;
use crate::catalog::{Catalog, CatalogError, ResourceKind};
use crate::query::{QueryContext, QueryError};

/// A freely-copyable handle to the storage engine, borrowed from the [`Db`] that
/// owns it. Every component that touches storage — catalog, projects, datasets —
/// holds one of these and routes through it. The engine is owned **solely** by
/// the `Db`; it is never shared (no `Rc`) or handed out, only borrowed for the
/// duration of an access through this facade.
#[derive(Clone, Copy)]
pub(crate) struct TableApi<'db> {
    engine: &'db RefCell<StorageEngine<BTreeMapStore>>,
}

impl<'db> TableApi<'db> {
    /// Borrow the engine for reading.
    pub(crate) fn read(self) -> Ref<'db, StorageEngine<BTreeMapStore>> {
        self.engine.borrow()
    }

    /// Borrow the engine for writing.
    pub(crate) fn write(self) -> RefMut<'db, StorageEngine<BTreeMapStore>> {
        self.engine.borrow_mut()
    }
}

pub struct Db {
    engine: RefCell<StorageEngine<BTreeMapStore>>,
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
        let db = Db {
            engine: RefCell::new(engine),
        };
        db.ensure_default_namespace()?;
        Ok(db)
    }
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, QueryError> {
        Self::builder().open(path)
    }

    pub fn builder() -> DbBuilder {
        DbBuilder::default()
    }

    /// A handle to the storage facade, borrowing this `Db`.
    fn api(&self) -> TableApi<'_> {
        TableApi {
            engine: &self.engine,
        }
    }

    /// Idempotently seed the default `strata.public` namespace, so a
    /// freshly opened (or reopened) database always has a project and
    /// dataset that SQL can reference. Existing rows are left untouched.
    fn ensure_default_namespace(&self) -> Result<(), QueryError> {
        let catalog = Catalog::new(self.api());
        let project = match catalog.open_project(DEFAULT_PROJECT_NAME)? {
            Some(meta) => meta,
            None => catalog.create_project(DEFAULT_PROJECT_NAME)?,
        };
        if catalog
            .open_dataset(project.id, DEFAULT_DATASET_NAME)?
            .is_none()
        {
            catalog.create_dataset(project.id, DEFAULT_DATASET_NAME)?;
        }
        Ok(())
    }

    pub fn create_project(&self, name: &str) -> Result<Project<'_>, QueryError> {
        let meta = Catalog::new(self.api()).create_project(name)?;
        Ok(Project::new(self.api(), meta.id, meta.name))
    }

    pub fn project(&self, name: &str) -> Result<Project<'_>, QueryError> {
        let meta = Catalog::new(self.api())
            .open_project(name)?
            .ok_or_else(|| CatalogError::NotFound {
                kind: ResourceKind::Project,
                name: name.to_string(),
            })?;
        Ok(Project::new(self.api(), meta.id, meta.name))
    }

    pub fn drop_project(&self, name: &str) -> Result<(), QueryError> {
        Catalog::new(self.api()).drop_project(name)
    }

    pub fn list_projects(&self) -> Result<Vec<String>, QueryError> {
        let metas = Catalog::new(self.api()).list_projects()?;
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
