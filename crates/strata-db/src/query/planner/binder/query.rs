//! Binding for `SELECT`: the query body, FROM relation, WHERE, and the
//! projection list.

use sqlparser::ast::{
    Expr as AstExpr, GroupByExpr, LimitClause, Query as AstQuery, Select, SelectItem, SetExpr,
    TableFactor, TableWithJoins,
};

use crate::catalog::schema::Schema;
use crate::query::QueryError;
use crate::query::expression::Expr;
use crate::query::logical_plan::{LogicalNode, LogicalPlan};
use crate::storage::types::{Tuple, Value};

use super::{BindNode, Binder, three_part_name};

impl BindNode for AstQuery {
    type Output = LogicalPlan;

    fn bind(&self, binder: &mut Binder) -> Result<LogicalPlan, QueryError> {
        // ORDER BY / FETCH would change the result; reject rather than
        // silently ignore them.
        if self.order_by.is_some() {
            return Err(QueryError::unsupported("ORDER BY"));
        }
        if self.fetch.is_some() {
            return Err(QueryError::unsupported("FETCH"));
        }

        let SetExpr::Select(select) = self.body.as_ref() else {
            return Err(QueryError::unsupported(format!(
                "query body: {:?}",
                self.body
            )));
        };
        let mut node = select.bind(binder)?;

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

impl BindNode for Select {
    type Output = LogicalNode;

    fn bind(&self, binder: &mut Binder) -> Result<LogicalNode, QueryError> {
        // Reject SQL features the engine doesn't implement yet, so they
        // surface as a clear `unsupported: <feature>` instead of silently
        // producing a wrong plan.
        if self.distinct.is_some() {
            return Err(QueryError::unsupported("DISTINCT"));
        }
        if !matches!(&self.group_by, GroupByExpr::Expressions(exprs, _) if exprs.is_empty()) {
            return Err(QueryError::unsupported("GROUP BY"));
        }
        if self.having.is_some() {
            return Err(QueryError::unsupported("HAVING"));
        }

        // 1. FROM → source + the schema it exposes. Empty FROM yields a
        // one-row, zero-column source so `SELECT <expr>` (no FROM) binds.
        let (source, scope) = match self.from.as_slice() {
            [] => (
                LogicalNode::Values {
                    rows: vec![Tuple { values: vec![] }],
                },
                Schema { fields: vec![] },
            ),
            [one] => one.bind(binder)?,
            _ => return Err(QueryError::unsupported("comma-separated FROM")),
        };

        binder.push_scope(scope);

        // 2. WHERE → wrap in Filter if a predicate is present.
        let after_where = match &self.selection {
            Some(pred) => LogicalNode::Filter {
                input: Box::new(source),
                predicate: pred.bind(binder)?,
            },
            None => source,
        };

        // 3. Projection → Project. Each `SelectItem` may expand to
        // multiple expressions (wildcards), hence flatten.
        let expressions: Vec<Expr> = self
            .projection
            .iter()
            .map(|item| item.bind(binder))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();

        binder.pop_scope();

        Ok(LogicalNode::Project {
            input: Box::new(after_where),
            expressions,
        })
    }
}

// --- FROM relation (produces a source + the schema it exposes) -------------

impl BindNode for TableWithJoins {
    type Output = (LogicalNode, Schema);

    fn bind(&self, binder: &mut Binder) -> Result<(LogicalNode, Schema), QueryError> {
        if !self.joins.is_empty() {
            return Err(QueryError::unsupported("joins"));
        }
        let TableFactor::Table { name, .. } = &self.relation else {
            return Err(QueryError::unsupported(format!(
                "FROM relation: {:?}",
                self.relation
            )));
        };
        // Three-part name only for now — no session defaults.
        let (project, dataset, table_name) = three_part_name(name)?;
        let table = binder
            .ctx()
            .catalog()
            .resolve_table(project, dataset, table_name)?;
        let schema = table.schema().clone();
        Ok((LogicalNode::Scan { table }, schema))
    }
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
                Ok((0..scope.fields.len()).map(Expr::column).collect())
            }
            other => Err(QueryError::unsupported(format!(
                "projection item: {other:?}"
            ))),
        }
    }
}

// --- Scalar expression -----------------------------------------------------
