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
//!
//! Plans are plain data. Binding, optimization, and lowering are all
//! free functions that take a plan and return a new one; the plan
//! itself has no behavior.

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

/// One node of a logical plan tree. Each variant describes *what* the
/// operator does; algorithm choice happens during lowering.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum LogicalNode {
    /// Read every tuple of a base table. Logical doesn't yet pick
    /// between sequential and index access — that's the lowerer's job.
    Scan { table: Table },
    /// Drop rows where `predicate` is not `Bool(true)`.
    /// (A `NULL` predicate drops the row, matching SQL `WHERE`.)
    Filter {
        input: Box<LogicalNode>,
        predicate: Expr,
    },
    /// Compute a new tuple per input row from a list of expressions.
    /// The output arity equals `expressions.len()`.
    Project {
        input: Box<LogicalNode>,
        expressions: Vec<Expr>,
    },
    /// Yield at most `count` rows from `input`, then stop.
    Limit {
        input: Box<LogicalNode>,
        count: usize,
    },
    /// Yield each tuple in `rows` then stop. Source node for things
    /// like `INSERT INTO t VALUES (..)`.
    Values { rows: Vec<Tuple> },
    /// Sink: drain `input` and write each tuple to `table`.
    Insert {
        table: Table,
        input: Box<LogicalNode>,
    },
    /// Sink: drain `input` and delete each tuple by its primary-key
    /// column (column 0).
    Delete {
        table: Table,
        input: Box<LogicalNode>,
    },
}
