//! Bind substage + the AST walk that later phases reuse.
//!
//! Two things live here:
//! - [`BindNode`], a trait implemented by each sqlparser AST type so it
//!   produces its logical-plan fragment, with [`Binder`] carrying the
//!   shared catalog handle and scope stack.
//! - [`Bind`], one substage of analysis, run from
//!   [`Analyze`](super::pass::Analyze). Today it forwards [`ParsedQuery`]
//!   into [`AnalyzedQuery`] unchanged; name and type resolution will
//!   land here. The AST → [`LogicalPlan`] walk above is consumed by
//!   [`BuildLogical`](super::pass::BuildLogical) downstream.

use sqlparser::ast::{
    BinaryOperator as AstBinaryOperator, ColumnDef, ColumnOption, CreateTable as AstCreateTable,
    DataType, Expr as AstExpr, GroupByExpr, Ident, ObjectName, Query as AstQuery, Select,
    SelectItem, SetExpr, Statement, TableFactor, TableWithJoins, UnaryOperator, Value as AstValue,
};

use crate::catalog::schema::Schema;
use crate::catalog::{CatalogError, ResourceKind};
use crate::query::stages::{AnalyzedQuery, ParsedQuery};
use crate::storage::types::{Field, FieldName, LogicalType, Tuple, Value};

use super::super::expression::{BinaryOperator, Expr};
use super::super::logical_plan::{LogicalNode, LogicalPlan};
use super::super::{QueryContext, QueryError};
use super::pass::Pass;

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

    fn push_scope(&mut self, schema: Schema) {
        self.scopes.push(schema);
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn current_scope(&self) -> Option<&Schema> {
        self.scopes.last()
    }

    pub(super) fn ctx(&self) -> &QueryContext<'db> {
        self.ctx
    }
}

pub(super) trait BindNode {
    type Output;
    fn bind(&self, binder: &mut Binder) -> Result<Self::Output, QueryError>;
}

// --- Statement dispatch ----------------------------------------------------

impl BindNode for Statement {
    type Output = LogicalPlan;

    fn bind(&self, binder: &mut Binder) -> Result<LogicalPlan, QueryError> {
        match self {
            Statement::Query(q) => q.bind(binder),
            Statement::CreateTable(ct) => bind_create_table(ct, binder),
            other => Err(QueryError::unsupported(format!("statement: {other:?}"))),
        }
    }
}

// --- CREATE TABLE ----------------------------------------------------------

/// Bind a `CREATE TABLE` into a [`LogicalNode::CreateTable`]. The parent
/// project + dataset are resolved to ids here (read-side); the table row
/// is minted in the catalog at execution.
fn bind_create_table(ct: &AstCreateTable, binder: &mut Binder) -> Result<LogicalPlan, QueryError> {
    // `CREATE TABLE ... AS SELECT` and `LIKE`/`CLONE` derive their shape
    // from another relation; we only support an explicit column list.
    if ct.query.is_some() {
        return Err(QueryError::unsupported("CREATE TABLE ... AS SELECT"));
    }
    if ct.like.is_some() || ct.clone.is_some() {
        return Err(QueryError::unsupported("CREATE TABLE ... LIKE / CLONE"));
    }
    // Table-level constraints (PRIMARY KEY, UNIQUE, …) aren't modeled
    // yet — column 0 is the PK by convention. Reject rather than drop.
    if !ct.constraints.is_empty() {
        return Err(QueryError::unsupported("table constraints"));
    }

    let (project, dataset, table) = three_part_name(&ct.name)?;

    // Resolve the parent project + dataset to ids; either missing is a
    // catalog NotFound, surfaced to the caller verbatim.
    let catalog = binder.ctx().catalog();
    let project_meta = catalog
        .get_project(project)?
        .ok_or_else(|| CatalogError::NotFound {
            kind: ResourceKind::Project,
            name: project.to_string(),
        })?;
    let dataset_meta = catalog
        .get_dataset(project_meta.id, dataset)?
        .ok_or_else(|| CatalogError::NotFound {
            kind: ResourceKind::Dataset,
            name: dataset.to_string(),
        })?;

    let schema = bind_schema(&ct.columns)?;

    Ok(LogicalPlan::new(LogicalNode::CreateTable {
        project_id: project_meta.id,
        dataset_id: dataset_meta.id,
        name: table.to_string(),
        schema,
        // `OR REPLACE` bumps the truncation id at execution; plain
        // `CREATE` errors if the table already exists.
        or_replace: ct.or_replace,
    }))
}

/// Turn the AST column list into a storage [`Schema`]. Field order is
/// preserved — column 0 is the primary key by convention.
fn bind_schema(columns: &[ColumnDef]) -> Result<Schema, QueryError> {
    if columns.is_empty() {
        return Err(QueryError::unsupported("CREATE TABLE with no columns"));
    }
    let fields = columns
        .iter()
        .map(bind_column)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Schema { fields })
}

fn bind_column(col: &ColumnDef) -> Result<Field, QueryError> {
    let ty = bind_data_type(&col.data_type)?;
    // SQL columns are nullable unless `NOT NULL` is given. We accept
    // only the null-related options for now; anything else (defaults,
    // generated columns, inline PK/UNIQUE) is rejected so we don't
    // silently ignore it.
    let mut nullable = true;
    for opt in &col.options {
        match &opt.option {
            ColumnOption::Null => nullable = true,
            ColumnOption::NotNull => nullable = false,
            other => {
                return Err(QueryError::unsupported(format!("column option: {other:?}")));
            }
        }
    }
    Ok(Field {
        name: FieldName::new(col.name.value.as_str()),
        ty,
        nullable,
    })
}

/// Map a SQL type to one of the engine's [`LogicalType`]s. Unknown or
/// unsupported types surface as `unsupported`.
fn bind_data_type(ty: &DataType) -> Result<LogicalType, QueryError> {
    Ok(match ty {
        DataType::Bool | DataType::Boolean => LogicalType::Bool,
        DataType::SmallInt(_) | DataType::Int2(_) => LogicalType::Int16,
        DataType::Int(_) | DataType::Integer(_) | DataType::Int4(_) => LogicalType::Int32,
        DataType::BigInt(_) | DataType::Int8(_) => LogicalType::Int64,
        DataType::Text | DataType::Varchar(_) | DataType::Char(_) | DataType::CharVarying(_) => {
            LogicalType::Text
        }
        DataType::Bytea => LogicalType::Bytes,
        DataType::JSON | DataType::JSONB => LogicalType::Json,
        other => return Err(QueryError::unsupported(format!("column type: {other:?}"))),
    })
}

/// Split a `project.dataset.table` object name into its three parts.
/// Errors `unsupported` if it isn't exactly three identifier segments —
/// we have no session defaults to fill in shorter names.
fn three_part_name(name: &ObjectName) -> Result<(&str, &str, &str), QueryError> {
    let parts: Vec<&str> = name
        .0
        .iter()
        .map(|p| p.as_ident().map(|i| i.value.as_str()))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| QueryError::unsupported(format!("non-identifier in name: {name}")))?;
    match parts.as_slice() {
        [p, d, t] => Ok((*p, *d, *t)),
        _ => Err(QueryError::unsupported(format!(
            "name needs project.dataset.table, got: {name}"
        ))),
    }
}

// --- Query → SetExpr → Select ----------------------------------------------

impl BindNode for AstQuery {
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

impl BindNode for AstExpr {
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

// --- Analysis Pass ---------------------------------------------------------

/// Analysis substage in the planner pipeline. Forwards the parsed AST
/// into [`AnalyzedQuery`] for now; name/type resolution will land here
/// once we grow an annotated AST.
pub(super) struct Bind;

impl Pass for Bind {
    type Input = ParsedQuery;
    type Output = AnalyzedQuery;

    fn name(&self) -> &'static str {
        "bind"
    }

    fn run(&self, input: ParsedQuery, ctx: &QueryContext<'_>) -> Result<AnalyzedQuery, QueryError> {
        let mut binder = Binder::new(ctx);
        let logical: Vec<LogicalPlan> = input
            .ast
            .iter()
            .map(|stmt| stmt.bind(&mut binder))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(AnalyzedQuery {
            sql: input.sql,
            ast: input.ast,
            logical,
        })
    }
}
