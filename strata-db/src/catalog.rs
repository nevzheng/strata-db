//! Catalog: project/dataset/table metadata stored in the system namespace.
//!
//! All catalog reads and writes go through the engine as normal rows in
//! reserved system tables (`_system._catalog.{_projects,_datasets,_tables}`).
//! Bootstrap relies on the constants in [`crate::consts`].
//!
//! Catalog rows ride the exact same pipeline as user data: the metadata
//! blob becomes a `Tuple` with one `Value::Json` field, then
//! [`Table::put`] / [`Table::get`] / [`Table::scan`] handle encoding
//! through the system-table schema. There's no longer a separate
//! "engine.put with raw bytes" path for system tables.
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
    CATALOG_DATASET_ID, DATASETS_TABLE_ID, DATASETS_TABLE_NAME, PROJECTS_TABLE_ID,
    PROJECTS_TABLE_NAME, SYSTEM_PROJECT_ID, TABLES_TABLE_ID, TABLES_TABLE_NAME,
    system_table_schema,
};
use crate::db::SharedEngine;
use crate::ids::{DatasetId, ProjectId, TableId};
use crate::schema::Schema;
use crate::tables::{Table, TypedStore};
use crate::types::{Tuple, Value};

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
// Thin wrappers around `Table`. Every helper builds a fresh `Table`
// handle for the relevant system table and routes the call through it.
// The typed-meta ↔ Tuple conversion lives in `meta_to_tuple` /
// `tuple_to_meta` and is the only thing the catalog adds on top of the
// generic table API.

fn system_table(engine: &SharedEngine, id: TableId, name: &str) -> Table {
    Table::new(
        engine.clone(),
        SYSTEM_PROJECT_ID,
        CATALOG_DATASET_ID,
        id,
        name.to_string(),
        system_table_schema(),
    )
}

fn meta_to_tuple<T: serde::Serialize>(meta: &T) -> Result<Tuple, CatalogError> {
    let json =
        serde_json::to_value(meta).map_err(|e| CatalogError::InternalError(e.to_string()))?;
    Ok(Tuple {
        values: vec![Value::Json(json)],
    })
}

fn tuple_to_meta<T: serde::de::DeserializeOwned>(tuple: Tuple) -> Result<T, CatalogError> {
    let json = match tuple.values.into_iter().next() {
        Some(Value::Json(j)) => j,
        _ => {
            return Err(CatalogError::InternalError(
                "expected Json value in catalog row".into(),
            ));
        }
    };
    serde_json::from_value(json).map_err(|e| CatalogError::InternalError(e.to_string()))
}

fn put_meta<T: serde::Serialize>(
    engine: &SharedEngine,
    table_id: TableId,
    table_name: &str,
    user_key: &[u8],
    meta: &T,
) -> Result<(), CatalogError> {
    let table = system_table(engine, table_id, table_name);
    table.put(user_key, &meta_to_tuple(meta)?)
}

fn get_meta<T: serde::de::DeserializeOwned>(
    engine: &SharedEngine,
    table_id: TableId,
    table_name: &str,
    user_key: &[u8],
) -> Result<Option<T>, CatalogError> {
    let table = system_table(engine, table_id, table_name);
    match table.get(user_key)? {
        None => Ok(None),
        Some(tuple) => Ok(Some(tuple_to_meta(tuple)?)),
    }
}

fn delete_meta(
    engine: &SharedEngine,
    table_id: TableId,
    table_name: &str,
    user_key: &[u8],
) -> Result<(), CatalogError> {
    let table = system_table(engine, table_id, table_name);
    table.delete(user_key)
}

fn list_metas<T: serde::de::DeserializeOwned>(
    engine: &SharedEngine,
    table_id: TableId,
    table_name: &str,
    user_key_prefix: &[u8],
) -> Result<Vec<T>, CatalogError> {
    let table = system_table(engine, table_id, table_name);
    let entries = table.scan(user_key_prefix)?;
    entries
        .into_iter()
        .map(|(_, tuple)| tuple_to_meta(tuple))
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
            PROJECTS_TABLE_NAME,
            name.as_bytes(),
            &meta,
        )?;
        Ok(meta)
    }

    pub(crate) fn open_project(&self, name: &str) -> Result<Option<ProjectMeta>, CatalogError> {
        get_meta(
            &self.engine,
            PROJECTS_TABLE_ID,
            PROJECTS_TABLE_NAME,
            name.as_bytes(),
        )
    }

    pub(crate) fn drop_project(&self, name: &str) -> Result<(), CatalogError> {
        if self.open_project(name)?.is_none() {
            return Err(CatalogError::NotFound {
                kind: ResourceKind::Project,
                name: name.to_string(),
            });
        }
        delete_meta(
            &self.engine,
            PROJECTS_TABLE_ID,
            PROJECTS_TABLE_NAME,
            name.as_bytes(),
        )
    }

    pub(crate) fn list_projects(&self) -> Result<Vec<ProjectMeta>, CatalogError> {
        list_metas(&self.engine, PROJECTS_TABLE_ID, PROJECTS_TABLE_NAME, &[])
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
        put_meta(
            &self.engine,
            DATASETS_TABLE_ID,
            DATASETS_TABLE_NAME,
            &self.user_key(name),
            &meta,
        )?;
        Ok(meta)
    }

    pub(crate) fn open_dataset(&self, name: &str) -> Result<Option<DatasetMeta>, CatalogError> {
        get_meta(
            &self.engine,
            DATASETS_TABLE_ID,
            DATASETS_TABLE_NAME,
            &self.user_key(name),
        )
    }

    pub(crate) fn drop_dataset(&self, name: &str) -> Result<(), CatalogError> {
        if self.open_dataset(name)?.is_none() {
            return Err(CatalogError::NotFound {
                kind: ResourceKind::Dataset,
                name: name.to_string(),
            });
        }
        delete_meta(
            &self.engine,
            DATASETS_TABLE_ID,
            DATASETS_TABLE_NAME,
            &self.user_key(name),
        )
    }

    pub(crate) fn list_datasets(&self) -> Result<Vec<DatasetMeta>, CatalogError> {
        list_metas(
            &self.engine,
            DATASETS_TABLE_ID,
            DATASETS_TABLE_NAME,
            self.project_id.as_bytes(),
        )
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
        put_meta(
            &self.engine,
            TABLES_TABLE_ID,
            TABLES_TABLE_NAME,
            &self.user_key(name),
            &meta,
        )?;
        Ok(meta)
    }

    pub(crate) fn open_table(&self, name: &str) -> Result<Option<TableMeta>, CatalogError> {
        get_meta(
            &self.engine,
            TABLES_TABLE_ID,
            TABLES_TABLE_NAME,
            &self.user_key(name),
        )
    }

    pub(crate) fn drop_table(&self, name: &str) -> Result<(), CatalogError> {
        if self.open_table(name)?.is_none() {
            return Err(CatalogError::NotFound {
                kind: ResourceKind::Table,
                name: name.to_string(),
            });
        }
        delete_meta(
            &self.engine,
            TABLES_TABLE_ID,
            TABLES_TABLE_NAME,
            &self.user_key(name),
        )
    }

    pub(crate) fn list_tables(&self) -> Result<Vec<TableMeta>, CatalogError> {
        list_metas(
            &self.engine,
            TABLES_TABLE_ID,
            TABLES_TABLE_NAME,
            &self.scope_prefix(),
        )
    }
}
