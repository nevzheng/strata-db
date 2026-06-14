//! Volcano: pull-based iterator backend.
//!
//! Each operator is an `Iterator<Item = RowResult>` that pulls from its input
//! one row at a time — the slowest backend (one virtual call per row, per
//! operator level) but the simplest that can run a plan; vectorized and JIT
//! backends will live alongside.
//!
//! This module owns the executor entry point and the [`build`] dispatch that
//! wires a [`PlanNode`] tree into a stream. The operators live in focused
//! submodules: [`scalar`] (Filter/Project/Limit/Offset), [`sink`]
//! (Insert/Delete writes), [`join`] (nested-loop family), and [`sort`]
//! (external merge sort). Shared bits — the recursive `build` and `drain` — stay
//! here; the workspace tuple codec lives in [`storage::codec`](crate::storage::codec).

mod join;
mod scalar;
mod sink;
mod sort;

use crate::catalog::CatalogError;
use crate::storage::table_api::ScanOptions;
use crate::storage::types::Tuple;

use super::QueryContext;
use super::QueryError;
use super::executor::{ExecuteResult, Executor, RowStream};
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

/// Top-level dispatch: writes drain their input under `&ctx` then apply the
/// writes under `&mut ctx`. Reads return a streaming `RowStream`.
fn run<'ctx>(
    plan: PhysicalPlan,
    ctx: &'ctx mut QueryContext<'_>,
) -> Result<ExecuteResult<'ctx>, QueryError> {
    match plan.root {
        PlanNode::Insert { table, input } => {
            Ok(ExecuteResult::Affected(sink::insert(table, *input, ctx)?))
        }
        PlanNode::Delete { table, input } => {
            Ok(ExecuteResult::Affected(sink::delete(table, *input, ctx)?))
        }
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

/// Build the row stream for a read plan, recursing into inputs. Operators
/// delegate to their submodules; pipeline breakers (`Join`, `Sort`) take the
/// raw input nodes and build them internally.
pub(super) fn build<'ctx>(
    node: PlanNode,
    ctx: &'ctx QueryContext<'_>,
) -> Result<RowStream<'ctx>, QueryError> {
    match node {
        PlanNode::SeqScan { table } => {
            Ok(RowStream::new(ctx.table(&table).scan(ScanOptions::new())))
        }
        // Virtual catalog relation: rows are generated from catalog metadata,
        // not read from storage.
        PlanNode::SystemScan { relation } => {
            let rows = relation.rows(&ctx.catalog())?;
            Ok(RowStream::new(rows.into_iter().map(Ok)))
        }
        PlanNode::Filter { input, predicate } => Ok(scalar::filter(build(*input, ctx)?, predicate)),
        PlanNode::Project { input, expressions } => {
            Ok(scalar::project(build(*input, ctx)?, expressions))
        }
        PlanNode::Limit { input, count } => Ok(scalar::limit(build(*input, ctx)?, count)),
        PlanNode::Offset { input, count } => Ok(scalar::offset(build(*input, ctx)?, count)),
        PlanNode::Join {
            left,
            right,
            on,
            join_type,
            // The schema-driven codec for tuples a join spills (block/grace);
            // nested-loop and sort-merge recover arities structurally instead.
            left_schema,
            right_schema,
            strategy,
        } => match strategy {
            JoinStrategy::NestedLoop => join::nested_loop_join(*left, *right, on, join_type, ctx),
            JoinStrategy::BlockNestedLoop => {
                join::block_nested_loop_join(*left, *right, on, join_type, right_schema, ctx)
            }
            JoinStrategy::SortMerge => join::sort_merge_join(*left, *right, on, join_type, ctx),
            JoinStrategy::GraceHash => {
                join::grace_hash_join(*left, *right, on, join_type, left_schema, right_schema, ctx)
            }
        },
        PlanNode::Sort {
            input,
            keys,
            input_schema,
        } => sort::sort(*input, keys, input_schema, ctx),
        PlanNode::Values { rows } => Ok(RowStream::new(rows.into_iter().map(Ok))),
        // Sinks (Insert/Delete/CreateTable) are top-level only — they need
        // `&mut ctx` and can't sit inside a pull iterator chain holding `&ctx`.
        PlanNode::Insert { .. }
        | PlanNode::Delete { .. }
        | PlanNode::CreateTable { .. }
        | PlanNode::CreateDataset { .. } => Err(QueryError::Internal(
            "write sinks may only appear at the top of a plan".into(),
        )),
    }
}

/// Fully materialize a read plan's rows — used by the write sinks.
pub(super) fn drain(input: PlanNode, ctx: &QueryContext<'_>) -> Result<Vec<Tuple>, QueryError> {
    build(input, ctx)?.collect()
}
