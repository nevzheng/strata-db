//! Catalog: project/dataset/table metadata stored in the system namespace.
//!
//! All catalog reads and writes go through the engine as normal rows in
//! reserved system tables (`_system._catalog.{_projects,_datasets,_tables}`).
//! Bootstrap relies on the constants in [`crate::consts`].
//!
//! ## Known gaps (intentional, scaffolded)
//!
//! - **No cascade on drop.** Dropping a project leaves its dataset and
//!   table metadata rows in the engine, unreachable through the catalog
//!   API. A future pass will list children and drop them transactively.
//!   `list_*` is now implemented, so cascade is a small follow-up.
//! - **Existence checks are non-atomic.** `create_*` and `drop_*`
//!   read-then-write; concurrent callers could race. Fine for the
//!   current single-threaded use; engine-level CAS would close the gap.

use crate::consts::{
    CATALOG_DATASET_ID, DATASETS_TABLE_ID, PROJECTS_TABLE_ID, SYSTEM_PROJECT_ID, TABLES_TABLE_ID,
};
use crate::db::SharedEngine;
use crate::ids::{DatasetId, ProjectId, TableId};
use crate::row::{RowKey, next_after_prefix};
use crate::schema::Schema;

#[derive(Debug, Clone, Copy)]
pub enum ResourceKind {
    Project,
    Dataset,
    Table,
}

#[derive(Debug)]
pub enum CatalogError {
    NotFound { kind: ResourceKind, name: String },
    AlreadyExists { kind: ResourceKind, name: String },
    InternalError(String),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ProjectMeta {
    pub id: ProjectId,
    pub name: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DatasetMeta {
    pub id: DatasetId,
    pub name: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct TableMeta {
    pub id: TableId,
    pub name: String,
    pub schema: Schema,
}

// --- System-table helpers ---
//
// Every catalog op against a system table goes through one of these. They
// centralize RowKey construction, JSON serialization, and the engine call
// so the public methods stay small.

fn put_meta<T: serde::Serialize>(
    engine: &SharedEngine,
    system_table: TableId,
    user_key: Vec<u8>,
    meta: &T,
) -> Result<(), CatalogError> {
    let row_key = RowKey::new(
        SYSTEM_PROJECT_ID,
        CATALOG_DATASET_ID,
        system_table,
        user_key,
    );
    let value = serde_json::to_vec(meta).map_err(|e| CatalogError::InternalError(e.to_string()))?;
    engine
        .lock()
        .unwrap()
        .put(&row_key.encode(), &value)
        .map_err(|e| CatalogError::InternalError(e.to_string()))?;
    Ok(())
}

fn get_meta<T: serde::de::DeserializeOwned>(
    engine: &SharedEngine,
    system_table: TableId,
    user_key: Vec<u8>,
) -> Result<Option<T>, CatalogError> {
    let row_key = RowKey::new(
        SYSTEM_PROJECT_ID,
        CATALOG_DATASET_ID,
        system_table,
        user_key,
    );
    let raw = engine
        .lock()
        .unwrap()
        .get(&row_key.encode())
        .map_err(|e| CatalogError::InternalError(e.to_string()))?;
    match raw {
        None => Ok(None),
        Some(bytes) => {
            let meta = serde_json::from_slice(&bytes)
                .map_err(|e| CatalogError::InternalError(e.to_string()))?;
            Ok(Some(meta))
        }
    }
}

fn delete_meta(
    engine: &SharedEngine,
    system_table: TableId,
    user_key: Vec<u8>,
) -> Result<(), CatalogError> {
    let row_key = RowKey::new(
        SYSTEM_PROJECT_ID,
        CATALOG_DATASET_ID,
        system_table,
        user_key,
    );
    engine
        .lock()
        .unwrap()
        .delete(&row_key.encode())
        .map_err(|e| CatalogError::InternalError(e.to_string()))?;
    Ok(())
}

/// Scan every row whose key starts with the given user-key prefix in a
/// system table, decoding values as `T`.
///
/// NOTE: scan currently returns a fully-materialized Vec. Fine for the
/// catalog (small, bounded). User-table scans will want an iterator once
/// `strata::StorageEngine` supports streaming results.
fn list_metas<T: serde::de::DeserializeOwned>(
    engine: &SharedEngine,
    system_table: TableId,
    user_key_prefix: &[u8],
) -> Result<Vec<T>, CatalogError> {
    let mut prefix = RowKey::table_prefix(SYSTEM_PROJECT_ID, CATALOG_DATASET_ID, system_table);
    prefix.extend_from_slice(user_key_prefix);
    let entries = match next_after_prefix(&prefix) {
        Some(end) => engine.lock().unwrap().scan(prefix..end),
        None => engine.lock().unwrap().scan(prefix..),
    }
    .map_err(|e| CatalogError::InternalError(e.to_string()))?;

    entries
        .into_iter()
        .map(|(_, v)| {
            serde_json::from_slice::<T>(&v).map_err(|e| CatalogError::InternalError(e.to_string()))
        })
        .collect()
}

// --- Catalog (top-level) ---

pub(crate) struct Catalog {
    engine: SharedEngine,
}

impl Catalog {
    pub(crate) fn new(engine: SharedEngine) -> Self {
        Self { engine }
    }

    pub(crate) fn create_project(&self, name: &str) -> Result<ProjectMeta, CatalogError> {
        if self.open_project(name)?.is_some() {
            return Err(CatalogError::AlreadyExists {
                kind: ResourceKind::Project,
                name: name.to_string(),
            });
        }
        let meta = ProjectMeta {
            id: ProjectId::new(),
            name: name.to_string(),
        };
        put_meta(
            &self.engine,
            PROJECTS_TABLE_ID,
            name.as_bytes().to_vec(),
            &meta,
        )?;
        Ok(meta)
    }

    pub(crate) fn open_project(&self, name: &str) -> Result<Option<ProjectMeta>, CatalogError> {
        get_meta(&self.engine, PROJECTS_TABLE_ID, name.as_bytes().to_vec())
    }

    pub(crate) fn drop_project(&self, name: &str) -> Result<(), CatalogError> {
        if self.open_project(name)?.is_none() {
            return Err(CatalogError::NotFound {
                kind: ResourceKind::Project,
                name: name.to_string(),
            });
        }
        delete_meta(&self.engine, PROJECTS_TABLE_ID, name.as_bytes().to_vec())
    }

    pub(crate) fn list_projects(&self) -> Result<Vec<ProjectMeta>, CatalogError> {
        list_metas(&self.engine, PROJECTS_TABLE_ID, &[])
    }

    /// Narrow the catalog to a single project's scope for dataset operations.
    pub(crate) fn project(&self, project_id: ProjectId) -> CatalogProject {
        CatalogProject {
            engine: self.engine.clone(),
            project_id,
        }
    }
}

// --- CatalogProject (scoped to one project) ---

pub(crate) struct CatalogProject {
    engine: SharedEngine,
    project_id: ProjectId,
}

impl CatalogProject {
    fn user_key(&self, name: &str) -> Vec<u8> {
        let mut k = self.project_id.as_bytes().to_vec();
        k.extend_from_slice(name.as_bytes());
        k
    }

    pub(crate) fn create_dataset(&self, name: &str) -> Result<DatasetMeta, CatalogError> {
        if self.open_dataset(name)?.is_some() {
            return Err(CatalogError::AlreadyExists {
                kind: ResourceKind::Dataset,
                name: name.to_string(),
            });
        }
        let meta = DatasetMeta {
            id: DatasetId::new(),
            name: name.to_string(),
        };
        put_meta(&self.engine, DATASETS_TABLE_ID, self.user_key(name), &meta)?;
        Ok(meta)
    }

    pub(crate) fn open_dataset(&self, name: &str) -> Result<Option<DatasetMeta>, CatalogError> {
        get_meta(&self.engine, DATASETS_TABLE_ID, self.user_key(name))
    }

    pub(crate) fn drop_dataset(&self, name: &str) -> Result<(), CatalogError> {
        if self.open_dataset(name)?.is_none() {
            return Err(CatalogError::NotFound {
                kind: ResourceKind::Dataset,
                name: name.to_string(),
            });
        }
        delete_meta(&self.engine, DATASETS_TABLE_ID, self.user_key(name))
    }

    pub(crate) fn list_datasets(&self) -> Result<Vec<DatasetMeta>, CatalogError> {
        list_metas(&self.engine, DATASETS_TABLE_ID, self.project_id.as_bytes())
    }

    /// Narrow further to a single dataset's scope for table operations.
    pub(crate) fn dataset(&self, dataset_id: DatasetId) -> CatalogDataset {
        CatalogDataset {
            engine: self.engine.clone(),
            project_id: self.project_id,
            dataset_id,
        }
    }
}

// --- CatalogDataset (scoped to one project + dataset) ---

pub(crate) struct CatalogDataset {
    engine: SharedEngine,
    project_id: ProjectId,
    dataset_id: DatasetId,
}

impl CatalogDataset {
    fn user_key(&self, name: &str) -> Vec<u8> {
        let mut k = self.project_id.as_bytes().to_vec();
        k.extend_from_slice(self.dataset_id.as_bytes());
        k.extend_from_slice(name.as_bytes());
        k
    }

    fn scope_prefix(&self) -> Vec<u8> {
        let mut k = self.project_id.as_bytes().to_vec();
        k.extend_from_slice(self.dataset_id.as_bytes());
        k
    }

    pub(crate) fn create_table(
        &self,
        name: &str,
        schema: Schema,
    ) -> Result<TableMeta, CatalogError> {
        if self.open_table(name)?.is_some() {
            return Err(CatalogError::AlreadyExists {
                kind: ResourceKind::Table,
                name: name.to_string(),
            });
        }
        let meta = TableMeta {
            id: TableId::new(),
            name: name.to_string(),
            schema,
        };
        put_meta(&self.engine, TABLES_TABLE_ID, self.user_key(name), &meta)?;
        Ok(meta)
    }

    pub(crate) fn open_table(&self, name: &str) -> Result<Option<TableMeta>, CatalogError> {
        get_meta(&self.engine, TABLES_TABLE_ID, self.user_key(name))
    }

    pub(crate) fn drop_table(&self, name: &str) -> Result<(), CatalogError> {
        if self.open_table(name)?.is_none() {
            return Err(CatalogError::NotFound {
                kind: ResourceKind::Table,
                name: name.to_string(),
            });
        }
        delete_meta(&self.engine, TABLES_TABLE_ID, self.user_key(name))
    }

    pub(crate) fn list_tables(&self) -> Result<Vec<TableMeta>, CatalogError> {
        list_metas(&self.engine, TABLES_TABLE_ID, &self.scope_prefix())
    }
}
