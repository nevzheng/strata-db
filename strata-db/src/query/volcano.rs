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

use crate::catalog::tables::Table;
use crate::storage::types::{Tuple, Value};

use super::QueryError;
use super::context::QueryContext;
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
pub struct Executor<'ctx> {
    root: Box<dyn Operator + 'ctx>,
}

impl<'ctx> Executor<'ctx> {
    pub fn new(plan: PhysicalPlan, ctx: &'ctx QueryContext<'_>) -> Result<Self, QueryError> {
        Ok(Self {
            root: build(plan.root, ctx)?,
        })
    }
}

impl Operator for Executor<'_> {
    fn next(&mut self) -> Result<NextRow, QueryError> {
        self.root.next()
    }
}

/// Recursively turn a plan tree into an operator tree.
fn build<'ctx>(
    node: PlanNode,
    ctx: &'ctx QueryContext<'_>,
) -> Result<Box<dyn Operator + 'ctx>, QueryError> {
    match node {
        PlanNode::SeqScan { table } => Ok(Box::new(SeqScan::open(table, ctx)?)),
        PlanNode::Filter { input, predicate } => {
            Ok(Box::new(Filter::new(build(*input, ctx)?, predicate)))
        }
        PlanNode::Project { input, expressions } => {
            Ok(Box::new(Project::new(build(*input, ctx)?, expressions)))
        }
        PlanNode::Limit { input, count } => Ok(Box::new(Limit::new(build(*input, ctx)?, count))),
        PlanNode::Values { rows } => Ok(Box::new(Values {
            rows: rows.into_iter(),
        })),
        // Write operators are executed via `execute()`; they're not
        // valid as inner nodes because each write needs `&mut ctx`
        // while the pull iterator chain only has `&ctx`.
        PlanNode::Insert { .. } | PlanNode::Delete { .. } => Err(QueryError::Internal(
            "Insert/Delete may only appear at the top of a plan; use volcano::execute".into(),
        )),
    }
}

/// Outcome of running a plan: either a streaming row source (read
/// queries) or a count of rows affected (writes).
pub enum ExecuteResult<'ctx> {
    Rows(Executor<'ctx>),
    Affected(u64),
}

/// Run `plan` against `ctx`. Reads return a streaming [`Executor`];
/// writes (`Insert`, `Delete`) drain their input first, then apply the
/// writes — that decouples the read borrow from the write borrow of
/// `ctx` and matches the usual snapshot semantics of `INSERT ... SELECT`.
pub fn execute<'ctx>(
    plan: PhysicalPlan,
    ctx: &'ctx mut QueryContext<'_>,
) -> Result<ExecuteResult<'ctx>, QueryError> {
    match plan.root {
        PlanNode::Insert { table, input } => {
            let count = run_insert(&table, *input, ctx)?;
            Ok(ExecuteResult::Affected(count))
        }
        PlanNode::Delete { table, input } => {
            let count = run_delete(&table, *input, ctx)?;
            Ok(ExecuteResult::Affected(count))
        }
        read_node => {
            let exec = Executor::new(PhysicalPlan::new(read_node), &*ctx)?;
            Ok(ExecuteResult::Rows(exec))
        }
    }
}

fn drain(input: PlanNode, ctx: &QueryContext<'_>) -> Result<Vec<Tuple>, QueryError> {
    let mut exec = Executor::new(PhysicalPlan::new(input), ctx)?;
    let mut out = Vec::new();
    loop {
        match exec.next()? {
            NextRow::Row(t) => out.push(t),
            NextRow::Done => return Ok(out),
        }
    }
}

fn run_insert(
    table: &Table,
    input: PlanNode,
    ctx: &mut QueryContext<'_>,
) -> Result<u64, QueryError> {
    let tuples = drain(input, &*ctx)?;
    let mut count = 0;
    for tuple in &tuples {
        ctx.put(table, tuple)?;
        count += 1;
    }
    Ok(count)
}

fn run_delete(
    table: &Table,
    input: PlanNode,
    ctx: &mut QueryContext<'_>,
) -> Result<u64, QueryError> {
    let tuples = drain(input, &*ctx)?;
    let mut count = 0;
    for tuple in &tuples {
        let key = tuple.values.first().ok_or_else(|| {
            QueryError::Internal("delete source has no primary-key column".into())
        })?;
        ctx.delete(table, key)?;
        count += 1;
    }
    Ok(count)
}

// ----- Operators -----------------------------------------------------------

/// Sequential scan over a base table. Pulls one decoded `Tuple` per
/// `next()` straight from the engine cursor held in the query context
/// — no intermediate buffering.
struct SeqScan<'ctx> {
    rows: Box<dyn Iterator<Item = Result<Tuple, QueryError>> + 'ctx>,
}

impl<'ctx> SeqScan<'ctx> {
    fn open(table: Table, ctx: &'ctx QueryContext<'_>) -> Result<Self, QueryError> {
        Ok(Self {
            rows: Box::new(ctx.scan(&table)),
        })
    }
}

impl Operator for SeqScan<'_> {
    fn next(&mut self) -> Result<NextRow, QueryError> {
        match self.rows.next() {
            Some(Ok(tuple)) => Ok(NextRow::Row(tuple)),
            Some(Err(e)) => Err(e),
            None => Ok(NextRow::Done),
        }
    }
}

/// Filter: drop rows where the predicate isn't `Bool(true)`. A `NULL`
/// predicate drops the row (matches SQL `WHERE`).
struct Filter<'ctx> {
    input: Box<dyn Operator + 'ctx>,
    predicate: Expr,
}

impl<'ctx> Filter<'ctx> {
    fn new(input: Box<dyn Operator + 'ctx>, predicate: Expr) -> Self {
        Self { input, predicate }
    }
}

impl Operator for Filter<'_> {
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
struct Project<'ctx> {
    input: Box<dyn Operator + 'ctx>,
    expressions: Vec<Expr>,
}

impl<'ctx> Project<'ctx> {
    fn new(input: Box<dyn Operator + 'ctx>, expressions: Vec<Expr>) -> Self {
        Self { input, expressions }
    }
}

impl Operator for Project<'_> {
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

/// Values: yield each row from a pre-built `Vec`, then stop. Leaf
/// operator — typically used as the source side of `Insert`.
struct Values {
    rows: std::vec::IntoIter<Tuple>,
}

impl Operator for Values {
    fn next(&mut self) -> Result<NextRow, QueryError> {
        match self.rows.next() {
            Some(t) => Ok(NextRow::Row(t)),
            None => Ok(NextRow::Done),
        }
    }
}

/// Limit: yield at most `remaining` rows from the input, then stop.
struct Limit<'ctx> {
    input: Box<dyn Operator + 'ctx>,
    remaining: usize,
}

impl<'ctx> Limit<'ctx> {
    fn new(input: Box<dyn Operator + 'ctx>, count: usize) -> Self {
        Self {
            input,
            remaining: count,
        }
    }
}

impl Operator for Limit<'_> {
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
