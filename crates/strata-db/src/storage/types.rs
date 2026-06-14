//! Logical types and runtime values for the SQL engine.
//!
//! This module is the schema-level vocabulary — *what* types exist and
//! how they're represented in memory. *How* they go on the wire lives
//! in [`crate::codec`]; how they're composed into table shapes lives in
//! [`crate::schema`].
//!
//! Supported types (v1):
//!
//! | Logical | Backing Rust | SQL              |
//! |---------|--------------|------------------|
//! | `Bool`  | `bool`       | `BOOLEAN`        |
//! | `Int16` | `i16`        | `SMALLINT`       |
//! | `Int32` | `i32`        | `INT` / `INTEGER`|
//! | `Int64` | `i64`        | `BIGINT`         |
//! | `Text`  | `String`     | `TEXT`           |
//! | `Bytes` | `Vec<u8>`    | `BYTEA`          |
//! | `Json`  | `serde_json` | `JSON` / `JSONB` |
//! | `Date`  | `i32`        | `DATE`           |
//! | `Timestamp` | `i64`    | `TIMESTAMP WITH TIME ZONE` |
//! | `Float32` | `f32`      | `REAL` / `FLOAT4` |
//! | `Float64` | `f64`      | `DOUBLE PRECISION` / `FLOAT8` / `FLOAT` |
//! | `Numeric` | `Decimal`  | `NUMERIC` / `DECIMAL` |
//! | `Time`  | `i64`        | `TIME` (without time zone) |
//! | `Uuid`  | `uuid::Uuid` | `UUID` |
//! | `Interval` | [`Interval`] | `INTERVAL` |
//!
//! `Interval` keeps Postgres's three components — months, days, and
//! microseconds — separately (a month isn't a fixed number of days), so
//! `'1 mon'` and `'30 days'` store and display distinctly. They compare
//! *equal* via [`Interval::to_micros`] (30-day months, 24-h days), which
//! is what the order-preserving key encodes and what SQL `=` uses.
//!
//! `Numeric` is exact decimal (backed by `rust_decimal`); its
//! order-preserving key encoding lives in [`crate::storage::codec`].
//!
//! `Date` is a count of days since the Unix epoch (`1970-01-01`, UTC) —
//! no time, no timezone. `Timestamp` is an absolute instant: microseconds
//! since that same epoch, UTC. They share `i32`/`i64`'s byte encodings;
//! the calendar conversions live in [`crate::storage::temporal`].
//!
//! Each value has two byte encodings, dispatched on `Value` and backed
//! by the two codec traits in [`crate::storage::codec`]:
//!
//! - **As a column value** ([`Value::encode`](crate::storage::codec)
//!   via `ValueCodec`): variable-length types (`Text`, `Bytes`, `Json`)
//!   carry a `u32` length prefix so the schema can decode columns
//!   positionally.
//! - **As a storage user-key** ([`Value::encode_key`](crate::storage::codec)
//!   via `KeyCodec`): no length prefix — raw bytes — so the engine's
//!   lex sort matches content sort. Required for prefix and range scans
//!   to behave.

// No longer `Copy` — `Array` holds a boxed element type. Pass by
// reference or clone where a `LogicalType` is needed.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LogicalType {
    Bool,
    Int16,
    Int32,
    Int64,
    Text,
    Bytes,
    Json,
    Date,
    Timestamp,
    Float32,
    Float64,
    Numeric,
    Time,
    Uuid,
    Interval,
    /// A one-dimensional array of `element` (no nesting in v1).
    Array(Box<LogicalType>),
}

/// A SQL `INTERVAL`, stored as Postgres's three independent components.
/// Comparison and the order-preserving key use [`Interval::to_micros`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

impl Interval {
    /// Normalize to a single microsecond count for comparison/ordering,
    /// using 30-day months and 24-hour days (matching Postgres). `i128`
    /// because a full `i32` month count overflows `i64` micros.
    pub fn to_micros(self) -> i128 {
        const DAY: i128 = 86_400_000_000;
        self.months as i128 * 30 * DAY + self.days as i128 * DAY + self.micros as i128
    }
}

/// A single runtime datum carrying both its type tag and the data.
/// `Null` is in-band; nullability is a property of the column, not the
/// value, but at runtime a null cell still needs a representation.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Text(String),
    Bytes(Vec<u8>),
    Json(serde_json::Value),
    /// Days since the Unix epoch (`1970-01-01`, UTC). See
    /// [`crate::storage::temporal`] for the calendar conversions.
    Date(i32),
    /// Microseconds since the Unix epoch, UTC — an absolute instant
    /// (`TIMESTAMP WITH TIME ZONE`). See [`crate::storage::temporal`].
    Timestamp(i64),
    /// IEEE-754 single — `REAL` / `FLOAT4` (4 bytes).
    Float32(f32),
    /// IEEE-754 double — `DOUBLE PRECISION` / `FLOAT8` / `FLOAT` (8 bytes).
    /// Inexact; for exact decimals use `Numeric`.
    Float64(f64),
    /// Exact decimal — `NUMERIC` / `DECIMAL`. See
    /// [`crate::storage::codec`] for the order-preserving key encoding.
    Numeric(rust_decimal::Decimal),
    /// Microseconds since midnight — `TIME` (without time zone).
    Time(i64),
    /// A `UUID` (16 bytes); orders by byte value, like Postgres.
    Uuid(uuid::Uuid),
    /// An `INTERVAL` — months / days / microseconds (see [`Interval`]).
    Interval(Interval),
    /// A one-dimensional array. Elements share the column's element type;
    /// `NULL` elements aren't supported in v1.
    Array(Vec<Value>),
}

/// An ordered row of [`Value`]s. The schema that interprets a tuple is
/// held by the caller (table, operator, etc.) — the tuple itself does
/// not carry its schema.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Tuple {
    pub values: Vec<Value>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FieldName(String);

impl FieldName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A named, typed slot in a [`crate::schema::Schema`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Field {
    pub name: FieldName,
    pub ty: LogicalType,
    pub nullable: bool,
    /// `DEFAULT` expression, as JSON-serialized `query::expression::Expr`.
    ///
    /// Stored as an opaque string so this storage type stays free of any
    /// query-layer dependency — only the binder serializes it (CREATE
    /// TABLE) and deserializes it (INSERT). `None` means no default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

impl Field {
    /// Create a nullable field of the given type (matches SQL's default
    /// — `CREATE TABLE foo (x INT)` is nullable unless `NOT NULL`).
    pub fn new(name: impl Into<String>, ty: LogicalType) -> Self {
        Self {
            name: FieldName::new(name),
            ty,
            nullable: true,
            default: None,
        }
    }
}
