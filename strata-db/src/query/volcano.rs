//! Volcano: pull-based iterator interpreter.
//!
//! Each operator implements [`Operator`] and emits rows one at a time
//! when its parent calls [`Operator::next`]. The [`Executor`] builds an
//! operator tree from a [`PhysicalPlan`] and drives it from the root.
//!
//! This is the simplest backend that can run a plan. It's also the
//! slowest: every row crosses a virtual call boundary at every
//! operator. A vectorized backend (batches across the same boundary)
//! and a JIT backend (the whole pipeline becomes one compiled
//! function) live elsewhere when they exist.

use crate::tables::{Table, TypedStore};
use crate::types::{Tuple, Value};

use super::QueryError;
use super::expression::Expr;
use super::physical_plan::{PhysicalPlan, PlanNode};

/// One pull from an operator: a row, or end-of-stream. Errors are
/// threaded through the outer `Result`, so the full three-state return
/// is `Result<NextRow, QueryError>`.
pub enum NextRow {
    Row(Tuple),
    Done,
}

/// A node in a Volcano operator tree. Each call to [`next`] yields the
/// next row, signals `Done`, or errors.
///
/// [`next`]: Operator::next
pub trait Operator {
    fn next(&mut self) -> Result<NextRow, QueryError>;
}

/// Drives a [`PhysicalPlan`] to completion, one row at a time.
pub struct Executor {
    root: Box<dyn Operator>,
}

impl Executor {
    pub fn new(plan: PhysicalPlan) -> Result<Self, QueryError> {
        Ok(Self {
            root: build(plan.root)?,
        })
    }
}

impl Operator for Executor {
    fn next(&mut self) -> Result<NextRow, QueryError> {
        self.root.next()
    }
}

/// Recursively turn a plan tree into an operator tree.
fn build(node: PlanNode) -> Result<Box<dyn Operator>, QueryError> {
    match node {
        PlanNode::SeqScan { table } => Ok(Box::new(SeqScan::open(table)?)),
        PlanNode::Filter { input, predicate } => {
            Ok(Box::new(Filter::new(build(*input)?, predicate)))
        }
        PlanNode::Project { input, expressions } => {
            Ok(Box::new(Project::new(build(*input)?, expressions)))
        }
        PlanNode::Limit { input, count } => Ok(Box::new(Limit::new(build(*input)?, count))),
    }
}

// ----- Operators -----------------------------------------------------------

/// Sequential scan: read every tuple from a base table.
///
/// Today this is not actually streaming — [`TypedStore::scan`] returns
/// a fully materialized `Vec`. The operator just hands those tuples
/// back one by one. A streaming scan is a future change to the storage
/// layer.
struct SeqScan {
    rows: std::vec::IntoIter<(Vec<u8>, Tuple)>,
}

impl SeqScan {
    fn open(table: Table) -> Result<Self, QueryError> {
        let rows = table.scan(&[])?;
        Ok(Self {
            rows: rows.into_iter(),
        })
    }
}

impl Operator for SeqScan {
    fn next(&mut self) -> Result<NextRow, QueryError> {
        match self.rows.next() {
            Some((_, tuple)) => Ok(NextRow::Row(tuple)),
            None => Ok(NextRow::Done),
        }
    }
}

/// Filter: drop rows where the predicate isn't `Bool(true)`. A `NULL`
/// predicate drops the row (matches SQL `WHERE`).
struct Filter {
    input: Box<dyn Operator>,
    predicate: Expr,
}

impl Filter {
    fn new(input: Box<dyn Operator>, predicate: Expr) -> Self {
        Self { input, predicate }
    }
}

impl Operator for Filter {
    fn next(&mut self) -> Result<NextRow, QueryError> {
        loop {
            match self.input.next()? {
                NextRow::Done => return Ok(NextRow::Done),
                NextRow::Row(tuple) => {
                    if matches!(self.predicate.eval(&tuple)?, Value::Bool(true)) {
                        return Ok(NextRow::Row(tuple));
                    }
                }
            }
        }
    }
}

/// Project: compute a new tuple per input row from a list of
/// expressions. Output arity equals `expressions.len()`.
struct Project {
    input: Box<dyn Operator>,
    expressions: Vec<Expr>,
}

impl Project {
    fn new(input: Box<dyn Operator>, expressions: Vec<Expr>) -> Self {
        Self { input, expressions }
    }
}

impl Operator for Project {
    fn next(&mut self) -> Result<NextRow, QueryError> {
        match self.input.next()? {
            NextRow::Done => Ok(NextRow::Done),
            NextRow::Row(tuple) => {
                let mut values = Vec::with_capacity(self.expressions.len());
                for expr in &self.expressions {
                    values.push(expr.eval(&tuple)?);
                }
                Ok(NextRow::Row(Tuple { values }))
            }
        }
    }
}

/// Limit: yield at most `remaining` rows from the input, then stop.
struct Limit {
    input: Box<dyn Operator>,
    remaining: usize,
}

impl Limit {
    fn new(input: Box<dyn Operator>, count: usize) -> Self {
        Self {
            input,
            remaining: count,
        }
    }
}

impl Operator for Limit {
    fn next(&mut self) -> Result<NextRow, QueryError> {
        if self.remaining == 0 {
            return Ok(NextRow::Done);
        }
        match self.input.next()? {
            NextRow::Done => Ok(NextRow::Done),
            NextRow::Row(tuple) => {
                self.remaining -= 1;
                Ok(NextRow::Row(tuple))
            }
        }
    }
}
