//! Logical types and runtime values for the SQL engine.
//!
//! This module is the schema-level vocabulary — *what* types exist and
//! how they're represented in memory. *How* they go on the wire lives
//! in [`crate::codec`]; how they're composed into table shapes lives in
//! [`crate::schema`].
//!
//! Supported types (v1):
//!
//! | Logical    | Backing Rust | Notes                                  |
//! |------------|--------------|----------------------------------------|
//! | `Bool`     | `bool`       | 1 byte on the wire                     |
//! | `Int16`    | `i16`        | signed; SQL `SMALLINT`                 |
//! | `Int32`    | `i32`        | signed; SQL `INT` / `INTEGER`          |
//! | `Int64`    | `i64`        | signed; SQL `BIGINT`                   |
//! | `Text`     | `String`     | UTF-8, length-prefixed; SQL `TEXT`     |
//! | `Json`     | `serde_json` | arbitrary JSON blob, serde-encoded     |
//!
//! `Json` is the escape hatch for data that doesn't fit a primitive
//! (today: catalog metadata blobs). Unsigned integers, floating-point,
//! decimals, timestamps, and composite types (arrays, structs) are
//! intentionally out of scope for v1 — add a variant + a `Codec` impl
//! when the need is real.

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum LogicalType {
    Bool,
    Int16,
    Int32,
    Int64,
    Text,
    Json,
}

/// A single runtime datum carrying both its type tag and the data.
/// `Null` is in-band; nullability is a property of the column, not the
/// value, but at runtime a null cell still needs a representation.
#[derive(Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Text(String),
    Json(serde_json::Value),
}

/// An ordered row of [`Value`]s. The schema that interprets a tuple is
/// held by the caller (table, operator, etc.) — the tuple itself does
/// not carry its schema.
#[derive(Debug, PartialEq)]
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
}

impl Field {
    /// Create a nullable field of the given type (matches SQL's default
    /// — `CREATE TABLE foo (x INT)` is nullable unless `NOT NULL`).
    pub fn new(name: impl Into<String>, ty: LogicalType) -> Self {
        Self {
            name: FieldName::new(name),
            ty,
            nullable: true,
        }
    }
}
