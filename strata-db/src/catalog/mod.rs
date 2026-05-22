//! Catalog: identity + metadata for projects, datasets, and tables.
//!
//! This module groups everything that names or describes user data —
//! the `Db` entry point, the `Project` / `Dataset` / `Table` handles,
//! the schema language, and the system tables that persist all of it.
//! The catalog itself (the file you're reading) is the bridge to the
//! storage engine for that metadata: it reads and writes catalog rows
//! using raw byte-key addressing — composite `(project_id, name)` and
//! `(project_id, dataset_id, name)` keys — that the PK-derived
//! [`crate::QueryContext`] API doesn't expose. Each row is a
//! single-column tuple holding the metadata as `Value::Json`.
//!
//! ## Known gaps (intentional, scaffolded)
//!
//! - **No cascade on drop.** Dropping a project leaves its dataset and
//!   table metadata rows in the engine, unreachable through the catalog
//!   API. A future pass will list children and drop them transactively.
//! - **Existence checks are non-atomic.** `create_*` and `drop_*`
//!   read-then-write; concurrent callers could race. Fine for the
//!   current single-threaded use; engine-level CAS would close the gap.

pub mod consts;
pub mod dataset;
pub mod db;
pub mod ids;
pub mod project;
pub mod schema;
pub mod tables;

use crate::catalog::consts::{
    CATALOG_DATASET_ID, DATASETS_TABLE_ID, PROJECTS_TABLE_ID, SYSTEM_PROJECT_ID, TABLES_TABLE_ID,
    system_table_schema,
};
use crate::catalog::db::SharedEngine;
use crate::catalog::ids::{DatasetId, ProjectId, QueryId, TableId};
use crate::catalog::schema::Schema;
use crate::query::QueryError;
use crate::storage::row::{RowKey, next_after_prefix};
use crate::storage::types::{Tuple, Value};

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

/// One row per query executed within a project. `info` is freeform
/// JSON until we know what's worth promoting to typed columns.
/// Scaffolding — read/write helpers land when the planner records
/// into it.
#[allow(dead_code)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct QueryMeta {
    pub id: QueryId,
    pub info: serde_json::Value,
}

// --- System-table helpers ---
//
// Catalog rows live in reserved system tables under
// `(SYSTEM_PROJECT_ID, CATALOG_DATASET_ID, <table_id>)`. Each helper
// constructs the row key for the addressed catalog row and goes
// straight to the engine — there's no `Table` handle in the picture
// because the catalog needs raw byte-key addressing (composite project
// + name keys) that the PK-derived [`crate::QueryContext`] API doesn't
// expose. Eventually this collapses into plan-based catalog ops once
// `Insert`/`Delete` operators and proper PK schemas exist.

fn meta_to_tuple<T: serde::Serialize>(meta: &T) -> Result<Tuple, QueryError> {
    let json = serde_json::to_value(meta)?;
    Ok(Tuple {
        values: vec![Value::Json(json)],
    })
}

fn tuple_to_meta<T: serde::de::DeserializeOwned>(tuple: Tuple) -> Result<T, QueryError> {
    let json = match tuple.values.into_iter().next() {
        Some(Value::Json(j)) => j,
        _ => {
            // Schema invariant: every catalog row is one Json column.
            // Reaching here would mean a stored row doesn't match the
            // declared system-table schema — treat as a decode failure.
            return Err(QueryError::Codec(crate::query::CodecError::Serde(
                serde::de::Error::custom("expected Json value in catalog row"),
            )));
        }
    };
    Ok(serde_json::from_value(json)?)
}

fn row_key(table_id: TableId, user_key: &[u8]) -> Vec<u8> {
    RowKey::new(
        SYSTEM_PROJECT_ID,
        CATALOG_DATASET_ID,
        table_id,
        user_key.to_vec(),
    )
    .encode()
}

fn put_meta<T: serde::Serialize>(
    engine: &SharedEngine,
    table_id: TableId,
    user_key: &[u8],
    meta: &T,
) -> Result<(), QueryError> {
    let value_bytes = system_table_schema().encode(&meta_to_tuple(meta)?);
    engine
        .lock()
        .unwrap()
        .put(&row_key(table_id, user_key), &value_bytes)?;
    Ok(())
}

fn get_meta<T: serde::de::DeserializeOwned>(
    engine: &SharedEngine,
    table_id: TableId,
    user_key: &[u8],
) -> Result<Option<T>, QueryError> {
    let raw = engine.lock().unwrap().get(&row_key(table_id, user_key))?;
    match raw {
        None => Ok(None),
        Some(bytes) => {
            let tuple = system_table_schema().decode(&bytes)?;
            Ok(Some(tuple_to_meta(tuple)?))
        }
    }
}

fn delete_meta(
    engine: &SharedEngine,
    table_id: TableId,
    user_key: &[u8],
) -> Result<(), QueryError> {
    engine
        .lock()
        .unwrap()
        .delete(&row_key(table_id, user_key))?;
    Ok(())
}

fn list_metas<T: serde::de::DeserializeOwned>(
    engine: &SharedEngine,
    table_id: TableId,
    user_key_prefix: &[u8],
) -> Result<Vec<T>, QueryError> {
    let mut prefix = RowKey::table_prefix(SYSTEM_PROJECT_ID, CATALOG_DATASET_ID, table_id);
    prefix.extend_from_slice(user_key_prefix);

    // Engine cursor borrows from the lock guard; drain before its scope ends.
    let entries: Vec<(Vec<u8>, Vec<u8>)> = {
        let engine = engine.lock().unwrap();
        let iter = match next_after_prefix(&prefix) {
            Some(end) => engine.scan(prefix..end),
            None => engine.scan(prefix..),
        };
        iter.collect::<Result<_, _>>()?
    };

    let schema = system_table_schema();
    entries
        .into_iter()
        .map(|(_, v)| {
            let tuple = schema.decode(&v)?;
            tuple_to_meta(tuple)
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

    pub(crate) fn create_project(&self, name: &str) -> Result<ProjectMeta, QueryError> {
        if self.open_project(name)?.is_some() {
            return Err(CatalogError::AlreadyExists {
                kind: ResourceKind::Project,
                name: name.to_string(),
            }
            .into());
        }
        let meta = ProjectMeta {
            id: ProjectId::new(),
            name: name.to_string(),
        };
        put_meta(&self.engine, PROJECTS_TABLE_ID, name.as_bytes(), &meta)?;
        Ok(meta)
    }

    pub(crate) fn open_project(&self, name: &str) -> Result<Option<ProjectMeta>, QueryError> {
        get_meta(&self.engine, PROJECTS_TABLE_ID, name.as_bytes())
    }

    pub(crate) fn drop_project(&self, name: &str) -> Result<(), QueryError> {
        if self.open_project(name)?.is_none() {
            return Err(CatalogError::NotFound {
                kind: ResourceKind::Project,
                name: name.to_string(),
            }
            .into());
        }
        delete_meta(&self.engine, PROJECTS_TABLE_ID, name.as_bytes())
    }

    pub(crate) fn list_projects(&self) -> Result<Vec<ProjectMeta>, QueryError> {
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

    pub(crate) fn create_dataset(&self, name: &str) -> Result<DatasetMeta, QueryError> {
        if self.open_dataset(name)?.is_some() {
            return Err(CatalogError::AlreadyExists {
                kind: ResourceKind::Dataset,
                name: name.to_string(),
            }
            .into());
        }
        let meta = DatasetMeta {
            id: DatasetId::new(),
            name: name.to_string(),
        };
        put_meta(&self.engine, DATASETS_TABLE_ID, &self.user_key(name), &meta)?;
        Ok(meta)
    }

    pub(crate) fn open_dataset(&self, name: &str) -> Result<Option<DatasetMeta>, QueryError> {
        get_meta(&self.engine, DATASETS_TABLE_ID, &self.user_key(name))
    }

    pub(crate) fn drop_dataset(&self, name: &str) -> Result<(), QueryError> {
        if self.open_dataset(name)?.is_none() {
            return Err(CatalogError::NotFound {
                kind: ResourceKind::Dataset,
                name: name.to_string(),
            }
            .into());
        }
        delete_meta(&self.engine, DATASETS_TABLE_ID, &self.user_key(name))
    }

    pub(crate) fn list_datasets(&self) -> Result<Vec<DatasetMeta>, QueryError> {
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

    pub(crate) fn create_table(&self, name: &str, schema: Schema) -> Result<TableMeta, QueryError> {
        if self.open_table(name)?.is_some() {
            return Err(CatalogError::AlreadyExists {
                kind: ResourceKind::Table,
                name: name.to_string(),
            }
            .into());
        }
        let meta = TableMeta {
            id: TableId::new(),
            name: name.to_string(),
            schema,
        };
        put_meta(&self.engine, TABLES_TABLE_ID, &self.user_key(name), &meta)?;
        Ok(meta)
    }

    pub(crate) fn open_table(&self, name: &str) -> Result<Option<TableMeta>, QueryError> {
        get_meta(&self.engine, TABLES_TABLE_ID, &self.user_key(name))
    }

    pub(crate) fn drop_table(&self, name: &str) -> Result<(), QueryError> {
        if self.open_table(name)?.is_none() {
            return Err(CatalogError::NotFound {
                kind: ResourceKind::Table,
                name: name.to_string(),
            }
            .into());
        }
        delete_meta(&self.engine, TABLES_TABLE_ID, &self.user_key(name))
    }

    pub(crate) fn list_tables(&self) -> Result<Vec<TableMeta>, QueryError> {
        list_metas(&self.engine, TABLES_TABLE_ID, &self.scope_prefix())
    }
}
