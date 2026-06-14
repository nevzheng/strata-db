//! Logical → physical lowering.
//!
//! Each [`LogicalNode`] variant becomes a concrete [`PlanNode`]. Today
//! the only real choice is `Scan → SeqScan`; the rest is a structural
//! rename. The walk lives on the [`LowerNode`] trait — mirroring the
//! [`BindNode`](super::binder::BindNode) pattern in the binder — so a
//! variant that grows multiple physical realizations can branch from
//! one place. [`Lower`] is the planner [`Pass`](super::pass::Pass) that
//! plugs this into the pipeline.

use crate::query::QueryContext;
use crate::query::QueryError;
use crate::query::expression::Expr;
use crate::query::logical_plan::LogicalNode;
use crate::query::physical_plan::{JoinStrategy, PhysicalPlan, PlanNode};
use crate::query::stages::{LogicalQuery, PhysicalQuery};

use super::pass::Pass;

pub(super) struct Lower;

impl Pass for Lower {
    type Input = LogicalQuery;
    type Output = PhysicalQuery;

    fn name(&self) -> &'static str {
        "lower"
    }

    fn run(
        &self,
        input: LogicalQuery,
        _ctx: &QueryContext<'_>,
    ) -> Result<PhysicalQuery, QueryError> {
        let physical: Vec<PhysicalPlan> = input
            .logical
            .iter()
            .map(|lp| PhysicalPlan::new(lp.root.lower()))
            .collect();
        Ok(PhysicalQuery {
            sql: input.sql,
            ast: input.ast,
            logical: input.logical,
            physical,
        })
    }
}

/// Pick the physical algorithm for a join — the single place strategy
/// selection lives. Today it always returns the general nested-loop
/// algorithm (handles any predicate and cross joins); it grows cost-based
/// — equi-join → hash/sort-merge, sized inputs → grace — as those
/// operators land.
fn select_join_strategy(_on: Option<&Expr>) -> JoinStrategy {
    JoinStrategy::NestedLoop
}

pub(super) trait LowerNode {
    type Output;
    fn lower(&self) -> Self::Output;
}

impl LowerNode for LogicalNode {
    type Output = PlanNode;

    fn lower(&self) -> PlanNode {
        match self {
            LogicalNode::Scan { table } => PlanNode::SeqScan {
                table: table.clone(),
            },
            LogicalNode::SystemScan { relation } => PlanNode::SystemScan {
                relation: *relation,
            },
            LogicalNode::Filter { input, predicate } => PlanNode::Filter {
                input: Box::new(input.lower()),
                predicate: predicate.clone(),
            },
            LogicalNode::Project { input, expressions } => PlanNode::Project {
                input: Box::new(input.lower()),
                expressions: expressions.clone(),
            },
            LogicalNode::Limit { input, count } => PlanNode::Limit {
                input: Box::new(input.lower()),
                count: *count,
            },
            LogicalNode::Offset { input, count } => PlanNode::Offset {
                input: Box::new(input.lower()),
                count: *count,
            },
            LogicalNode::Join {
                left,
                right,
                on,
                join_type,
            } => PlanNode::Join {
                left: Box::new(left.lower()),
                right: Box::new(right.lower()),
                on: on.clone(),
                join_type: *join_type,
                strategy: select_join_strategy(on.as_ref()),
            },
            LogicalNode::Values { rows } => PlanNode::Values { rows: rows.clone() },
            LogicalNode::Insert { table, input } => PlanNode::Insert {
                table: table.clone(),
                input: Box::new(input.lower()),
            },
            LogicalNode::Delete { table, input } => PlanNode::Delete {
                table: table.clone(),
                input: Box::new(input.lower()),
            },
            LogicalNode::CreateTable {
                project_id,
                dataset_id,
                name,
                schema,
                or_replace,
            } => PlanNode::CreateTable {
                project_id: *project_id,
                dataset_id: *dataset_id,
                name: name.clone(),
                schema: schema.clone(),
                or_replace: *or_replace,
            },
            LogicalNode::CreateDataset {
                project_id,
                name,
                if_not_exists,
            } => PlanNode::CreateDataset {
                project_id: *project_id,
                name: name.clone(),
                if_not_exists: *if_not_exists,
            },
        }
    }
}
