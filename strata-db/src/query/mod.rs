//! Query processing: planning and execution.
//!
//! A query passes through this module in stages: parse → bind → logical
//! plan → optimize → physical plan → execute. Today only the physical
//! plan and the (forthcoming) execution backends live here; a parser,
//! binder, and logical planner will land alongside.
//!
//! Two trees show up at this level. [`PhysicalPlan`] / [`PlanNode`]
//! describe dataflow over rows — scans, filters, joins. [`Expr`]
//! describes per-row computation — column refs, comparisons, boolean
//! combinators — and lives inside plan nodes that need predicates or
//! projections.

pub mod expression;
pub mod physical_plan;
pub mod volcano;

pub use expression::{BinaryOperator, Expr};
pub use physical_plan::{PhysicalPlan, PlanNode};

use crate::catalog::CatalogError;

#[derive(Debug)]
pub enum QueryError {
    Storage(CatalogError),
    /// Type mismatch in expression evaluation (e.g. `Bool < Int32`).
    Type(String),
    /// Column reference past the end of the input tuple.
    ColumnIndex {
        index: usize,
        arity: usize,
    },
}

impl From<CatalogError> for QueryError {
    fn from(err: CatalogError) -> Self {
        QueryError::Storage(err)
    }
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryError::Storage(e) => write!(f, "storage: {e:?}"),
            QueryError::Type(msg) => write!(f, "type error: {msg}"),
            QueryError::ColumnIndex { index, arity } => {
                write!(f, "column index {index} out of bounds (arity {arity})")
            }
        }
    }
}

impl std::error::Error for QueryError {}
