//! Reserved identifiers and constants for the system namespace.
//!
//! # Reservation scheme
//!
//! UUIDs whose first 48 bits (the leading `ffffffff-ffff` block) are all
//! ones are reserved for system use. UUIDv7 encodes a unix-millisecond
//! timestamp in those first 48 bits, so a minted v7 ID can never collide
//! with a reserved ID until roughly the year 10895 — well past any
//! reasonable lifetime of this database.
//!
//! Within the reserved space, the remaining 80 bits distinguish system
//! entities. Different newtype levels ([`ProjectId`], [`DatasetId`],
//! [`TableId`]) can reuse the same byte pattern without colliding because
//! they are distinct types — there is no way to confuse a `ProjectId` for
//! a `TableId` at a call site.
//!
//! # Layout
//!
//! ```text
//! Reserved prefix:    ffffffff-ffff
//!
//! System project:     ffffffff-ffff-0000-0000-000000000000   (ProjectId)
//! Catalog dataset:    ffffffff-ffff-0000-0000-000000000000   (DatasetId)
//!
//! System tables (TableId, within the catalog dataset):
//!   _uuids:           ffffffff-ffff-0000-0000-000000000000   name -> UUID
//!   _projects:        ffffffff-ffff-0000-0000-000000000001   ProjectId -> ProjectMeta
//!   _datasets:        ffffffff-ffff-0000-0000-000000000002   (ProjectId, DatasetId) -> DatasetMeta
//!   _tables:          ffffffff-ffff-0000-0000-000000000003   (ProjectId, DatasetId, TableId) -> TableMeta
//!   _queries:         ffffffff-ffff-0000-0000-000000000004   (ProjectId, QueryId) -> QueryMeta
//! ```
//!
//! Future system tables should pick the next available suffix
//! (`...000000000005`, `...000000000006`, ...) so they remain distinct
//! [`TableId`]s within the catalog dataset.
//!
//! # Roles
//!
//! - `_uuids` is the universal reverse index from a (kind, scope, name)
//!   tuple to a UUID. It powers `db.project("acme")` and friends — given
//!   a name, find the ID.
//! - `_projects` / `_datasets` / `_tables` are the forward indices keyed
//!   by the natural id hierarchy. They store the canonical `*Meta` blob
//!   and let us list children of a parent via prefix scan
//!   (e.g. all datasets in a project).

// The meta-builder fns below are scaffolding for the catalog bootstrap.
// They'll be wired into the catalog bodies on the next pass.
#![allow(dead_code)]

use uuid::uuid;

use crate::catalog::ids::{DatasetId, ProjectId, TableId, TruncationId};

// --- Default namespace ---
//
// Seeded by [`crate::Db`] on open so SQL has a namespace to reference
// out of the box, and so `CREATE SCHEMA <dataset>` (no project segment)
// resolves its project here — mirroring BigQuery's "defaults to the
// project that runs this DDL statement".

/// Default project, seeded on open and used when a `CREATE SCHEMA` name
/// omits the project segment.
pub const DEFAULT_PROJECT_NAME: &str = "strata";

/// Default dataset, seeded under [`DEFAULT_PROJECT_NAME`].
pub const DEFAULT_DATASET_NAME: &str = "public";
use crate::catalog::schema::Schema;
use crate::catalog::{DatasetMeta, ProjectMeta, TableMeta};
use crate::storage::types::{Field, LogicalType};

/// Schema for every system table: a `Bytes` PK column carrying the
/// composite natural key (project name, `(project_id, dataset_name)`,
/// etc.) plus a `Json` blob with the serialized metadata. With this
/// shape catalog reads and writes flow through the regular table API.
pub(crate) fn system_table_schema() -> Schema {
    Schema {
        fields: vec![
            Field::new("pk", LogicalType::Bytes),
            Field::new("meta", LogicalType::Json),
        ],
    }
}

// --- System project ---

/// Display name of the reserved system project.
pub const SYSTEM_PROJECT_NAME: &str = "_system";

/// Reserved [`ProjectId`] for the system project.
pub const SYSTEM_PROJECT_ID: ProjectId = ProjectId(uuid!("ffffffff-ffff-0000-0000-000000000000"));

// --- Catalog dataset (within the system project) ---

/// Display name of the reserved catalog dataset.
pub const CATALOG_DATASET_NAME: &str = "_catalog";

/// Reserved [`DatasetId`] for the catalog dataset.
pub const CATALOG_DATASET_ID: DatasetId = DatasetId(uuid!("ffffffff-ffff-0000-0000-000000000000"));

// --- System tables (within the catalog dataset) ---

/// Display name of the table mapping resource names to UUIDs.
pub const UUID_TABLE_NAME: &str = "_uuids";

/// Reserved [`TableId`] for the name → UUID table.
pub const UUID_TABLE_ID: TableId = TableId(uuid!("ffffffff-ffff-0000-0000-000000000000"));

/// Display name of the table storing [`ProjectMeta`] rows keyed by `ProjectId`.
pub const PROJECTS_TABLE_NAME: &str = "_projects";

/// Reserved [`TableId`] for the projects metadata table.
pub const PROJECTS_TABLE_ID: TableId = TableId(uuid!("ffffffff-ffff-0000-0000-000000000001"));

/// Display name of the table storing [`DatasetMeta`] rows keyed by
/// `(ProjectId, DatasetId)`.
pub const DATASETS_TABLE_NAME: &str = "_datasets";

/// Reserved [`TableId`] for the datasets metadata table.
pub const DATASETS_TABLE_ID: TableId = TableId(uuid!("ffffffff-ffff-0000-0000-000000000002"));

/// Display name of the table storing [`TableMeta`] rows keyed by
/// `(ProjectId, DatasetId, TableId)`.
pub const TABLES_TABLE_NAME: &str = "_tables";

/// Reserved [`TableId`] for the tables metadata table.
pub const TABLES_TABLE_ID: TableId = TableId(uuid!("ffffffff-ffff-0000-0000-000000000003"));

/// Display name of the table storing `QueryMeta` rows keyed by
/// `(ProjectId, QueryId)`. One row per query executed, scoped to its
/// project.
pub const QUERIES_TABLE_NAME: &str = "_queries";

/// Reserved [`TableId`] for the queries metadata table.
pub const QUERIES_TABLE_ID: TableId = TableId(uuid!("ffffffff-ffff-0000-0000-000000000004"));

// --- Pre-built metas ---
//
// These can't be `const` because `*Meta` carries an owned `String` name.
// They're cheap to construct on demand.

pub(crate) fn system_project_meta() -> ProjectMeta {
    ProjectMeta {
        id: SYSTEM_PROJECT_ID,
        name: SYSTEM_PROJECT_NAME.to_string(),
    }
}

pub(crate) fn catalog_dataset_meta() -> DatasetMeta {
    DatasetMeta {
        id: CATALOG_DATASET_ID,
        name: CATALOG_DATASET_NAME.to_string(),
    }
}

pub(crate) fn uuid_table_meta() -> TableMeta {
    TableMeta {
        id: UUID_TABLE_ID,
        name: UUID_TABLE_NAME.to_string(),
        truncation_id: TruncationId::INITIAL,
        schema: system_table_schema(),
    }
}

pub(crate) fn projects_table_meta() -> TableMeta {
    TableMeta {
        id: PROJECTS_TABLE_ID,
        name: PROJECTS_TABLE_NAME.to_string(),
        truncation_id: TruncationId::INITIAL,
        schema: system_table_schema(),
    }
}

pub(crate) fn datasets_table_meta() -> TableMeta {
    TableMeta {
        id: DATASETS_TABLE_ID,
        name: DATASETS_TABLE_NAME.to_string(),
        truncation_id: TruncationId::INITIAL,
        schema: system_table_schema(),
    }
}

pub(crate) fn tables_table_meta() -> TableMeta {
    TableMeta {
        id: TABLES_TABLE_ID,
        name: TABLES_TABLE_NAME.to_string(),
        truncation_id: TruncationId::INITIAL,
        schema: system_table_schema(),
    }
}

pub(crate) fn queries_table_meta() -> TableMeta {
    TableMeta {
        id: QUERIES_TABLE_ID,
        name: QUERIES_TABLE_NAME.to_string(),
        truncation_id: TruncationId::INITIAL,
        schema: system_table_schema(),
    }
}
