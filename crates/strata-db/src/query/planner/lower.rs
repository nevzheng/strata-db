//! Logical → physical lowering.
//!
//! Each [`LogicalNode`] variant becomes a concrete [`PlanNode`]. Today
//! the only real choice is `Scan → SeqScan`; the rest is a structural
//! rename. The walk lives on the [`LowerNode`] trait — mirroring the
//! [`BindNode`](super::binder::BindNode) pattern in the binder — so a
//! variant that grows multiple physical realizations can branch from
//! one place. [`Lower`] is the planner [`Pass`](super::pass::Pass) that
//! plugs this into the pipeline.

use crate::catalog::schema::Schema;
use crate::query::QueryContext;
use crate::query::QueryError;
use crate::query::expression::{BinaryOperator, Expr};
use crate::query::logical_plan::{JoinType, LogicalNode, SortKey};
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

/// Lower a join, choosing the physical algorithm. An inner equi-join becomes a
/// **sort-merge** join: each input is wrapped in a `Sort` enforcer on its join
/// key (so the operator can assume sorted inputs). Everything else — non-equi,
/// cross, and outer joins — falls back to **block nested loop**, the general
/// algorithm. (`NestedLoop` stays the reference, selectable only by a hand-built
/// plan; grace-hash routing lands with that operator.)
fn lower_join(
    left: &LogicalNode,
    right: &LogicalNode,
    on: &Option<Expr>,
    join_type: JoinType,
    left_schema: &Schema,
    right_schema: &Schema,
) -> PlanNode {
    let left_p = left.lower();
    let right_p = right.lower();
    let left_arity = left_schema.fields.len();

    if join_type == JoinType::Inner
        && let Some((left_key, right_key)) = equi_join_keys(on, left_arity)
    {
        return PlanNode::Join {
            left: Box::new(sort_on(left_p, left_key, left_schema)),
            right: Box::new(sort_on(right_p, right_key, right_schema)),
            on: on.clone(),
            join_type,
            left_schema: left_schema.clone(),
            right_schema: right_schema.clone(),
            strategy: JoinStrategy::SortMerge,
        };
    }

    PlanNode::Join {
        left: Box::new(left_p),
        right: Box::new(right_p),
        on: on.clone(),
        join_type,
        left_schema: left_schema.clone(),
        right_schema: right_schema.clone(),
        strategy: JoinStrategy::BlockNestedLoop,
    }
}

/// If `on` is an equi-join `col = col` referencing exactly one column on each
/// side, return the (left, right) key positions *within their own tuples*
/// (the right index de-offset from the combined `left ++ right` row).
fn equi_join_keys(on: &Option<Expr>, left_arity: usize) -> Option<(usize, usize)> {
    let Some(Expr::Binary {
        op: BinaryOperator::Eq,
        lhs,
        rhs,
    }) = on
    else {
        return None;
    };
    let (Expr::Column { index: a }, Expr::Column { index: b }) = (lhs.as_ref(), rhs.as_ref())
    else {
        return None;
    };
    let (a, b) = (*a, *b);
    if a < left_arity && b >= left_arity {
        Some((a, b - left_arity))
    } else if b < left_arity && a >= left_arity {
        Some((b, a - left_arity))
    } else {
        None
    }
}

/// Wrap `input` in a `Sort` on a single ascending column — the enforcer that
/// gives a sort-merge join its required input ordering.
fn sort_on(input: PlanNode, column: usize, input_schema: &Schema) -> PlanNode {
    PlanNode::Sort {
        input: Box::new(input),
        keys: vec![SortKey {
            expr: Expr::column(column),
            ascending: true,
            nulls_first: false,
        }],
        input_schema: input_schema.clone(),
    }
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
            LogicalNode::Sort {
                input,
                keys,
                input_schema,
            } => PlanNode::Sort {
                input: Box::new(input.lower()),
                keys: keys.clone(),
                input_schema: input_schema.clone(),
            },
            LogicalNode::Join {
                left,
                right,
                on,
                join_type,
                left_schema,
                right_schema,
            } => lower_join(left, right, on, *join_type, left_schema, right_schema),
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
