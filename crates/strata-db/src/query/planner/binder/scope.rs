//! Binding scope — the columns visible at one nesting level.
//!
//! Replaces the bare `Schema` the binder used to push: each visible column
//! now carries its **source relation** (table name or alias), so qualified
//! references (`t.col`) disambiguate columns that share a name, and a join's
//! scope is just `left ++ right` with provenance retained. A column resolves
//! to its *position* here, which is its index in the row at run time.

use crate::catalog::schema::Schema;
use crate::query::QueryError;
use crate::storage::types::Field;

/// One visible column: where it came from, and what it is.
#[derive(Debug, Clone)]
pub(super) struct ScopeColumn {
    /// Source relation — a table name or its `AS` alias. `None` for a column
    /// with no nameable origin (e.g. a `SELECT <expr>` with empty FROM).
    pub relation: Option<String>,
    pub field: Field,
}

/// The columns in scope, in output order.
#[derive(Debug, Clone, Default)]
pub(super) struct Scope {
    pub columns: Vec<ScopeColumn>,
}

impl Scope {
    /// An empty scope (e.g. `SELECT 1` with no FROM).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build a scope for one base relation: every field tagged with `relation`.
    pub fn for_relation(relation: Option<String>, schema: &Schema) -> Self {
        Self {
            columns: schema
                .fields
                .iter()
                .map(|field| ScopeColumn {
                    relation: relation.clone(),
                    field: field.clone(),
                })
                .collect(),
        }
    }

    /// Concatenate `left ++ right` — the column layout of a join's output row.
    pub fn concat(mut left: Scope, right: Scope) -> Scope {
        left.columns.extend(right.columns);
        left
    }

    /// Number of columns (the row arity at this level).
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// The columns as a [`Schema`] — what a materializing operator (sort, join
    /// build side) needs to encode/decode these rows in scratch storage.
    pub fn schema(&self) -> Schema {
        Schema {
            fields: self.columns.iter().map(|c| c.field.clone()).collect(),
        }
    }

    /// Resolve a column reference to its index. `relation` is the optional
    /// qualifier (`t` in `t.col`). Errors if nothing matches, or if the name
    /// is ambiguous across relations.
    pub fn resolve(&self, relation: Option<&str>, name: &str) -> Result<usize, QueryError> {
        let mut found = None;
        for (i, col) in self.columns.iter().enumerate() {
            let name_matches = col.field.name.as_str() == name;
            let relation_matches = match relation {
                Some(r) => col.relation.as_deref() == Some(r),
                None => true,
            };
            if name_matches && relation_matches {
                if found.is_some() {
                    return Err(QueryError::Internal(format!(
                        "ambiguous column: {}",
                        display_name(relation, name)
                    )));
                }
                found = Some(i);
            }
        }
        found.ok_or_else(|| {
            QueryError::Internal(format!("unknown column: {}", display_name(relation, name)))
        })
    }
}

fn display_name(relation: Option<&str>, name: &str) -> String {
    match relation {
        Some(r) => format!("{r}.{name}"),
        None => name.to_string(),
    }
}
