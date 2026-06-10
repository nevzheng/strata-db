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

use strata_store::StorageEngine;
use strata_store::memstore::BTreeMapStore;

use crate::catalog::consts::{
    CATALOG_DATASET_ID, DATASETS_TABLE_ID, PROJECTS_TABLE_ID, SYSTEM_PROJECT_ID, TABLES_TABLE_ID,
    system_table_schema,
};
use crate::catalog::db::TableApi;
use crate::catalog::ids::{DatasetId, ProjectId, QueryId, TableId};
use crate::catalog::schema::Schema;
use crate::catalog::tables::Table;
use crate::query::QueryError;
use crate::storage::table_api::{ScanOptions, TableReader, TableWriter};
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
// Catalog rows are stored in reserved system tables under
// `(SYSTEM_PROJECT_ID, CATALOG_DATASET_ID, <table_id>)`. Schema is
// `(pk: Bytes, meta: Json)` — the PK is the composite natural key for
// the row (project name, `(project_id, dataset_name)`, etc.), encoded
// raw into the storage user_key so prefix scans line up. Reads and
// writes flow through `storage::table_api`.

fn meta_to_tuple<T: serde::Serialize>(user_key: &[u8], meta: &T) -> Result<Tuple, QueryError> {
    Ok(Tuple {
        values: vec![
            Value::Bytes(user_key.to_vec()),
            Value::Json(serde_json::to_value(meta)?),
        ],
    })
}

fn tuple_to_meta<T: serde::de::DeserializeOwned>(tuple: Tuple) -> Result<T, QueryError> {
    // Schema is (pk: Bytes, meta: Json) — skip the PK column.
    let json = match tuple.values.into_iter().nth(1) {
        Some(Value::Json(j)) => j,
        _ => {
            return Err(QueryError::Codec(crate::query::CodecError::Serde(
                serde::de::Error::custom("expected Json meta column in catalog row"),
            )));
        }
    };
    Ok(serde_json::from_value(json)?)
}

fn write_meta<T: serde::Serialize>(
    engine: &mut StorageEngine<BTreeMapStore>,
    system_table: &Table,
    user_key: &[u8],
    meta: &T,
) -> Result<(), QueryError> {
    let tuple = meta_to_tuple(user_key, meta)?;
    TableWriter::new(engine, system_table).put(&tuple)
}

fn remove_meta(
    engine: &mut StorageEngine<BTreeMapStore>,
    system_table: &Table,
    user_key: &[u8],
) -> Result<(), QueryError> {
    TableWriter::new(engine, system_table).delete(&Value::Bytes(user_key.to_vec()))
}

/// Build a `Table` descriptor for one of our system tables. Lets the
/// catalog reuse the regular Table API instead of hand-rolling row
/// keys against the engine.
fn system_table(table_id: TableId, name: &'static str) -> Table {
    Table::new(
        SYSTEM_PROJECT_ID,
        CATALOG_DATASET_ID,
        table_id,
        name.to_string(),
        system_table_schema(),
    )
}

fn projects_meta_table() -> Table {
    system_table(PROJECTS_TABLE_ID, "_projects")
}

fn datasets_meta_table() -> Table {
    system_table(DATASETS_TABLE_ID, "_datasets")
}

fn tables_meta_table() -> Table {
    system_table(TABLES_TABLE_ID, "_tables")
}

fn list_metas<T: serde::de::DeserializeOwned>(
    engine: &StorageEngine<BTreeMapStore>,
    system_table: Table,
    user_key_prefix: &[u8],
) -> Result<Vec<T>, QueryError> {
    TableReader::new(engine, &system_table)
        .scan(ScanOptions::new().prefix(user_key_prefix))
        .map(|row| row.and_then(tuple_to_meta))
        .collect()
}

// --- Read-side API ---------------------------------------------------------
//
// These functions take a borrowed `StorageEngine` so they can be called
// from a context that already holds the storage lock (e.g. the binder
// running inside a `QueryContext`). They sit alongside the lock-acquiring
// `Catalog` API below, which wraps these same lookups in a fresh lock.

pub(crate) fn get_project(
    engine: &StorageEngine<BTreeMapStore>,
    name: &str,
) -> Result<Option<ProjectMeta>, QueryError> {
    lookup_meta(engine, &projects_meta_table(), name.as_bytes())
}

pub(crate) fn get_dataset(
    engine: &StorageEngine<BTreeMapStore>,
    project_id: ProjectId,
    name: &str,
) -> Result<Option<DatasetMeta>, QueryError> {
    let mut key = project_id.as_bytes().to_vec();
    key.extend_from_slice(name.as_bytes());
    lookup_meta(engine, &datasets_meta_table(), &key)
}

pub(crate) fn get_table(
    engine: &StorageEngine<BTreeMapStore>,
    project_id: ProjectId,
    dataset_id: DatasetId,
    name: &str,
) -> Result<Option<TableMeta>, QueryError> {
    let mut key = project_id.as_bytes().to_vec();
    key.extend_from_slice(dataset_id.as_bytes());
    key.extend_from_slice(name.as_bytes());
    lookup_meta(engine, &tables_meta_table(), &key)
}

/// Resolve a three-part `project.dataset.table` name to a `Table`
/// handle. Errors with `CatalogError::NotFound` at the first missing
/// segment.
pub(crate) fn resolve_table(
    engine: &StorageEngine<BTreeMapStore>,
    project: &str,
    dataset: &str,
    table: &str,
) -> Result<Table, QueryError> {
    let project_meta = get_project(engine, project)?.ok_or_else(|| CatalogError::NotFound {
        kind: ResourceKind::Project,
        name: project.to_string(),
    })?;
    let dataset_meta =
        get_dataset(engine, project_meta.id, dataset)?.ok_or_else(|| CatalogError::NotFound {
            kind: ResourceKind::Dataset,
            name: dataset.to_string(),
        })?;
    let table_meta =
        get_table(engine, project_meta.id, dataset_meta.id, table)?.ok_or_else(|| {
            CatalogError::NotFound {
                kind: ResourceKind::Table,
                name: table.to_string(),
            }
        })?;
    Ok(Table::new(
        project_meta.id,
        dataset_meta.id,
        table_meta.id,
        table_meta.name,
        table_meta.schema,
    ))
}

fn lookup_meta<T: serde::de::DeserializeOwned>(
    engine: &StorageEngine<BTreeMapStore>,
    system_table: &Table,
    user_key: &[u8],
) -> Result<Option<T>, QueryError> {
    match TableReader::new(engine, system_table).get(&Value::Bytes(user_key.to_vec()))? {
        None => Ok(None),
        Some(tuple) => Ok(Some(tuple_to_meta(tuple)?)),
    }
}

/// Read-side catalog handle: a thin wrapper around an engine borrow
/// that surfaces the catalog operations as methods. Returned by
/// [`crate::QueryContext::catalog`].
///
/// Conceptually this is just "specific reads against the meta tables"
/// — if those reads ever express cleanly through normal SQL, this
/// type disappears.
#[derive(Clone, Copy)]
pub(crate) struct CatalogReader<'a> {
    engine: &'a StorageEngine<BTreeMapStore>,
}

// Most of CatalogReader's CRUD-shaped reads are scaffolding for upcoming
// DDL / system-introspection paths — only `resolve_table` is hit today.
#[allow(dead_code)]
impl<'a> CatalogReader<'a> {
    pub(crate) fn new(engine: &'a StorageEngine<BTreeMapStore>) -> Self {
        Self { engine }
    }

    pub(crate) fn get_project(&self, name: &str) -> Result<Option<ProjectMeta>, QueryError> {
        get_project(self.engine, name)
    }

    pub(crate) fn get_dataset(
        &self,
        project_id: ProjectId,
        name: &str,
    ) -> Result<Option<DatasetMeta>, QueryError> {
        get_dataset(self.engine, project_id, name)
    }

    pub(crate) fn get_table(
        &self,
        project_id: ProjectId,
        dataset_id: DatasetId,
        name: &str,
    ) -> Result<Option<TableMeta>, QueryError> {
        get_table(self.engine, project_id, dataset_id, name)
    }

    pub(crate) fn resolve_table(
        &self,
        project: &str,
        dataset: &str,
        table: &str,
    ) -> Result<Table, QueryError> {
        resolve_table(self.engine, project, dataset, table)
    }
}

// --- Catalog (top-level) ---

pub(crate) struct Catalog<'db> {
    api: TableApi<'db>,
}

impl<'db> Catalog<'db> {
    pub(crate) fn new(api: TableApi<'db>) -> Self {
        Self { api }
    }

    pub(crate) fn create_project(&self, name: &str) -> Result<ProjectMeta, QueryError> {
        let mut engine = self.api.write();
        if get_project(&engine, name)?.is_some() {
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
        write_meta(&mut engine, &projects_meta_table(), name.as_bytes(), &meta)?;
        Ok(meta)
    }

    pub(crate) fn open_project(&self, name: &str) -> Result<Option<ProjectMeta>, QueryError> {
        let engine = self.api.read();
        get_project(&engine, name)
    }

    pub(crate) fn drop_project(&self, name: &str) -> Result<(), QueryError> {
        let mut engine = self.api.write();
        if get_project(&engine, name)?.is_none() {
            return Err(CatalogError::NotFound {
                kind: ResourceKind::Project,
                name: name.to_string(),
            }
            .into());
        }
        remove_meta(&mut engine, &projects_meta_table(), name.as_bytes())
    }

    pub(crate) fn list_projects(&self) -> Result<Vec<ProjectMeta>, QueryError> {
        let engine = self.api.read();
        list_metas(&engine, projects_meta_table(), &[])
    }

    /// Narrow the catalog to a single project's scope for dataset operations.
    pub(crate) fn project(&self, project_id: ProjectId) -> CatalogProject<'db> {
        CatalogProject {
            api: self.api,
            project_id,
        }
    }
}

// --- CatalogProject (scoped to one project) ---

pub(crate) struct CatalogProject<'db> {
    api: TableApi<'db>,
    project_id: ProjectId,
}

impl<'db> CatalogProject<'db> {
    fn user_key(&self, name: &str) -> Vec<u8> {
        let mut k = self.project_id.as_bytes().to_vec();
        k.extend_from_slice(name.as_bytes());
        k
    }

    pub(crate) fn create_dataset(&self, name: &str) -> Result<DatasetMeta, QueryError> {
        let mut engine = self.api.write();
        if get_dataset(&engine, self.project_id, name)?.is_some() {
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
        write_meta(
            &mut engine,
            &datasets_meta_table(),
            &self.user_key(name),
            &meta,
        )?;
        Ok(meta)
    }

    pub(crate) fn open_dataset(&self, name: &str) -> Result<Option<DatasetMeta>, QueryError> {
        let engine = self.api.read();
        get_dataset(&engine, self.project_id, name)
    }

    pub(crate) fn drop_dataset(&self, name: &str) -> Result<(), QueryError> {
        let mut engine = self.api.write();
        if get_dataset(&engine, self.project_id, name)?.is_none() {
            return Err(CatalogError::NotFound {
                kind: ResourceKind::Dataset,
                name: name.to_string(),
            }
            .into());
        }
        remove_meta(&mut engine, &datasets_meta_table(), &self.user_key(name))
    }

    pub(crate) fn list_datasets(&self) -> Result<Vec<DatasetMeta>, QueryError> {
        let engine = self.api.read();
        list_metas(&engine, datasets_meta_table(), self.project_id.as_bytes())
    }

    /// Narrow further to a single dataset's scope for table operations.
    pub(crate) fn dataset(&self, dataset_id: DatasetId) -> CatalogDataset<'db> {
        CatalogDataset {
            api: self.api,
            project_id: self.project_id,
            dataset_id,
        }
    }
}

// --- CatalogDataset (scoped to one project + dataset) ---

pub(crate) struct CatalogDataset<'db> {
    api: TableApi<'db>,
    project_id: ProjectId,
    dataset_id: DatasetId,
}

impl<'db> CatalogDataset<'db> {
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
        let mut engine = self.api.write();
        if get_table(&engine, self.project_id, self.dataset_id, name)?.is_some() {
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
        write_meta(
            &mut engine,
            &tables_meta_table(),
            &self.user_key(name),
            &meta,
        )?;
        Ok(meta)
    }

    pub(crate) fn open_table(&self, name: &str) -> Result<Option<TableMeta>, QueryError> {
        let engine = self.api.read();
        get_table(&engine, self.project_id, self.dataset_id, name)
    }

    pub(crate) fn drop_table(&self, name: &str) -> Result<(), QueryError> {
        let mut engine = self.api.write();
        if get_table(&engine, self.project_id, self.dataset_id, name)?.is_none() {
            return Err(CatalogError::NotFound {
                kind: ResourceKind::Table,
                name: name.to_string(),
            }
            .into());
        }
        remove_meta(&mut engine, &tables_meta_table(), &self.user_key(name))
    }

    pub(crate) fn list_tables(&self) -> Result<Vec<TableMeta>, QueryError> {
        let engine = self.api.read();
        list_metas(&engine, tables_meta_table(), &self.scope_prefix())
    }
}
