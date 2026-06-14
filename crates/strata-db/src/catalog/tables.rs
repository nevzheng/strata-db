//! Table descriptors: ids, name, schema.
//!
//! Data access lives on [`crate::QueryContext`] — `Table` is what you
//! pass to a context method to say *which* table the op addresses.

use crate::catalog::ids::{DatasetId, ProjectId, TableId, TruncationId};
use crate::catalog::index::Index;
use crate::catalog::schema::Schema;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Table {
    project_id: ProjectId,
    dataset_id: DatasetId,
    id: TableId,
    /// Which incarnation this handle addresses. Resolved to the live
    /// (largest) one by the catalog; the table API folds it into the
    /// row-key prefix so reads/writes only ever touch this incarnation.
    truncation_id: TruncationId,
    name: String,
    schema: Schema,
    /// Columns the table is physically ordered by — its row key. A scan
    /// yields rows in this order for free (no `Sort`). Today the leftmost
    /// column (column 0); empty for a zero-column table. Derived, not stored.
    clustered_key: Vec<usize>,
    /// Secondary indexes recorded for this table (empty until `CREATE INDEX`).
    indexes: Vec<Index>,
}

impl Table {
    pub(crate) fn new(
        project_id: ProjectId,
        dataset_id: DatasetId,
        id: TableId,
        truncation_id: TruncationId,
        name: String,
        schema: Schema,
    ) -> Self {
        // The leftmost column is the storage row key (see `table_api`), so a
        // scan is clustered on it.
        let clustered_key = if schema.fields.is_empty() {
            Vec::new()
        } else {
            vec![0]
        };
        Self {
            project_id,
            dataset_id,
            id,
            truncation_id,
            name,
            schema,
            clustered_key,
            indexes: Vec::new(),
        }
    }

    /// Attach the table's secondary indexes (from catalog metadata).
    pub(crate) fn with_indexes(mut self, indexes: Vec<Index>) -> Self {
        self.indexes = indexes;
        self
    }

    pub fn id(&self) -> TableId {
        self.id
    }

    pub fn truncation_id(&self) -> TruncationId {
        self.truncation_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    pub fn dataset_id(&self) -> DatasetId {
        self.dataset_id
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The columns the table's rows are physically ordered by (the row key).
    pub fn clustered_key(&self) -> &[usize] {
        &self.clustered_key
    }

    /// The table's secondary indexes.
    pub fn indexes(&self) -> &[Index] {
        &self.indexes
    }

    /// Whether a plain scan already produces rows ordered by `columns` — i.e.
    /// the clustered key starts with `columns`. Lets the optimizer skip a
    /// `Sort` (e.g. for a sort-merge join on the key).
    pub fn clustered_on(&self, columns: &[usize]) -> bool {
        !columns.is_empty() && self.clustered_key.starts_with(columns)
    }

    /// A secondary index that produces rows ordered by `columns` (its key
    /// starts with `columns`), if any.
    pub fn index_on(&self, columns: &[usize]) -> Option<&Index> {
        if columns.is_empty() {
            return None;
        }
        self.indexes.iter().find(|i| i.columns.starts_with(columns))
    }
}
