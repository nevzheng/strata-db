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

use crate::tables::Table;

use super::expression::Expr;

/// A physical plan: the root of an operator tree, plus future
/// plan-level metadata.
pub struct PhysicalPlan {
    pub root: PlanNode,
}

impl PhysicalPlan {
    pub fn new(root: PlanNode) -> Self {
        Self { root }
    }
}

/// One node of a physical plan tree. Each variant is a concrete
/// operator with the algorithm baked in (e.g. `SeqScan` versus a
/// hypothetical `IndexScan`).
pub enum PlanNode {
    /// Read every tuple from a base table.
    SeqScan { table: Table },
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
}
