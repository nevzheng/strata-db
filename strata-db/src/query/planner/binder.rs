//! AST → logical plans.
//!
//! Each sqlparser AST type implements [`Bind`] with its corresponding
//! output. [`Binder`] carries shared state (catalog access, scope
//! stack) and drives the bind phase on a [`Query`].

use sqlparser::ast::{
    BinaryOperator as AstBinaryOperator, Expr as AstExpr, GroupByExpr, Ident, Query as AstQuery,
    Select, SelectItem, SetExpr, Statement, TableFactor, TableWithJoins, UnaryOperator,
    Value as AstValue,
};

use crate::catalog::schema::Schema;
use crate::storage::types::{Tuple, Value};

use super::super::expression::{BinaryOperator, Expr};
use super::super::logical_plan::{LogicalNode, LogicalPlan};
use super::super::{Query, QueryContext, QueryError, QueryStage};

pub(super) struct Binder<'a, 'db> {
    ctx: &'a QueryContext<'db>,
    /// Stack of binding scopes. Each entry describes the columns
    /// visible at one nesting level (one outer query, one subquery,
    /// etc.). `current_scope()` returns the identifiers that resolve
    /// right now — pushed when we enter a FROM, popped when we leave.
    ///
    /// Modeled as `Schema` today; once we add joins / aliases / CTEs,
    /// this grows into a richer `Scope` struct.
    scopes: Vec<Schema>,
}

impl<'a, 'db> Binder<'a, 'db> {
    pub(super) fn new(ctx: &'a QueryContext<'db>) -> Self {
        Self {
            ctx,
            scopes: Vec::new(),
        }
    }

    pub(super) fn run(&mut self, query: &mut Query) -> Result<(), QueryError> {
        if query.logical_plan.is_some() {
            return Ok(());
        }
        if query.stage != QueryStage::Parsed {
            return Err(QueryError::Internal(format!(
                "bind requires parsed query, got stage {:?}",
                query.stage
            )));
        }
        let plans = Bind::bind(&*query, self)?;
        query.logical_plan = Some(plans);
        query.stage = QueryStage::Bound;
        Ok(())
    }

    fn push_scope(&mut self, schema: Schema) {
        self.scopes.push(schema);
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn current_scope(&self) -> Option<&Schema> {
        self.scopes.last()
    }

    fn ctx(&self) -> &QueryContext<'db> {
        self.ctx
    }
}

pub(super) trait Bind {
    type Output;
    fn bind(&self, binder: &mut Binder) -> Result<Self::Output, QueryError>;
}

// --- Query: iterate statements --------------------------------------------

impl Bind for Query {
    type Output = Vec<LogicalPlan>;

    fn bind(&self, binder: &mut Binder) -> Result<Vec<LogicalPlan>, QueryError> {
        self.ast
            .as_ref()
            .ok_or_else(|| QueryError::Internal("bind: ast missing".into()))?
            .iter()
            .map(|stmt| stmt.bind(binder))
            .collect()
    }
}

// --- Statement dispatch ----------------------------------------------------

impl Bind for Statement {
    type Output = LogicalPlan;

    fn bind(&self, binder: &mut Binder) -> Result<LogicalPlan, QueryError> {
        match self {
            Statement::Query(q) => q.bind(binder),
            other => Err(QueryError::unsupported(format!("statement: {other:?}"))),
        }
    }
}

// --- Query → SetExpr → Select ----------------------------------------------

impl Bind for AstQuery {
    type Output = LogicalPlan;

    fn bind(&self, binder: &mut Binder) -> Result<LogicalPlan, QueryError> {
        let SetExpr::Select(select) = self.body.as_ref() else {
            return Err(QueryError::unsupported(format!(
                "query body: {:?}",
                self.body
            )));
        };
        Ok(LogicalPlan::new(select.bind(binder)?))
    }
}

// --- SELECT body: FROM + WHERE + projection --------------------------------

impl Bind for Select {
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

impl Bind for TableWithJoins {
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
        let parts: Vec<&str> = name
            .0
            .iter()
            .map(|p| p.as_ident().map(|i| i.value.as_str()))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| QueryError::unsupported(format!("non-identifier in name: {name}")))?;
        let (project, dataset, table_name) = match parts.as_slice() {
            [p, d, t] => (*p, *d, *t),
            _ => {
                return Err(QueryError::unsupported(format!(
                    "FROM needs project.dataset.table, got: {name}"
                )));
            }
        };
        let table = binder
            .ctx()
            .catalog()
            .resolve_table(project, dataset, table_name)?;
        let schema = table.schema().clone();
        Ok((LogicalNode::Scan { table }, schema))
    }
}

// --- One projection item — may expand (wildcard → many) --------------------

impl Bind for SelectItem {
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

impl Bind for AstExpr {
    type Output = Expr;

    fn bind(&self, binder: &mut Binder) -> Result<Expr, QueryError> {
        match self {
            AstExpr::Value(v) => Ok(Expr::Literal {
                value: bind_value(&v.value)?,
            }),
            AstExpr::Identifier(ident) => resolve_column(ident, binder),
            AstExpr::BinaryOp { op, left, right } => Ok(Expr::Binary {
                op: bind_binary_op(op)?,
                lhs: Box::new(left.bind(binder)?),
                rhs: Box::new(right.bind(binder)?),
            }),
            AstExpr::UnaryOp { op, expr } => match op {
                UnaryOperator::Not => Ok(Expr::Not {
                    input: Box::new(expr.bind(binder)?),
                }),
                other => Err(QueryError::unsupported(format!("unary op: {other:?}"))),
            },
            AstExpr::Nested(inner) => inner.bind(binder),
            other => Err(QueryError::unsupported(format!("expression: {other:?}"))),
        }
    }
}

fn bind_value(v: &AstValue) -> Result<Value, QueryError> {
    match v {
        AstValue::Number(s, _) => s
            .parse::<i64>()
            .map(Value::Int64)
            .map_err(|_| QueryError::Internal(format!("bad numeric literal: {s}"))),
        AstValue::SingleQuotedString(s) => Ok(Value::Text(s.clone())),
        AstValue::Boolean(b) => Ok(Value::Bool(*b)),
        AstValue::Null => Ok(Value::Null),
        other => Err(QueryError::unsupported(format!("literal: {other:?}"))),
    }
}

fn resolve_column(ident: &Ident, binder: &Binder) -> Result<Expr, QueryError> {
    let scope = binder
        .current_scope()
        .ok_or_else(|| QueryError::Internal("no scope for column ref".into()))?;
    let name = ident.value.as_str();
    let index = scope
        .fields
        .iter()
        .position(|f| f.name.as_str() == name)
        .ok_or_else(|| QueryError::Internal(format!("unknown column: {name}")))?;
    Ok(Expr::column(index))
}

fn bind_binary_op(op: &AstBinaryOperator) -> Result<BinaryOperator, QueryError> {
    Ok(match op {
        AstBinaryOperator::Eq => BinaryOperator::Eq,
        AstBinaryOperator::NotEq => BinaryOperator::NotEq,
        AstBinaryOperator::Lt => BinaryOperator::Lt,
        AstBinaryOperator::LtEq => BinaryOperator::LtEq,
        AstBinaryOperator::Gt => BinaryOperator::Gt,
        AstBinaryOperator::GtEq => BinaryOperator::GtEq,
        AstBinaryOperator::And => BinaryOperator::And,
        AstBinaryOperator::Or => BinaryOperator::Or,
        other => return Err(QueryError::unsupported(format!("binary op: {other:?}"))),
    })
}
