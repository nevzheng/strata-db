//! Table descriptors: ids, name, schema.
//!
//! Data access lives on [`crate::QueryContext`] — `Table` is what you
//! pass to a context method to say *which* table the op addresses.

use crate::catalog::ids::{DatasetId, ProjectId, TableId, TruncationId};
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
        Self {
            project_id,
            dataset_id,
            id,
            truncation_id,
            name,
            schema,
        }
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
}
