//! Index descriptors — access paths recorded in the catalog.
//!
//! An index records the column order a table is navigable in, so the optimizer
//! can recognize an ordering it may exploit (e.g. feeding a sort-merge join
//! without an explicit `Sort`). The *clustered* order — the table's own row-key
//! order, which a scan yields for free — lives on
//! [`Table`](super::tables::Table); these are *secondary* indexes.

/// A secondary index: the table can be produced in order of `columns`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Index {
    pub name: String,
    /// Columns the index is keyed on, in order (positions in the table schema).
    /// Ascending for now; per-column direction/nulls is future work.
    pub columns: Vec<usize>,
}
