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
//! - **Dead incarnations are not garbage-collected.** `CREATE OR
//!   REPLACE` (and `DROP`) leave the prior incarnation's data rows on
//!   disk under a lower [`TruncationId`](crate::catalog::ids::TruncationId)
//!   prefix — unreachable, since reads only scan the live (largest) id.
//!   Correctness never depends on reclaiming them: the swap is a single
//!   metadata write, and the old rows are simply orphaned. A future GC
//!   pass scans for these dead prefixes (incarnations below the live id,
//!   eventually past a retention window) and deletes them — at which
//!   point "dead incarnations" become a Time-Travel retention policy
//!   rather than pure garbage. Because the truncation id is monotonic,
//!   "dead = everything below the live id" is implicit; no graveyard
//!   list is stored.

pub mod consts;
pub mod dataset;
pub mod db;
pub mod ids;
pub mod project;
pub mod schema;
pub mod system;
pub mod tables;

use strata_store::StorageEngine;
use strata_store::memstore::BTreeMapStore;

use crate::catalog::consts::{
    CATALOG_DATASET_ID, DATASETS_TABLE_ID, PROJECTS_TABLE_ID, STATS_TABLE_ID, STATS_TABLE_NAME,
    SYSTEM_PROJECT_ID, TABLES_TABLE_ID, system_table_schema,
};
use crate::catalog::db::TableApi;
use crate::catalog::ids::{DatasetId, ProjectId, QueryId, TableId, TruncationId};
use crate::catalog::schema::Schema;
use crate::catalog::system::ColumnStats;
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
    NotFound {
        kind: ResourceKind,
        name: String,
    },
    AlreadyExists {
        kind: ResourceKind,
        name: String,
    },
    /// A table's incarnation counter hit `u64::MAX` — no further
    /// `CREATE OR REPLACE` is possible without GC reclaiming the space.
    /// Effectively unreachable; surfaced rather than wrapping the
    /// counter, which would alias new writes onto old incarnation data.
    TruncationExhausted {
        name: String,
    },
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

/// One row per table *incarnation*. `CREATE OR REPLACE` writes a new
/// row with a higher `truncation_id`, keeping its own `schema`; the
/// live incarnation is the one with the largest id. `id` (the logical
/// table identity) is stable across incarnations.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct TableMeta {
    pub id: TableId,
    pub truncation_id: TruncationId,
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
    // System tables are never replaced, so they live at the initial
    // incarnation forever.
    Table::new(
        SYSTEM_PROJECT_ID,
        CATALOG_DATASET_ID,
        table_id,
        TruncationId::INITIAL,
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

fn stats_meta_table() -> Table {
    system_table(STATS_TABLE_ID, STATS_TABLE_NAME)
}

/// Key for a column-statistics row: `schema\0table\0column`. NUL separators
/// keep the variable-length parts unambiguous.
// Test-only until ANALYZE exists; the read path needs no key.
#[cfg(test)]
fn stats_key(stats: &ColumnStats) -> Vec<u8> {
    let mut key = Vec::new();
    for part in [&stats.schemaname, &stats.tablename, &stats.attname] {
        key.extend_from_slice(part.as_bytes());
        key.push(0);
    }
    key
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
    lookup_meta(
        engine,
        &datasets_meta_table(),
        &dataset_key(project_id, name),
    )
}

/// The live incarnation of `name`: the catalog row with the largest
/// truncation id. `None` if the table was never created (or fully
/// dropped). Lower-id rows are retained history.
pub(crate) fn get_table(
    engine: &StorageEngine<BTreeMapStore>,
    project_id: ProjectId,
    dataset_id: DatasetId,
    name: &str,
) -> Result<Option<TableMeta>, QueryError> {
    Ok(table_incarnations(engine, project_id, dataset_id, name)?
        .into_iter()
        .max_by_key(|m| m.truncation_id))
}

/// Every retained incarnation of `name`, in no particular order. We
/// scan the whole dataset and filter on the deserialized name rather
/// than prefix-scanning by name, because table names are variable
/// length — a name prefix scan would alias `ev` onto `events`.
fn table_incarnations(
    engine: &StorageEngine<BTreeMapStore>,
    project_id: ProjectId,
    dataset_id: DatasetId,
    name: &str,
) -> Result<Vec<TableMeta>, QueryError> {
    let metas: Vec<TableMeta> = list_metas(
        engine,
        tables_meta_table(),
        &dataset_scope(project_id, dataset_id),
    )?;
    Ok(metas.into_iter().filter(|m| m.name == name).collect())
}

// Composite catalog keys, shared by readers (`get_*`) and writers (`Catalog`)
// so the two can never disagree on a row's address. Datasets are scoped under
// their project, tables under their (project, dataset). A table row is keyed
// per incarnation, with the truncation id as a fixed-width suffix — so each
// `CREATE OR REPLACE` lands a distinct row rather than overwriting.

fn dataset_key(project_id: ProjectId, name: &str) -> Vec<u8> {
    let mut key = project_id.as_bytes().to_vec();
    key.extend_from_slice(name.as_bytes());
    key
}

fn dataset_scope(project_id: ProjectId, dataset_id: DatasetId) -> Vec<u8> {
    let mut key = project_id.as_bytes().to_vec();
    key.extend_from_slice(dataset_id.as_bytes());
    key
}

fn table_key(
    project_id: ProjectId,
    dataset_id: DatasetId,
    name: &str,
    truncation_id: TruncationId,
) -> Vec<u8> {
    let mut key = dataset_scope(project_id, dataset_id);
    key.extend_from_slice(name.as_bytes());
    // Fixed-width suffix: makes the (name, truncation_id) pair an
    // injective key, so distinct incarnations never collide.
    key.extend_from_slice(&truncation_id.to_be_bytes());
    key
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
        table_meta.truncation_id,
        table_meta.name,
        table_meta.schema,
    ))
}

// --- Write-side free functions ---------------------------------------------
//
// Mirror the read-side `get_*` helpers: they take a borrowed `&mut
// StorageEngine` so a caller already holding the storage lock (the
// executor running a DDL sink under a `QueryContext`) can write catalog
// metadata without going through the lock-acquiring `Catalog` facade.
// `Catalog`'s methods delegate here so the two paths can't diverge.

/// `CREATE SCHEMA`: create a dataset under `project_id`. Errors
/// `AlreadyExists` if the name is taken (the `IF NOT EXISTS` caller
/// decides whether to swallow that).
pub(crate) fn create_dataset(
    engine: &mut StorageEngine<BTreeMapStore>,
    project_id: ProjectId,
    name: &str,
) -> Result<DatasetMeta, QueryError> {
    if get_dataset(engine, project_id, name)?.is_some() {
        return Err(already_exists(ResourceKind::Dataset, name));
    }
    let meta = DatasetMeta {
        id: DatasetId::new(),
        name: name.to_string(),
    };
    write_meta(
        engine,
        &datasets_meta_table(),
        &dataset_key(project_id, name),
        &meta,
    )?;
    Ok(meta)
}

/// `CREATE TABLE`: mint a fresh table at the initial incarnation.
/// Errors `AlreadyExists` if any incarnation of the name is live.
pub(crate) fn create_table(
    engine: &mut StorageEngine<BTreeMapStore>,
    project_id: ProjectId,
    dataset_id: DatasetId,
    name: &str,
    schema: Schema,
) -> Result<TableMeta, QueryError> {
    if get_table(engine, project_id, dataset_id, name)?.is_some() {
        return Err(already_exists(ResourceKind::Table, name));
    }
    let meta = TableMeta {
        id: TableId::new(),
        truncation_id: TruncationId::INITIAL,
        name: name.to_string(),
        schema,
    };
    put_table_meta(engine, project_id, dataset_id, &meta)?;
    Ok(meta)
}

/// `CREATE OR REPLACE TABLE`: write a new incarnation one above the
/// current largest truncation id, keeping the table's logical `id`
/// stable. Behaves like `create_table` when the name is absent. The new
/// incarnation starts empty; the old one's rows are retained but
/// unreachable (a future GC pass reclaims them). Errors
/// `TruncationExhausted` only if the counter has hit `u64::MAX`.
pub(crate) fn replace_table(
    engine: &mut StorageEngine<BTreeMapStore>,
    project_id: ProjectId,
    dataset_id: DatasetId,
    name: &str,
    schema: Schema,
) -> Result<TableMeta, QueryError> {
    let (truncation_id, id) = match get_table(engine, project_id, dataset_id, name)? {
        Some(current) => (
            current
                .truncation_id
                .next()
                .ok_or_else(|| truncation_exhausted(name))?,
            // Stable logical identity across incarnations.
            current.id,
        ),
        None => (TruncationId::INITIAL, TableId::new()),
    };
    let meta = TableMeta {
        id,
        truncation_id,
        name: name.to_string(),
        schema,
    };
    put_table_meta(engine, project_id, dataset_id, &meta)?;
    Ok(meta)
}

/// Persist one incarnation row, keyed by its `(name, truncation_id)`.
fn put_table_meta(
    engine: &mut StorageEngine<BTreeMapStore>,
    project_id: ProjectId,
    dataset_id: DatasetId,
    meta: &TableMeta,
) -> Result<(), QueryError> {
    write_meta(
        engine,
        &tables_meta_table(),
        &table_key(project_id, dataset_id, &meta.name, meta.truncation_id),
        meta,
    )
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

    /// Every project. Used by system-catalog enumeration.
    pub(crate) fn list_projects(&self) -> Result<Vec<ProjectMeta>, QueryError> {
        list_metas(self.engine, projects_meta_table(), &[])
    }

    /// Every dataset in `project_id`.
    pub(crate) fn list_datasets(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<DatasetMeta>, QueryError> {
        list_metas(self.engine, datasets_meta_table(), project_id.as_bytes())
    }

    /// One row per *live* table in `(project_id, dataset_id)` — the highest
    /// incarnation of each name (see [`Catalog::list_tables`]).
    pub(crate) fn list_tables(
        &self,
        project_id: ProjectId,
        dataset_id: DatasetId,
    ) -> Result<Vec<TableMeta>, QueryError> {
        let metas: Vec<TableMeta> = list_metas(
            self.engine,
            tables_meta_table(),
            &dataset_scope(project_id, dataset_id),
        )?;
        Ok(live_incarnations(metas))
    }

    /// Every stored column-statistics row (the `pg_stats` / `st_stats`
    /// source). Empty until `ANALYZE` writes any.
    pub(crate) fn list_column_stats(&self) -> Result<Vec<ColumnStats>, QueryError> {
        list_metas(self.engine, stats_meta_table(), &[])
    }
}

/// Collapse a name's table incarnations to the live one (highest
/// `truncation_id`), dropping retained history. Shared by the read- and
/// write-side `list_tables`.
fn live_incarnations(metas: Vec<TableMeta>) -> Vec<TableMeta> {
    let mut live: std::collections::HashMap<String, TableMeta> = std::collections::HashMap::new();
    for meta in metas {
        let keep = live
            .get(&meta.name)
            .is_none_or(|existing| meta.truncation_id > existing.truncation_id);
        if keep {
            live.insert(meta.name.clone(), meta);
        }
    }
    live.into_values().collect()
}

// --- Catalog: write-side metadata operations ---
//
// One handle for all of projects, datasets, and tables. Scope is passed as
// id parameters (a dataset belongs to a project, a table to a project +
// dataset) rather than carried in nested handles. Each `create_*`/`drop_*`
// reads-then-writes under a single engine borrow.

pub(crate) struct Catalog<'db> {
    api: TableApi<'db>,
}

impl<'db> Catalog<'db> {
    pub(crate) fn new(api: TableApi<'db>) -> Self {
        Self { api }
    }

    // --- projects ---

    pub(crate) fn create_project(&self, name: &str) -> Result<ProjectMeta, QueryError> {
        let mut engine = self.api.write();
        if get_project(&engine, name)?.is_some() {
            return Err(already_exists(ResourceKind::Project, name));
        }
        let meta = ProjectMeta {
            id: ProjectId::new(),
            name: name.to_string(),
        };
        write_meta(&mut engine, &projects_meta_table(), name.as_bytes(), &meta)?;
        Ok(meta)
    }

    pub(crate) fn open_project(&self, name: &str) -> Result<Option<ProjectMeta>, QueryError> {
        get_project(&self.api.read(), name)
    }

    pub(crate) fn drop_project(&self, name: &str) -> Result<(), QueryError> {
        let mut engine = self.api.write();
        if get_project(&engine, name)?.is_none() {
            return Err(not_found(ResourceKind::Project, name));
        }
        remove_meta(&mut engine, &projects_meta_table(), name.as_bytes())
    }

    pub(crate) fn list_projects(&self) -> Result<Vec<ProjectMeta>, QueryError> {
        list_metas(&self.api.read(), projects_meta_table(), &[])
    }

    // --- datasets (scoped to a project) ---

    pub(crate) fn create_dataset(
        &self,
        project_id: ProjectId,
        name: &str,
    ) -> Result<DatasetMeta, QueryError> {
        let mut engine = self.api.write();
        create_dataset(&mut engine, project_id, name)
    }

    pub(crate) fn open_dataset(
        &self,
        project_id: ProjectId,
        name: &str,
    ) -> Result<Option<DatasetMeta>, QueryError> {
        get_dataset(&self.api.read(), project_id, name)
    }

    pub(crate) fn drop_dataset(&self, project_id: ProjectId, name: &str) -> Result<(), QueryError> {
        let mut engine = self.api.write();
        if get_dataset(&engine, project_id, name)?.is_none() {
            return Err(not_found(ResourceKind::Dataset, name));
        }
        remove_meta(
            &mut engine,
            &datasets_meta_table(),
            &dataset_key(project_id, name),
        )
    }

    pub(crate) fn list_datasets(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<DatasetMeta>, QueryError> {
        list_metas(
            &self.api.read(),
            datasets_meta_table(),
            project_id.as_bytes(),
        )
    }

    // --- tables (scoped to a project + dataset) ---

    pub(crate) fn create_table(
        &self,
        project_id: ProjectId,
        dataset_id: DatasetId,
        name: &str,
        schema: Schema,
    ) -> Result<TableMeta, QueryError> {
        let mut engine = self.api.write();
        create_table(&mut engine, project_id, dataset_id, name, schema)
    }

    pub(crate) fn open_table(
        &self,
        project_id: ProjectId,
        dataset_id: DatasetId,
        name: &str,
    ) -> Result<Option<TableMeta>, QueryError> {
        get_table(&self.api.read(), project_id, dataset_id, name)
    }

    pub(crate) fn drop_table(
        &self,
        project_id: ProjectId,
        dataset_id: DatasetId,
        name: &str,
    ) -> Result<(), QueryError> {
        let mut engine = self.api.write();
        // Drop every incarnation so the name stops resolving entirely;
        // their data rows become dead prefixes for a future GC pass.
        let incarnations = table_incarnations(&engine, project_id, dataset_id, name)?;
        if incarnations.is_empty() {
            return Err(not_found(ResourceKind::Table, name));
        }
        for meta in incarnations {
            remove_meta(
                &mut engine,
                &tables_meta_table(),
                &table_key(project_id, dataset_id, name, meta.truncation_id),
            )?;
        }
        Ok(())
    }

    /// One row per *live* table — the highest-truncation incarnation of
    /// each name. Retained older incarnations are collapsed away.
    pub(crate) fn list_tables(
        &self,
        project_id: ProjectId,
        dataset_id: DatasetId,
    ) -> Result<Vec<TableMeta>, QueryError> {
        let metas: Vec<TableMeta> = list_metas(
            &self.api.read(),
            tables_meta_table(),
            &dataset_scope(project_id, dataset_id),
        )?;
        Ok(live_incarnations(metas))
    }

    // --- column statistics ---

    /// Store one column-statistics row, overwriting any existing row for the
    /// same `(schema, table, column)`. Test-only injection point — when
    /// `ANALYZE` lands it will own the real write path.
    #[cfg(test)]
    pub(crate) fn put_column_stats(&self, stats: &ColumnStats) -> Result<(), QueryError> {
        let mut engine = self.api.write();
        write_meta(&mut engine, &stats_meta_table(), &stats_key(stats), stats)
    }
}

fn already_exists(kind: ResourceKind, name: &str) -> QueryError {
    CatalogError::AlreadyExists {
        kind,
        name: name.to_string(),
    }
    .into()
}

fn not_found(kind: ResourceKind, name: &str) -> QueryError {
    CatalogError::NotFound {
        kind,
        name: name.to_string(),
    }
    .into()
}

fn truncation_exhausted(name: &str) -> QueryError {
    CatalogError::TruncationExhausted {
        name: name.to_string(),
    }
    .into()
}
