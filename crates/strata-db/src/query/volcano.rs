//! Volcano: pull-based iterator backend.
//!
//! Each operator is an `Iterator<Item = RowResult>` that pulls from
//! its input one row at a time. The slowest backend (one virtual call
//! per row, per operator level) but the simplest that can run a plan;
//! vectorized and JIT backends will live alongside.

use crate::catalog::CatalogError;
use crate::catalog::tables::Table;
use crate::storage::table_api::ScanOptions;
use crate::storage::types::{Tuple, Value};

use super::QueryContext;
use super::QueryError;
use super::executor::{ExecuteResult, Executor, RowResult, RowStream};
use super::expression::Expr;
use super::logical_plan::JoinType;
use super::physical_plan::{JoinStrategy, PhysicalPlan, PlanNode};

pub struct Volcano;

impl Executor for Volcano {
    fn execute<'ctx>(
        &self,
        plan: PhysicalPlan,
        ctx: &'ctx mut QueryContext<'_>,
    ) -> Result<ExecuteResult<'ctx>, QueryError> {
        run(plan, ctx)
    }
}

/// Top-level dispatch: writes drain their input under `&ctx` then apply
/// the writes under `&mut ctx`. Reads return a streaming `RowStream`.
fn run<'ctx>(
    plan: PhysicalPlan,
    ctx: &'ctx mut QueryContext<'_>,
) -> Result<ExecuteResult<'ctx>, QueryError> {
    match plan.root {
        PlanNode::Insert { table, input } => Ok(ExecuteResult::Affected(
            InsertSink {
                table,
                input: *input,
            }
            .run(ctx)?,
        )),
        PlanNode::Delete { table, input } => Ok(ExecuteResult::Affected(
            DeleteSink {
                table,
                input: *input,
            }
            .run(ctx)?,
        )),
        PlanNode::CreateTable {
            project_id,
            dataset_id,
            name,
            schema,
            or_replace,
        } => {
            if or_replace {
                crate::catalog::replace_table(
                    &mut ctx.engine,
                    project_id,
                    dataset_id,
                    &name,
                    schema,
                )?;
            } else {
                crate::catalog::create_table(
                    &mut ctx.engine,
                    project_id,
                    dataset_id,
                    &name,
                    schema,
                )?;
            }
            Ok(ExecuteResult::Affected(0))
        }
        PlanNode::CreateDataset {
            project_id,
            name,
            if_not_exists,
        } => {
            match crate::catalog::create_dataset(&mut ctx.engine, project_id, &name) {
                Ok(_) => {}
                // `IF NOT EXISTS`: an existing dataset is a no-op, not an error.
                Err(QueryError::Catalog(CatalogError::AlreadyExists { .. })) if if_not_exists => {}
                Err(e) => return Err(e),
            }
            Ok(ExecuteResult::Affected(0))
        }
        read_node => Ok(ExecuteResult::Rows(build(read_node, &*ctx)?)),
    }
}

fn build<'ctx>(node: PlanNode, ctx: &'ctx QueryContext<'_>) -> Result<RowStream<'ctx>, QueryError> {
    match node {
        PlanNode::SeqScan { table } => {
            Ok(RowStream::new(ctx.table(&table).scan(ScanOptions::new())))
        }
        // Virtual catalog relation: rows are generated from catalog
        // metadata, not read from storage.
        PlanNode::SystemScan { relation } => {
            let rows = relation.rows(&ctx.catalog())?;
            Ok(RowStream::new(rows.into_iter().map(Ok)))
        }
        PlanNode::Filter { input, predicate } => Ok(RowStream::new(Filter {
            input: build(*input, ctx)?,
            predicate,
        })),
        PlanNode::Project { input, expressions } => Ok(RowStream::new(Project {
            input: build(*input, ctx)?,
            expressions,
        })),
        PlanNode::Limit { input, count } => Ok(RowStream::new(Limit {
            input: build(*input, ctx)?,
            remaining: count,
        })),
        PlanNode::Offset { input, count } => Ok(RowStream::new(Offset {
            input: build(*input, ctx)?,
            remaining: count,
        })),
        PlanNode::Join {
            left,
            right,
            on,
            join_type,
            strategy,
        } => match strategy {
            JoinStrategy::NestedLoop => nested_loop_join(*left, *right, on, join_type, ctx),
            // The only strategy the optimizer emits today; the others are
            // built in upcoming pieces.
            other => Err(QueryError::Internal(format!(
                "join strategy not implemented: {other:?}"
            ))),
        },
        PlanNode::Values { rows } => Ok(RowStream::new(rows.into_iter().map(Ok))),
        // Sinks (Insert/Delete/CreateTable) are top-level only — they
        // need `&mut ctx` and can't sit inside a pull iterator chain
        // that holds `&ctx`.
        PlanNode::Insert { .. }
        | PlanNode::Delete { .. }
        | PlanNode::CreateTable { .. }
        | PlanNode::CreateDataset { .. } => Err(QueryError::Internal(
            "write sinks may only appear at the top of a plan".into(),
        )),
    }
}

fn drain(input: PlanNode, ctx: &QueryContext<'_>) -> Result<Vec<Tuple>, QueryError> {
    build(input, ctx)?.collect()
}

// --- joins -----------------------------------------------------------------

/// Tuple-at-a-time nested-loop join — the general algorithm, correct for any
/// predicate (and cross joins with `on: None`). The output row is the left
/// tuple concatenated with the right.
///
/// Materializes both inputs up front (the inner must be rescanned; outer/full
/// joins need a second pass over unmatched right rows). The slow, always-right
/// baseline; the streaming/spilling variants are separate operators.
fn nested_loop_join<'ctx>(
    left: PlanNode,
    right: PlanNode,
    on: Option<Expr>,
    join_type: JoinType,
    ctx: &'ctx QueryContext<'_>,
) -> Result<RowStream<'ctx>, QueryError> {
    // Arities (for NULL-padding) read structurally, so an empty side still
    // pads to the right width.
    let left_width = output_arity(&left);
    let right_width = output_arity(&right);
    let left_rows: Vec<Tuple> = build(left, ctx)?.collect::<Result<_, _>>()?;
    let right_rows: Vec<Tuple> = build(right, ctx)?.collect::<Result<_, _>>()?;

    let mut out: Vec<Tuple> = Vec::new();
    // Tracks which right rows matched, for RIGHT/FULL's unmatched pass.
    let mut right_matched = vec![false; right_rows.len()];

    for l in &left_rows {
        let mut l_matched = false;
        for (j, r) in right_rows.iter().enumerate() {
            let combined = concat(l, r);
            let matched = match &on {
                None => true,
                Some(pred) => matches!(pred.eval(&combined)?, Value::Bool(true)),
            };
            if matched {
                l_matched = true;
                right_matched[j] = true;
                out.push(combined);
            }
        }
        // LEFT/FULL: an unmatched left row survives, padded with right NULLs.
        if !l_matched && matches!(join_type, JoinType::Left | JoinType::Full) {
            out.push(pad_right(l, right_width));
        }
    }

    // RIGHT/FULL: unmatched right rows survive, padded with left NULLs.
    if matches!(join_type, JoinType::Right | JoinType::Full) {
        for (j, r) in right_rows.iter().enumerate() {
            if !right_matched[j] {
                out.push(pad_left(left_width, r));
            }
        }
    }

    Ok(RowStream::new(out.into_iter().map(Ok)))
}

/// `left ++ right`.
fn concat(left: &Tuple, right: &Tuple) -> Tuple {
    let mut values = Vec::with_capacity(left.values.len() + right.values.len());
    values.extend(left.values.iter().cloned());
    values.extend(right.values.iter().cloned());
    Tuple { values }
}

/// `left ++ NULLs` — an unmatched left row in a LEFT/FULL join.
fn pad_right(left: &Tuple, right_width: usize) -> Tuple {
    let mut values = left.values.clone();
    values.resize(values.len() + right_width, Value::Null);
    Tuple { values }
}

/// `NULLs ++ right` — an unmatched right row in a RIGHT/FULL join.
fn pad_left(left_width: usize, right: &Tuple) -> Tuple {
    let mut values = vec![Value::Null; left_width];
    values.extend(right.values.iter().cloned());
    Tuple { values }
}

/// A plan node's output column count. Needed to NULL-pad the absent side of an
/// outer join even when that side yields zero rows. Computed structurally —
/// plans don't carry an output schema yet.
fn output_arity(node: &PlanNode) -> usize {
    match node {
        PlanNode::SeqScan { table } => table.schema().fields.len(),
        PlanNode::SystemScan { relation } => relation.schema().fields.len(),
        PlanNode::Filter { input, .. }
        | PlanNode::Limit { input, .. }
        | PlanNode::Offset { input, .. } => output_arity(input),
        PlanNode::Project { expressions, .. } => expressions.len(),
        PlanNode::Values { rows } => rows.first().map_or(0, |t| t.values.len()),
        PlanNode::Join { left, right, .. } => output_arity(left) + output_arity(right),
        // Sinks produce a row count, not rows.
        PlanNode::Insert { .. }
        | PlanNode::Delete { .. }
        | PlanNode::CreateTable { .. }
        | PlanNode::CreateDataset { .. } => 0,
    }
}

// --- sinks: drain input, then apply writes ---------------------------------

/// Plan-level operation that consumes rows and returns a count.
///
/// Sinks have a different shape from read operators: they need
/// `&mut QueryContext` (so they can't sit inside a pull iterator
/// chain), they eagerly drain their input before writing, and they
/// produce a `u64` row count rather than a row stream.
trait SinkOperator {
    fn run(self, ctx: &mut QueryContext<'_>) -> Result<u64, QueryError>;
}

struct InsertSink {
    table: Table,
    input: PlanNode,
}

impl SinkOperator for InsertSink {
    fn run(self, ctx: &mut QueryContext<'_>) -> Result<u64, QueryError> {
        let tuples = drain(self.input, &*ctx)?;
        let mut writer = ctx.table_mut(&self.table);
        let mut count = 0;
        for tuple in &tuples {
            writer.put(tuple)?;
            count += 1;
        }
        Ok(count)
    }
}

struct DeleteSink {
    table: Table,
    input: PlanNode,
}

impl SinkOperator for DeleteSink {
    fn run(self, ctx: &mut QueryContext<'_>) -> Result<u64, QueryError> {
        let tuples = drain(self.input, &*ctx)?;
        let mut writer = ctx.table_mut(&self.table);
        let mut count = 0;
        for tuple in &tuples {
            let key = tuple.values.first().ok_or_else(|| {
                QueryError::Internal("delete source has no primary-key column".into())
            })?;
            writer.delete(key)?;
            count += 1;
        }
        Ok(count)
    }
}

// --- operators -------------------------------------------------------------

struct Filter<'ctx> {
    input: RowStream<'ctx>,
    predicate: Expr,
}

impl Iterator for Filter<'_> {
    type Item = RowResult;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let tuple = match self.input.next()? {
                Ok(t) => t,
                err @ Err(_) => return Some(err),
            };
            match self.predicate.eval(&tuple) {
                Ok(Value::Bool(true)) => return Some(Ok(tuple)),
                Ok(_) => continue,
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

struct Project<'ctx> {
    input: RowStream<'ctx>,
    expressions: Vec<Expr>,
}

impl Iterator for Project<'_> {
    type Item = RowResult;

    fn next(&mut self) -> Option<Self::Item> {
        let tuple = match self.input.next()? {
            Ok(t) => t,
            err @ Err(_) => return Some(err),
        };
        let mut values = Vec::with_capacity(self.expressions.len());
        for expr in &self.expressions {
            match expr.eval(&tuple) {
                Ok(v) => values.push(v),
                Err(e) => return Some(Err(e)),
            }
        }
        Some(Ok(Tuple { values }))
    }
}

struct Limit<'ctx> {
    input: RowStream<'ctx>,
    remaining: usize,
}

impl Iterator for Limit<'_> {
    type Item = RowResult;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let row = self.input.next()?;
        self.remaining -= 1;
        Some(row)
    }
}

struct Offset<'ctx> {
    input: RowStream<'ctx>,
    /// Rows still to skip before passing input through.
    remaining: usize,
}

impl Iterator for Offset<'_> {
    type Item = RowResult;

    fn next(&mut self) -> Option<Self::Item> {
        // Drop the first `remaining` rows, but surface an error hit while
        // skipping instead of swallowing it.
        while self.remaining > 0 {
            match self.input.next()? {
                Ok(_) => self.remaining -= 1,
                err @ Err(_) => return Some(err),
            }
        }
        self.input.next()
    }
}
