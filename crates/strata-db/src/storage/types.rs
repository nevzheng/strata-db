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

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum LogicalType {
    Bool,
    Int16,
    Int32,
    Int64,
    Text,
    Bytes,
    Json,
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
