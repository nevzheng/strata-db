//! Table schemas.
//!
//! A [`Schema`] describes the typed shape of values written to a table.
//! Each [`Field`] is a named slot with a [`FieldType`]. Schemas are stored
//! alongside the rest of a [`TableMeta`](crate::catalog::TableMeta) blob
//! and travel with the [`Table`](crate::Table) handle so the put path can
//! validate incoming values.
//!
//! An empty schema (`Schema::empty()`) means "no validation, accept any
//! JSON value." System tables use this — their values are catalog
//! metadata, not user-shaped rows.

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Schema {
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Field {
    pub name: String,
    pub ty: FieldType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    String,
    Integer,
    Float,
    Boolean,
    /// Arbitrary JSON — catchall for fields that don't fit a primitive.
    Json,
}

impl Schema {
    pub fn new(fields: Vec<Field>) -> Self {
        Self { fields }
    }

    /// A schema with no fields. Values are accepted without validation.
    pub fn empty() -> Self {
        Self { fields: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

impl Field {
    pub fn new(name: impl Into<String>, ty: FieldType) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }
}
