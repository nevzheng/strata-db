//! System catalog: virtual `information_schema.*` and `pg_catalog.*`
//! (plus `st_*`) relations that ORMs and SQL tools introspect on connect.
//!
//! These relations are **not stored**. The canonical metadata already lives
//! in the `_projects` / `_datasets` / `_tables` system tables; a scan of a
//! system relation enumerates that metadata and projects the columns at read
//! time (always consistent, no second copy). `pg_type` / `st_type` is the one
//! static table — our fixed type set with canonical Postgres type OIDs.
//!
//! Naming: `st_*` are the canonical Strata relations; the `pg_*` names are
//! 1:1 aliases over the same generator so literal ORM queries resolve.

use uuid::Uuid;

use crate::catalog::CatalogReader;
use crate::catalog::consts::SYSTEM_PROJECT_ID;
use crate::catalog::schema::Schema;
use crate::query::QueryError;
use crate::storage::types::{Field, LogicalType, Tuple, Value};

// --- Type → Postgres OID table ---------------------------------------------

/// Postgres-compatible facts about one of our logical types. The `oid` is
/// the canonical PG type OID, so the wire `RowDescription`, `pg_type`, and
/// ORM type lookups all agree.
pub(crate) struct PgType {
    pub oid: i64,
    /// `pg_type.typname` / `udt_name` (e.g. `int4`, `text`).
    pub name: &'static str,
    /// `information_schema.columns.data_type` (e.g. `integer`).
    pub display: &'static str,
    /// Byte length, or -1 for a variable-length type.
    pub len: i64,
    /// `pg_type.typcategory`.
    pub category: &'static str,
    /// Array element type OID, or 0 for a non-array.
    pub elem: i64,
}

/// Map a logical type to its Postgres type facts.
pub(crate) fn pg_type(ty: &LogicalType) -> PgType {
    let base = |oid, name, display, len, category| PgType {
        oid,
        name,
        display,
        len,
        category,
        elem: 0,
    };
    match ty {
        LogicalType::Bool => base(16, "bool", "boolean", 1, "B"),
        LogicalType::Bytes => base(17, "bytea", "bytea", -1, "U"),
        LogicalType::Int64 => base(20, "int8", "bigint", 8, "N"),
        LogicalType::Int16 => base(21, "int2", "smallint", 2, "N"),
        LogicalType::Int32 => base(23, "int4", "integer", 4, "N"),
        LogicalType::Text => base(25, "text", "text", -1, "S"),
        LogicalType::Json => base(114, "json", "json", -1, "U"),
        LogicalType::Float32 => base(700, "float4", "real", 4, "N"),
        LogicalType::Float64 => base(701, "float8", "double precision", 8, "N"),
        LogicalType::Date => base(1082, "date", "date", 4, "D"),
        LogicalType::Time => base(1083, "time", "time without time zone", 8, "D"),
        LogicalType::Timestamp => base(1184, "timestamptz", "timestamp with time zone", 8, "D"),
        LogicalType::Interval => base(1186, "interval", "interval", 16, "T"),
        LogicalType::Numeric => base(1700, "numeric", "numeric", -1, "N"),
        LogicalType::Uuid => base(2950, "uuid", "uuid", 16, "U"),
        LogicalType::Array(elem) => {
            let inner = pg_type(elem);
            PgType {
                oid: array_oid(inner.oid),
                name: "array",
                display: "ARRAY",
                len: -1,
                category: "A",
                elem: inner.oid,
            }
        }
    }
}

/// The canonical PG array-type OID for an element type OID (the `_`-prefixed
/// types). Falls back to `anyarray` (2277) for an element we don't map.
fn array_oid(elem_oid: i64) -> i64 {
    match elem_oid {
        16 => 1000,   // _bool
        17 => 1001,   // _bytea
        20 => 1016,   // _int8
        21 => 1005,   // _int2
        23 => 1007,   // _int4
        25 => 1009,   // _text
        700 => 1021,  // _float4
        701 => 1022,  // _float8
        1082 => 1182, // _date
        1083 => 1183, // _time
        1184 => 1185, // _timestamptz
        1700 => 1231, // _numeric
        2950 => 2951, // _uuid
        _ => 2277,    // anyarray
    }
}

/// The base types we advertise in `pg_type` / `st_type`, in OID order.
const BASE_TYPES: &[LogicalType] = &[
    LogicalType::Bool,
    LogicalType::Bytes,
    LogicalType::Int64,
    LogicalType::Int16,
    LogicalType::Int32,
    LogicalType::Text,
    LogicalType::Json,
    LogicalType::Float32,
    LogicalType::Float64,
    LogicalType::Date,
    LogicalType::Time,
    LogicalType::Timestamp,
    LogicalType::Interval,
    LogicalType::Numeric,
    LogicalType::Uuid,
];

/// Derive a stable u32-range OID for a catalog object from its UUID. The low
/// 31 bits keep it positive; collisions are astronomically unlikely at our
/// scale (revisit with a catalog sequence if that changes).
fn oid_of(uuid: Uuid) -> i64 {
    (uuid.as_u128() as u64 & 0x7fff_ffff) as i64
}

// --- Column statistics ------------------------------------------------------

/// One stored `pg_stats` row — per-column statistics. Unlike the other
/// catalog relations, these aren't derivable from metadata: they're computed
/// by `ANALYZE` (not yet built) and stored. Denormalized with the
/// schema/table/column names so a stats row is self-contained.
///
/// The array-valued columns (`most_common_vals`, `histogram_bounds`) are
/// `anyarray` in Postgres; we keep them as the already-rendered text form.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ColumnStats {
    pub schemaname: String,
    pub tablename: String,
    pub attname: String,
    pub null_frac: f32,
    pub avg_width: i32,
    pub n_distinct: f32,
    pub most_common_vals: String,
    pub most_common_freqs: String,
    pub histogram_bounds: String,
    pub correlation: f32,
}

impl ColumnStats {
    /// Reshape into a `pg_stats` tuple (column order matches
    /// [`SystemRelation::schema`] for `Stats`).
    fn to_row(&self) -> Tuple {
        row([
            text(&self.schemaname),
            text(&self.tablename),
            text(&self.attname),
            Value::Float32(self.null_frac),
            Value::Int32(self.avg_width),
            Value::Float32(self.n_distinct),
            text(&self.most_common_vals),
            text(&self.most_common_freqs),
            text(&self.histogram_bounds),
            Value::Float32(self.correlation),
        ])
    }
}

// --- System relations ------------------------------------------------------

/// A virtual catalog relation. The `pg_*` and `st_*` spellings of the same
/// concept resolve to one variant; `information_schema.*` are the standard
/// views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SystemRelation {
    InfoSchemata,
    InfoTables,
    InfoColumns,
    Database,
    Namespace,
    Class,
    Attribute,
    Type,
    Stats,
}

impl SystemRelation {
    /// Resolve a `(dataset, table)` name to a system relation, if it is one.
    /// `dataset` is `None` for an unqualified name (`SELECT … FROM pg_class`).
    ///
    /// `information_schema.*` names require the explicit `information_schema`
    /// dataset (they're common words). `pg_*` / `st_*` resolve either
    /// unqualified or under the `pg_catalog` dataset, matching how clients
    /// send them.
    pub(crate) fn resolve(dataset: Option<&str>, table: &str) -> Option<SystemRelation> {
        use SystemRelation::*;
        let in_info = dataset == Some("information_schema");
        let pg_ok = dataset.is_none() || dataset == Some("pg_catalog");
        match table {
            "schemata" if in_info => Some(InfoSchemata),
            "tables" if in_info => Some(InfoTables),
            "columns" if in_info => Some(InfoColumns),
            "pg_database" | "st_database" if pg_ok => Some(Database),
            "pg_namespace" | "st_namespace" if pg_ok => Some(Namespace),
            "pg_class" | "st_class" if pg_ok => Some(Class),
            "pg_attribute" | "st_attribute" if pg_ok => Some(Attribute),
            "pg_type" | "st_type" if pg_ok => Some(Type),
            "pg_stats" | "st_stats" if pg_ok => Some(Stats),
            _ => None,
        }
    }

    /// The output schema (column names + logical types).
    pub(crate) fn schema(&self) -> Schema {
        use LogicalType::{Bool, Float32, Int32, Int64, Text};
        let f = |name: &str, ty: LogicalType| Field::new(name, ty);
        let fields = match self {
            SystemRelation::InfoSchemata => vec![
                f("catalog_name", Text),
                f("schema_name", Text),
                f("schema_owner", Text),
            ],
            SystemRelation::InfoTables => vec![
                f("table_catalog", Text),
                f("table_schema", Text),
                f("table_name", Text),
                f("table_type", Text),
            ],
            SystemRelation::InfoColumns => vec![
                f("table_catalog", Text),
                f("table_schema", Text),
                f("table_name", Text),
                f("column_name", Text),
                f("ordinal_position", Int32),
                f("column_default", Text),
                f("is_nullable", Text),
                f("data_type", Text),
                f("udt_name", Text),
            ],
            SystemRelation::Database => vec![f("oid", Int64), f("datname", Text)],
            SystemRelation::Namespace => {
                vec![f("oid", Int64), f("nspname", Text), f("nspowner", Text)]
            }
            SystemRelation::Class => vec![
                f("oid", Int64),
                f("relname", Text),
                f("relnamespace", Int64),
                f("relkind", Text),
                f("relnatts", Int32),
                f("relhasindex", Bool),
                f("reltuples", Int64),
            ],
            SystemRelation::Attribute => vec![
                f("attrelid", Int64),
                f("attname", Text),
                f("atttypid", Int64),
                f("attnum", Int32),
                f("attnotnull", Bool),
                f("atthasdef", Bool),
                f("attisdropped", Bool),
            ],
            SystemRelation::Type => vec![
                f("oid", Int64),
                f("typname", Text),
                f("typlen", Int32),
                f("typtype", Text),
                f("typcategory", Text),
                f("typelem", Int64),
            ],
            // pg_stats shape. The array-valued columns (most_common_vals,
            // histogram_bounds) are `anyarray` in Postgres — rendered as
            // text here, since their element type varies per column.
            SystemRelation::Stats => vec![
                f("schemaname", Text),
                f("tablename", Text),
                f("attname", Text),
                f("null_frac", Float32),
                f("avg_width", Int32),
                f("n_distinct", Float32),
                f("most_common_vals", Text),
                f("most_common_freqs", Text),
                f("histogram_bounds", Text),
                f("correlation", Float32),
            ],
        };
        Schema { fields }
    }

    /// Generate the relation's rows by enumerating catalog metadata.
    pub(crate) fn rows(&self, catalog: &CatalogReader) -> Result<Vec<Tuple>, QueryError> {
        match self {
            SystemRelation::Type => Ok(type_rows()),
            // Statistics aren't derivable from metadata — they're computed
            // by ANALYZE (not yet built) and stored. We read whatever rows
            // have been stored; until ANALYZE exists that's empty (matching
            // Postgres, where pg_stats has no row for an un-analyzed column).
            SystemRelation::Stats => Ok(catalog
                .list_column_stats()?
                .iter()
                .map(ColumnStats::to_row)
                .collect()),
            SystemRelation::InfoSchemata => namespaces(catalog, |p, d| {
                row([text(&p.name), text(&d.name), text("strata")])
            }),
            SystemRelation::Database => {
                let mut rows = Vec::new();
                for p in user_projects(catalog)? {
                    rows.push(row([Value::Int64(oid_of(p.id.0)), text(&p.name)]));
                }
                Ok(rows)
            }
            SystemRelation::Namespace => namespaces(catalog, |_, d| {
                row([Value::Int64(oid_of(d.id.0)), text(&d.name), text("strata")])
            }),
            SystemRelation::InfoTables => tables(catalog, |p, d, t| {
                row([
                    text(&p.name),
                    text(&d.name),
                    text(&t.name),
                    text("BASE TABLE"),
                ])
            }),
            SystemRelation::Class => {
                let mut rows = Vec::new();
                for p in user_projects(catalog)? {
                    for d in catalog.list_datasets(p.id)? {
                        for t in catalog.list_tables(p.id, d.id)? {
                            let natts = catalog.list_columns(t.id)?.len();
                            rows.push(row([
                                Value::Int64(oid_of(t.id.0)),
                                text(&t.name),
                                Value::Int64(oid_of(d.id.0)),
                                text("r"),
                                Value::Int32(natts as i32),
                                // relhasindex — reflects real catalog metadata.
                                Value::Bool(!t.indexes.is_empty()),
                                // reltuples = -1: row count unknown (no ANALYZE yet).
                                Value::Int64(-1),
                            ]));
                        }
                    }
                }
                Ok(rows)
            }
            SystemRelation::InfoColumns => columns(catalog, |p, d, t, c| {
                let pg = pg_type(&c.ty);
                row([
                    text(&p.name),
                    text(&d.name),
                    text(&t.name),
                    text(&c.name),
                    Value::Int32((c.position + 1) as i32),
                    Value::Null, // column_default: SQL text not reconstructed yet
                    text(if c.nullable { "YES" } else { "NO" }),
                    text(pg.display),
                    text(pg.name),
                ])
            }),
            SystemRelation::Attribute => columns(catalog, |_, _, t, c| {
                row([
                    Value::Int64(oid_of(t.id.0)),
                    text(&c.name),
                    Value::Int64(pg_type(&c.ty).oid),
                    Value::Int32((c.position + 1) as i32),
                    Value::Bool(!c.nullable),
                    Value::Bool(c.default.is_some()),
                    Value::Bool(false),
                ])
            }),
        }
    }
}

// --- Row builders ----------------------------------------------------------

fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

fn row<const N: usize>(values: [Value; N]) -> Tuple {
    Tuple {
        values: values.into(),
    }
}

fn type_rows() -> Vec<Tuple> {
    BASE_TYPES
        .iter()
        .map(|ty| {
            let pg = pg_type(ty);
            row([
                Value::Int64(pg.oid),
                text(pg.name),
                Value::Int32(pg.len as i32),
                text("b"),
                text(pg.category),
                Value::Int64(pg.elem),
            ])
        })
        .collect()
}

// --- Catalog enumeration ----------------------------------------------------
//
// User-visible objects only — the reserved `_system` project (which holds the
// catalog's own meta tables) is hidden.

type ProjectMeta = crate::catalog::ProjectMeta;
type DatasetMeta = crate::catalog::DatasetMeta;
type TableMeta = crate::catalog::TableMeta;
type ColumnMeta = crate::catalog::ColumnMeta;

fn user_projects(catalog: &CatalogReader) -> Result<Vec<ProjectMeta>, QueryError> {
    Ok(catalog
        .list_projects()?
        .into_iter()
        .filter(|p| p.id != SYSTEM_PROJECT_ID)
        .collect())
}

/// Apply `f` to every `(project, dataset)` pair.
fn namespaces(
    catalog: &CatalogReader,
    f: impl Fn(&ProjectMeta, &DatasetMeta) -> Tuple,
) -> Result<Vec<Tuple>, QueryError> {
    let mut rows = Vec::new();
    for p in user_projects(catalog)? {
        for d in catalog.list_datasets(p.id)? {
            rows.push(f(&p, &d));
        }
    }
    Ok(rows)
}

/// Apply `f` to every `(project, dataset, table)` triple.
fn tables(
    catalog: &CatalogReader,
    f: impl Fn(&ProjectMeta, &DatasetMeta, &TableMeta) -> Tuple,
) -> Result<Vec<Tuple>, QueryError> {
    let mut rows = Vec::new();
    for p in user_projects(catalog)? {
        for d in catalog.list_datasets(p.id)? {
            for t in catalog.list_tables(p.id, d.id)? {
                rows.push(f(&p, &d, &t));
            }
        }
    }
    Ok(rows)
}

/// Apply `f` to every column (`field` + 0-based index) of every table.
fn columns(
    catalog: &CatalogReader,
    f: impl Fn(&ProjectMeta, &DatasetMeta, &TableMeta, &ColumnMeta) -> Tuple,
) -> Result<Vec<Tuple>, QueryError> {
    let mut rows = Vec::new();
    for p in user_projects(catalog)? {
        for d in catalog.list_datasets(p.id)? {
            for t in catalog.list_tables(p.id, d.id)? {
                // Columns come from the normalized `_columns` rows, in ordinal order.
                for c in catalog.list_columns(t.id)? {
                    rows.push(f(&p, &d, &t, &c));
                }
            }
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    //! Faked statistics flowing end-to-end through `pg_stats` / `st_stats`.
    //!
    //! `ANALYZE` (which would compute and store stats) doesn't exist yet, so
    //! this can't be a `.slt` spec — there's no SQL to populate the relation.
    //! Instead we inject `ColumnStats` directly (the same write path a future
    //! ANALYZE will use) and confirm they surface through a real SELECT.

    use super::ColumnStats;
    use crate::Db;
    use crate::query::Planner;
    use crate::query::executor::{ExecuteResult, Executor};
    use crate::query::volcano::Volcano;
    use crate::storage::types::{Tuple, Value};

    /// Plan + execute one SELECT, returning its rows.
    fn query(db: &Db, sql: &str) -> Vec<Tuple> {
        let mut ctx = db.query_context();
        let planner = Planner::builder().build().unwrap();
        let pq = planner.plan(sql, &ctx).unwrap();
        let mut rows = Vec::new();
        for plan in pq.physical {
            if let ExecuteResult::Rows(stream) = Volcano.execute(plan, &mut ctx).unwrap() {
                rows = stream.collect::<Result<_, _>>().unwrap();
            }
        }
        rows
    }

    fn stat(table: &str, col: &str, n_distinct: f32, mcv: &str) -> ColumnStats {
        ColumnStats {
            schemaname: "public".into(),
            tablename: table.into(),
            attname: col.into(),
            null_frac: 0.0,
            avg_width: 8,
            n_distinct,
            most_common_vals: mcv.into(),
            most_common_freqs: String::new(),
            histogram_bounds: String::new(),
            correlation: 0.0,
        }
    }

    #[test]
    fn faked_stats_surface_through_pg_stats() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = Db::open(tmp.path()).unwrap();

        // No ANALYZE yet → the relation is queryable but empty.
        assert!(query(&db, "SELECT attname FROM pg_stats").is_empty());

        // Fake what ANALYZE would store: a low-cardinality skewed column and
        // a unique one. (Exact-in-f32 values so the JSON round-trip is clean.)
        db.put_column_stats(&stat(
            "orders",
            "status",
            4.0,
            "{complete,pending,cancelled,failed}",
        ))
        .unwrap();
        db.put_column_stats(&stat("orders", "amount", -1.0, ""))
            .unwrap();

        // They now surface through a real SELECT. Rows come back in key order
        // (schema\0table\0column): amount before status.
        let rows = query(
            &db,
            "SELECT attname, n_distinct, most_common_vals FROM pg_stats WHERE tablename = 'orders'",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].values[0], Value::Text("amount".into()));
        assert_eq!(rows[0].values[1], Value::Float32(-1.0));
        assert_eq!(rows[1].values[0], Value::Text("status".into()));
        assert_eq!(rows[1].values[1], Value::Float32(4.0));
        assert_eq!(
            rows[1].values[2],
            Value::Text("{complete,pending,cancelled,failed}".into())
        );

        // The st_ alias reads the same store.
        let via_st = query(&db, "SELECT attname FROM st_stats WHERE attname = 'status'");
        assert_eq!(via_st.len(), 1);
        assert_eq!(via_st[0].values[0], Value::Text("status".into()));
    }
}
