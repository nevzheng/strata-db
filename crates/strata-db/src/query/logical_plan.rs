//! Logical plans.
//!
//! A [`LogicalPlan`] is a tree of [`LogicalNode`]s expressing query
//! semantics — *what* the query computes — without committing to an
//! algorithm. The binder produces it from the AST; a lowering pass
//! turns it into a [`crate::query::PhysicalPlan`] by picking concrete
//! operators (e.g. `Scan` → `SeqScan`).
//!
//! Same shape, different layer: this enum mirrors
//! [`crate::query::PlanNode`] one-for-one for the operators we support
//! today. The payoff for the split lands later, when a single logical
//! `Scan` could become a `SeqScan` or an `IndexScan`, and rewrite rules
//! can transform the logical tree without caring about access methods.

use crate::catalog::ids::{DatasetId, ProjectId};
use crate::catalog::schema::Schema;
use crate::catalog::tables::Table;
use crate::storage::types::Tuple;

use super::expression::Expr;

/// A logical plan: the root of an operator tree, plus future
/// plan-level metadata (output schema, estimated cardinality, …).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LogicalPlan {
    pub root: LogicalNode,
}

impl LogicalPlan {
    pub fn new(root: LogicalNode) -> Self {
        Self { root }
    }
}

/// Each variant describes *what* the operator does; algorithm choice
/// happens during lowering.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum LogicalNode {
    /// Algorithm choice (sequential vs index) is deferred to lowering.
    Scan { table: Table },
    /// `NULL` predicate drops the row, matching SQL `WHERE`.
    Filter {
        input: Box<LogicalNode>,
        predicate: Expr,
    },
    Project {
        input: Box<LogicalNode>,
        expressions: Vec<Expr>,
    },
    Limit {
        input: Box<LogicalNode>,
        count: usize,
    },
    /// Skip the first `count` rows of `input` (SQL `OFFSET`).
    Offset {
        input: Box<LogicalNode>,
        count: usize,
    },
    /// Source node for `INSERT INTO t VALUES (..)`.
    Values { rows: Vec<Tuple> },
    Insert {
        table: Table,
        input: Box<LogicalNode>,
    },
    /// Deletes by the tuple's primary-key column (column 0 by convention).
    Delete {
        table: Table,
        input: Box<LogicalNode>,
    },
    /// DDL sink for `CREATE TABLE`. The binder resolves the parent
    /// project + dataset to ids at bind time; the table itself is
    /// minted in the catalog at execution. No input — it produces no
    /// rows, only a side effect.
    CreateTable {
        project_id: ProjectId,
        dataset_id: DatasetId,
        name: String,
        schema: Schema,
        /// `CREATE OR REPLACE` — replace any existing table (bumping its
        /// truncation id) instead of erroring on conflict.
        or_replace: bool,
    },
    /// DDL sink for `CREATE SCHEMA` — creates a dataset under a resolved
    /// project (BigQuery models a schema as a dataset).
    CreateDataset {
        project_id: ProjectId,
        name: String,
        /// `IF NOT EXISTS` — succeed silently if the dataset already exists.
        if_not_exists: bool,
    },
}
