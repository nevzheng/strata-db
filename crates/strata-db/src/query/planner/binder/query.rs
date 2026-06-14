//! Binding for `SELECT`: the query body, FROM relation, WHERE, and the
//! projection list.

use sqlparser::ast::{
    Expr as AstExpr, GroupByExpr, Join, JoinConstraint, JoinOperator, LimitClause, OrderByExpr,
    OrderByKind, Query as AstQuery, Select, SelectItem, SetExpr, TableFactor, TableWithJoins,
};

use crate::catalog::system::SystemRelation;
use crate::query::QueryError;
use crate::query::expression::Expr;
use crate::query::logical_plan::{JoinType, LogicalNode, LogicalPlan, SortKey};
use crate::storage::types::{Tuple, Value};

use super::scope::Scope;
use super::{BindNode, Binder, name_idents, three_part_name};

impl BindNode for AstQuery {
    type Output = LogicalPlan;

    fn bind(&self, binder: &mut Binder) -> Result<LogicalPlan, QueryError> {
        // FETCH would change the result; reject rather than silently ignore.
        if self.fetch.is_some() {
            return Err(QueryError::unsupported("FETCH"));
        }
        let order_by = match &self.order_by {
            None => None,
            Some(ob) => match &ob.kind {
                OrderByKind::Expressions(exprs) => Some(exprs.as_slice()),
                OrderByKind::All(_) => return Err(QueryError::unsupported("ORDER BY ALL")),
            },
        };

        let SetExpr::Select(select) = self.body.as_ref() else {
            return Err(QueryError::unsupported(format!(
                "query body: {:?}",
                self.body
            )));
        };
        let mut node = bind_select(select, order_by, binder)?;

        // `LIMIT k OFFSET n` (Postgres): skip n rows, then take k. Apply
        // OFFSET first so it sits closest to the input.
        if let Some(clause) = &self.limit_clause {
            let LimitClause::LimitOffset {
                limit,
                offset,
                limit_by,
            } = clause
            else {
                return Err(QueryError::unsupported("`LIMIT offset, count`"));
            };
            if !limit_by.is_empty() {
                return Err(QueryError::unsupported("LIMIT BY"));
            }
            if let Some(offset) = offset {
                let count = bind_count(&offset.value, binder, "OFFSET")?;
                node = LogicalNode::Offset {
                    input: Box::new(node),
                    count,
                };
            }
            if let Some(limit) = limit {
                let count = bind_count(limit, binder, "LIMIT")?;
                node = LogicalNode::Limit {
                    input: Box::new(node),
                    count,
                };
            }
        }

        Ok(LogicalPlan::new(node))
    }
}

/// Evaluate a `LIMIT` / `OFFSET` expression to a non-negative row count.
/// It binds with no scope, so it must be a constant integer.
fn bind_count(expr: &AstExpr, binder: &mut Binder, what: &str) -> Result<usize, QueryError> {
    let n = match expr.bind(binder)?.eval(&Tuple { values: vec![] })? {
        Value::Int16(n) => i64::from(n),
        Value::Int32(n) => i64::from(n),
        Value::Int64(n) => n,
        other => {
            return Err(QueryError::type_error(format!(
                "{what} expects an integer, got {other:?}"
            )));
        }
    };
    usize::try_from(n).map_err(|_| QueryError::type_error(format!("{what} must be non-negative")))
}

// --- SELECT body: FROM + WHERE + projection --------------------------------

/// Bind a `SELECT` body, optionally with an `ORDER BY`. The Sort sits below the
/// projection so it can order by any input column or expression (binding
/// against the FROM scope), not just the select list.
fn bind_select(
    select: &Select,
    order_by: Option<&[OrderByExpr]>,
    binder: &mut Binder,
) -> Result<LogicalNode, QueryError> {
    // Reject SQL features the engine doesn't implement yet, so they surface as
    // a clear `unsupported: <feature>` instead of silently producing a wrong plan.
    if select.distinct.is_some() {
        return Err(QueryError::unsupported("DISTINCT"));
    }
    if !matches!(&select.group_by, GroupByExpr::Expressions(exprs, _) if exprs.is_empty()) {
        return Err(QueryError::unsupported("GROUP BY"));
    }
    if select.having.is_some() {
        return Err(QueryError::unsupported("HAVING"));
    }

    // 1. FROM → source + the scope it exposes. Empty FROM yields a one-row,
    // zero-column source so `SELECT <expr>` (no FROM) binds.
    let (source, scope) = bind_from(&select.from, binder)?;

    binder.push_scope(scope);

    // 2. WHERE → wrap in Filter if a predicate is present.
    let after_where = match &select.selection {
        Some(pred) => LogicalNode::Filter {
            input: Box::new(source),
            predicate: pred.bind(binder)?,
        },
        None => source,
    };

    // 3. ORDER BY → Sort below the projection, binding against the FROM scope.
    let after_order = match order_by {
        Some(exprs) => {
            let keys = bind_sort_keys(exprs, binder)?;
            // The Sort materializes the FROM rows — carry their schema so it can
            // encode them in scratch storage.
            let input_schema = binder
                .current_scope()
                .ok_or_else(|| QueryError::Internal("no scope for ORDER BY".into()))?
                .schema();
            LogicalNode::Sort {
                input: Box::new(after_where),
                keys,
                input_schema,
            }
        }
        None => after_where,
    };

    // 4. Projection → Project. Each `SelectItem` may expand to multiple
    // expressions (wildcards), hence flatten.
    let expressions: Vec<Expr> = select
        .projection
        .iter()
        .map(|item| item.bind(binder))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();

    binder.pop_scope();

    Ok(LogicalNode::Project {
        input: Box::new(after_order),
        expressions,
    })
}

/// Bind `ORDER BY` terms. NULL placement defaults the Postgres way: NULLS LAST
/// for ASC, NULLS FIRST for DESC.
fn bind_sort_keys(exprs: &[OrderByExpr], binder: &mut Binder) -> Result<Vec<SortKey>, QueryError> {
    exprs
        .iter()
        .map(|o| {
            let ascending = o.options.asc.unwrap_or(true);
            Ok(SortKey {
                expr: o.expr.bind(binder)?,
                ascending,
                nulls_first: o.options.nulls_first.unwrap_or(!ascending),
            })
        })
        .collect()
}

// --- FROM clause (relations + joins → a source + the scope it exposes) -----

/// Bind the FROM list. Empty FROM is a one-row, zero-column source (so
/// `SELECT <expr>` binds). Multiple comma-separated items are cross joins.
fn bind_from(
    items: &[TableWithJoins],
    binder: &mut Binder,
) -> Result<(LogicalNode, Scope), QueryError> {
    let [first, rest @ ..] = items else {
        return Ok((
            LogicalNode::Values {
                rows: vec![Tuple { values: vec![] }],
            },
            Scope::empty(),
        ));
    };

    let (mut node, mut scope) = bind_table_with_joins(first, binder)?;
    for item in rest {
        // `FROM a, b` is a cross join: every pair, no condition.
        let (rnode, rscope) = bind_table_with_joins(item, binder)?;
        let right_schema = rscope.schema();
        node = LogicalNode::Join {
            left: Box::new(node),
            right: Box::new(rnode),
            on: None,
            join_type: JoinType::Inner,
            right_schema,
        };
        scope = Scope::concat(scope, rscope);
    }
    Ok((node, scope))
}

/// A FROM item: a base relation followed by zero or more joins, folded
/// left-deep. Returns the source node and the combined scope.
fn bind_table_with_joins(
    twj: &TableWithJoins,
    binder: &mut Binder,
) -> Result<(LogicalNode, Scope), QueryError> {
    let (mut node, mut scope) = bind_table_factor(&twj.relation, binder)?;

    for join in &twj.joins {
        let (rnode, rscope) = bind_table_factor(&join.relation, binder)?;
        let (join_type, constraint) = join_kind(join)?;
        let right_schema = rscope.schema();
        // The output row — and so any column ref in ON — is `left ++ right`.
        let combined = Scope::concat(scope, rscope);
        let on = match constraint {
            Some(expr) => {
                // Bind ON against the combined scope.
                binder.push_scope(combined.clone());
                let bound = expr.bind(binder);
                binder.pop_scope();
                Some(bound?)
            }
            None => None,
        };
        node = LogicalNode::Join {
            left: Box::new(node),
            right: Box::new(rnode),
            on,
            join_type,
            right_schema,
        };
        scope = combined;
    }
    Ok((node, scope))
}

/// Map a parsed join to our `(JoinType, ON condition)`. Cross joins and a
/// bare join have no condition. `USING` / `NATURAL` and outer-apply-style
/// joins aren't supported yet.
fn join_kind(join: &Join) -> Result<(JoinType, Option<&AstExpr>), QueryError> {
    let (ty, constraint) = match &join.join_operator {
        // Postgres `JOIN`/`INNER` → `Join`/`Inner`; `LEFT [OUTER]` → `Left`/
        // `LeftOuter`; likewise right; `FULL [OUTER]` → `FullOuter`.
        JoinOperator::Join(c) | JoinOperator::Inner(c) => (JoinType::Inner, Some(c)),
        JoinOperator::Left(c) | JoinOperator::LeftOuter(c) => (JoinType::Left, Some(c)),
        JoinOperator::Right(c) | JoinOperator::RightOuter(c) => (JoinType::Right, Some(c)),
        JoinOperator::FullOuter(c) => (JoinType::Full, Some(c)),
        JoinOperator::CrossJoin(_) => (JoinType::Inner, None),
        other => {
            return Err(QueryError::unsupported(format!("join operator: {other:?}")));
        }
    };
    let on = match constraint {
        None | Some(JoinConstraint::None) => None,
        Some(JoinConstraint::On(expr)) => Some(expr),
        Some(other) => {
            return Err(QueryError::unsupported(format!(
                "join constraint: {other:?}"
            )));
        }
    };
    Ok((ty, on))
}

/// Bind one table factor (a base relation) to a source node and its scope,
/// tagging every column with the relation's alias or name for qualified refs.
fn bind_table_factor(
    factor: &TableFactor,
    binder: &mut Binder,
) -> Result<(LogicalNode, Scope), QueryError> {
    let TableFactor::Table { name, alias, .. } = factor else {
        return Err(QueryError::unsupported(format!(
            "FROM relation: {factor:?}"
        )));
    };
    let alias = alias.as_ref().map(|a| a.name.value.clone());

    // System-catalog relations resolve specially: an unqualified `pg_*` /
    // `st_*`, or a qualified `information_schema.x` / `pg_catalog.x`. They
    // carry no project.dataset prefix, unlike user tables.
    let parts = name_idents(name)?;
    let (dataset_opt, leaf) = match parts.as_slice() {
        [t] => (None, *t),
        [d, t] => (Some(*d), *t),
        [_, d, t] => (Some(*d), *t),
        _ => {
            return Err(QueryError::unsupported(format!(
                "name needs project.dataset.table, got: {name}"
            )));
        }
    };
    if let Some(relation) = SystemRelation::resolve(dataset_opt, leaf) {
        let schema = relation.schema();
        let rel = alias.unwrap_or_else(|| leaf.to_string());
        return Ok((
            LogicalNode::SystemScan { relation },
            Scope::for_relation(Some(rel), &schema),
        ));
    }

    // User tables: three-part name only for now — no session defaults.
    let (project, dataset, table_name) = three_part_name(name)?;
    let table = binder
        .ctx()
        .catalog()
        .resolve_table(project, dataset, table_name)?;
    let schema = table.schema().clone();
    let rel = alias.unwrap_or_else(|| table_name.to_string());
    Ok((
        LogicalNode::Scan { table },
        Scope::for_relation(Some(rel), &schema),
    ))
}

// --- One projection item — may expand (wildcard → many) --------------------

impl BindNode for SelectItem {
    type Output = Vec<Expr>;

    fn bind(&self, binder: &mut Binder) -> Result<Vec<Expr>, QueryError> {
        match self {
            SelectItem::UnnamedExpr(e) => Ok(vec![e.bind(binder)?]),
            SelectItem::ExprWithAlias { expr, .. } => Ok(vec![expr.bind(binder)?]),
            SelectItem::Wildcard(_) => {
                let scope = binder
                    .current_scope()
                    .ok_or_else(|| QueryError::Internal("no scope for `*`".into()))?;
                Ok((0..scope.len()).map(Expr::column).collect())
            }
            other => Err(QueryError::unsupported(format!(
                "projection item: {other:?}"
            ))),
        }
    }
}

// --- Scalar expression -----------------------------------------------------
