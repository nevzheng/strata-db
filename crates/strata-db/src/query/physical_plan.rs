//! Physical plans.
//!
//! A [`PhysicalPlan`] is a handle to a tree of [`PlanNode`]s with
//! concrete algorithms chosen. The wrapper exists so the plan can later
//! carry properties beyond the tree shape — output schema, estimated
//! cost, sortedness, partitioning — without changing every call site.
//!
//! This is plain data. A backend (Volcano interpreter, vectorized
//! engine, JIT codegen) consumes it and produces tuples; the plan does
//! no work itself.

use crate::catalog::ids::{DatasetId, ProjectId};
use crate::catalog::schema::Schema;
use crate::catalog::system::SystemRelation;
use crate::catalog::tables::Table;
use crate::storage::types::Tuple;

use super::expression::Expr;
use super::logical_plan::{JoinType, SortKey};

/// A physical plan: the root of an operator tree, plus future
/// plan-level metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PhysicalPlan {
    pub root: PlanNode,
}

impl PhysicalPlan {
    pub fn new(root: PlanNode) -> Self {
        Self { root }
    }
}

/// Which physical algorithm realizes a [`Join`](PlanNode::Join). Every
/// algorithm takes the same inputs and predicate, so the choice is a field,
/// not a variant (unlike `SeqScan` vs a future `IndexScan`, which differ in
/// shape). Chosen during lowering; the executor dispatches on it.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum JoinStrategy {
    /// Tuple-at-a-time nested loop — the only algorithm that handles any
    /// predicate (and cross joins). Always correct.
    NestedLoop,
    /// Block nested loop — buffer a batch of the outer, one inner scan per batch.
    BlockNestedLoop,
    /// Sort both inputs on the join key, then merge in one pass. Equi-joins only.
    SortMerge,
}

/// One node of a physical plan tree. Each variant is a concrete
/// operator with the algorithm baked in (e.g. `SeqScan` versus a
/// hypothetical `IndexScan`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum PlanNode {
    /// Read every tuple from a base table.
    SeqScan { table: Table },
    /// Generate rows for a virtual system-catalog relation
    /// (`information_schema.*`, `pg_catalog.*`, `st_*`) from catalog
    /// metadata at execution time.
    SystemScan { relation: SystemRelation },
    /// Drop rows where `predicate` is not `Bool(true)`.
    /// (A `NULL` predicate drops the row, matching SQL `WHERE`.)
    Filter {
        input: Box<PlanNode>,
        predicate: Expr,
    },
    /// Compute a new tuple per input row from a list of expressions.
    /// The output arity equals `expressions.len()`.
    Project {
        input: Box<PlanNode>,
        expressions: Vec<Expr>,
    },
    /// Yield at most `count` rows from `input`, then stop.
    Limit { input: Box<PlanNode>, count: usize },
    /// Skip the first `count` rows of `input` (SQL `OFFSET`).
    Offset { input: Box<PlanNode>, count: usize },
    /// Sort rows by `keys` (SQL `ORDER BY`). External merge sort; a pipeline
    /// breaker. `input_schema` is the row shape it spills. Sort-merge join
    /// requires its inputs ordered by such a node.
    Sort {
        input: Box<PlanNode>,
        keys: Vec<SortKey>,
        input_schema: Schema,
    },
    /// Join two inputs; the output row is `left ++ right`. `on: None` with
    /// `join_type = Inner` is a cross join. `join_type` is the semantics
    /// (fixed by the query); `strategy` is the algorithm picked in lowering.
    Join {
        left: Box<PlanNode>,
        right: Box<PlanNode>,
        on: Option<Expr>,
        join_type: JoinType,
        /// Inner (right) row shape — the build side a hash/block join encodes.
        right_schema: Schema,
        strategy: JoinStrategy,
    },
    /// Yield each tuple in `rows` then stop. Source node for things
    /// like `INSERT INTO t VALUES (..)`.
    Values { rows: Vec<Tuple> },
    /// Sink: drain `input` and write each tuple to `table`. Only valid
    /// at the top of a plan — write operators are executed through
    /// [`crate::query::volcano::execute`], not the pull-iterator chain.
    Insert { table: Table, input: Box<PlanNode> },
    /// Sink: drain `input` and delete each tuple by its primary-key
    /// column (column 0). Same top-of-plan constraint as `Insert`.
    Delete { table: Table, input: Box<PlanNode> },
    /// DDL sink: mint a new table in the catalog. Top-of-plan only —
    /// it writes catalog metadata through `&mut ctx`, like the other
    /// sinks, and produces no rows.
    CreateTable {
        project_id: ProjectId,
        dataset_id: DatasetId,
        name: String,
        schema: Schema,
        /// `CREATE OR REPLACE` — replace any existing table (bumping its
        /// truncation id) instead of erroring on conflict.
        or_replace: bool,
    },
    /// DDL sink: create a dataset (`CREATE SCHEMA`) under a resolved project.
    CreateDataset {
        project_id: ProjectId,
        name: String,
        /// `IF NOT EXISTS` — succeed silently if the dataset already exists.
        if_not_exists: bool,
    },
}
