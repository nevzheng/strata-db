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
    DataType, ExactNumberInfo, Expr as AstExpr, GroupByExpr, Ident, Insert, ObjectName,
    Query as AstQuery, SchemaName, Select, SelectItem, SetExpr, SqlOption, Statement, TableFactor,
    TableObject, TableWithJoins, TimezoneInfo, TypedString, UnaryOperator, Value as AstValue,
};

use crate::catalog::consts::DEFAULT_PROJECT_NAME;
use crate::catalog::schema::Schema;
use crate::catalog::{CatalogError, ResourceKind};
use crate::query::stages::{AnalyzedQuery, ParsedQuery};
use crate::storage::temporal;
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
            Statement::Insert(insert) => bind_insert(insert, binder),
            Statement::CreateTable(ct) => bind_create_table(ct, binder),
            Statement::CreateSchema {
                schema_name,
                if_not_exists,
                with,
                options,
                default_collate_spec,
                ..
            } => bind_create_schema(
                schema_name,
                *if_not_exists,
                with,
                options,
                default_collate_spec,
                binder,
            ),
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

// --- INSERT ----------------------------------------------------------------

/// Bind `INSERT INTO p.d.t VALUES (..), (..)` into an [`LogicalNode::Insert`]
/// over a [`LogicalNode::Values`] source. v1 limits: positional values only
/// (no column list), `VALUES` source only (no `INSERT ... SELECT`), constant
/// expressions only. Each value is validated and coerced against the table's
/// stored schema.
fn bind_insert(insert: &Insert, binder: &mut Binder) -> Result<LogicalPlan, QueryError> {
    if !insert.columns.is_empty() {
        return Err(QueryError::unsupported(
            "INSERT with an explicit column list",
        ));
    }
    let TableObject::TableName(name) = &insert.table else {
        return Err(QueryError::unsupported(
            "INSERT target must be a table name",
        ));
    };
    let (project, dataset, table_name) = three_part_name(name)?;
    let table = binder
        .ctx()
        .catalog()
        .resolve_table(project, dataset, table_name)?;
    let schema = table.schema().clone();

    let source = insert
        .source
        .as_ref()
        .ok_or_else(|| QueryError::unsupported("INSERT without VALUES"))?;
    let SetExpr::Values(values) = source.body.as_ref() else {
        return Err(QueryError::unsupported("INSERT source must be VALUES"));
    };

    let mut rows = Vec::with_capacity(values.rows.len());
    for row in &values.rows {
        let exprs = &row.content;
        if exprs.len() != schema.fields.len() {
            return Err(QueryError::type_error(format!(
                "INSERT has {} value(s) but table `{}` has {} column(s)",
                exprs.len(),
                table.name(),
                schema.fields.len()
            )));
        }
        let mut row_values = Vec::with_capacity(exprs.len());
        for (expr, field) in exprs.iter().zip(&schema.fields) {
            let value = const_eval(expr, binder)?;
            row_values.push(coerce_value(value, field)?);
        }
        rows.push(Tuple { values: row_values });
    }

    Ok(LogicalPlan::new(LogicalNode::Insert {
        table,
        input: Box::new(LogicalNode::Values { rows }),
    }))
}

/// Evaluate a `VALUES` expression to a constant. It binds with no scope,
/// so any column reference fails — `VALUES` rows are constants. Supports
/// literals and constant expressions (e.g. `1 = 1`); negative numeric
/// literals aren't supported yet (`-` binds as an unsupported unary op).
fn const_eval(expr: &AstExpr, binder: &mut Binder) -> Result<Value, QueryError> {
    let bound = expr.bind(binder)?;
    bound.eval(&Tuple { values: vec![] })
}

/// Validate and coerce `value` to `field`'s type. Same type passes;
/// integers widen freely and narrow with a range check; everything else
/// (cross-category, `NULL` into `NOT NULL`) is a type error.
fn coerce_value(value: Value, field: &Field) -> Result<Value, QueryError> {
    use LogicalType as T;
    match value {
        Value::Null if field.nullable => Ok(Value::Null),
        Value::Null => Err(QueryError::type_error(format!(
            "NULL into NOT NULL column `{}`",
            field.name.as_str()
        ))),
        Value::Bool(b) if matches!(field.ty, T::Bool) => Ok(Value::Bool(b)),
        Value::Date(d) if matches!(field.ty, T::Date) => Ok(Value::Date(d)),
        Value::Timestamp(t) if matches!(field.ty, T::Timestamp) => Ok(Value::Timestamp(t)),
        Value::Text(s) if matches!(field.ty, T::Text) => Ok(Value::Text(s)),
        Value::Bytes(b) if matches!(field.ty, T::Bytes) => Ok(Value::Bytes(b)),
        Value::Json(j) if matches!(field.ty, T::Json) => Ok(Value::Json(j)),
        Value::Int16(_) | Value::Int32(_) | Value::Int64(_)
            if matches!(field.ty, T::Int16 | T::Int32 | T::Int64) =>
        {
            let n = match value {
                Value::Int16(x) => x as i64,
                Value::Int32(x) => x as i64,
                Value::Int64(x) => x,
                _ => unreachable!(),
            };
            fit_int(n, field)
        }
        // Floats: exact-type passes; cross-width and int→float convert
        // (REAL narrows from DOUBLE/int, possibly to ±inf — same as SQL
        // assignment). Float→int is not implicit; it needs an explicit cast.
        Value::Float64(f) if matches!(field.ty, T::Float64) => Ok(Value::Float64(f)),
        Value::Float64(f) if matches!(field.ty, T::Float32) => Ok(Value::Float32(f as f32)),
        Value::Float32(f) if matches!(field.ty, T::Float32) => Ok(Value::Float32(f)),
        Value::Float32(f) if matches!(field.ty, T::Float64) => Ok(Value::Float64(f as f64)),
        Value::Int16(_) | Value::Int32(_) | Value::Int64(_)
            if matches!(field.ty, T::Float32 | T::Float64) =>
        {
            let n = match value {
                Value::Int16(x) => x as f64,
                Value::Int32(x) => x as f64,
                Value::Int64(x) => x as f64,
                _ => unreachable!(),
            };
            Ok(match field.ty {
                T::Float32 => Value::Float32(n as f32),
                _ => Value::Float64(n),
            })
        }
        other => Err(QueryError::type_error(format!(
            "cannot insert {other:?} into column `{}` of type {:?}",
            field.name.as_str(),
            field.ty
        ))),
    }
}

/// Fit an integer into an integer column: widening always succeeds, a
/// narrowing conversion that overflows is a type error.
fn fit_int(n: i64, field: &Field) -> Result<Value, QueryError> {
    let out_of_range = || {
        QueryError::type_error(format!(
            "value {n} out of range for column `{}` of type {:?}",
            field.name.as_str(),
            field.ty
        ))
    };
    match field.ty {
        LogicalType::Int64 => Ok(Value::Int64(n)),
        LogicalType::Int32 => i32::try_from(n)
            .map(Value::Int32)
            .map_err(|_| out_of_range()),
        LogicalType::Int16 => i16::try_from(n)
            .map(Value::Int16)
            .map_err(|_| out_of_range()),
        _ => unreachable!("fit_int is only called for integer columns"),
    }
}

// --- CREATE SCHEMA ---------------------------------------------------------

/// Bind a `CREATE SCHEMA` into a [`LogicalNode::CreateDataset`]. Follows
/// BigQuery: a schema *is* a dataset, named `[project.]dataset`. Anything
/// beyond the name (`WITH`, `OPTIONS`, `DEFAULT COLLATE`, authorization)
/// is rejected as unsupported.
fn bind_create_schema(
    schema_name: &SchemaName,
    if_not_exists: bool,
    with: &Option<Vec<SqlOption>>,
    options: &Option<Vec<SqlOption>>,
    default_collate_spec: &Option<AstExpr>,
    binder: &mut Binder,
) -> Result<LogicalPlan, QueryError> {
    if with.is_some() || options.is_some() || default_collate_spec.is_some() {
        return Err(QueryError::unsupported("CREATE SCHEMA options"));
    }
    let SchemaName::Simple(name) = schema_name else {
        return Err(QueryError::unsupported("CREATE SCHEMA AUTHORIZATION"));
    };

    let (project, dataset) = dataset_name(name)?;
    // The project must already exist — we create datasets, not projects
    // (matching BigQuery, where projects are provisioned out of band).
    let project_meta = binder
        .ctx()
        .catalog()
        .get_project(project)?
        .ok_or_else(|| CatalogError::NotFound {
            kind: ResourceKind::Project,
            name: project.to_string(),
        })?;

    Ok(LogicalPlan::new(LogicalNode::CreateDataset {
        project_id: project_meta.id,
        name: dataset.to_string(),
        if_not_exists,
    }))
}

/// Split a `CREATE SCHEMA` name into `(project, dataset)`. A bare
/// `dataset` resolves its project to [`DEFAULT_PROJECT_NAME`] — BigQuery's
/// "defaults to the project that runs this DDL statement".
fn dataset_name(name: &ObjectName) -> Result<(&str, &str), QueryError> {
    let parts: Vec<&str> = name
        .0
        .iter()
        .map(|p| p.as_ident().map(|i| i.value.as_str()))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| QueryError::unsupported(format!("non-identifier in name: {name}")))?;
    match parts.as_slice() {
        [d] => Ok((DEFAULT_PROJECT_NAME, *d)),
        [p, d] => Ok((*p, *d)),
        _ => Err(QueryError::unsupported(format!(
            "CREATE SCHEMA needs [project.]dataset, got: {name}"
        ))),
    }
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
        DataType::Date => LogicalType::Date,
        // Only the time-zone-aware variant is supported; bare `TIMESTAMP`
        // (`WITHOUT TIME ZONE`) is a distinct type we don't model yet.
        DataType::Timestamp(_, tz) if is_tz_aware(tz) => LogicalType::Timestamp,
        DataType::Timestamp(_, _) => {
            return Err(QueryError::unsupported(
                "TIMESTAMP WITHOUT TIME ZONE (use TIMESTAMP WITH TIME ZONE)",
            ));
        }
        // Postgres float sizing: REAL / FLOAT4 are 4-byte; DOUBLE
        // PRECISION / FLOAT8 are 8-byte; bare FLOAT is double precision.
        // FLOAT(p) is single precision for p ≤ 24, double otherwise.
        DataType::Real | DataType::Float4 | DataType::Float32 => LogicalType::Float32,
        DataType::DoublePrecision | DataType::Double(_) | DataType::Float8 | DataType::Float64 => {
            LogicalType::Float64
        }
        DataType::Float(info) => match float_precision(info) {
            Some(p) if p <= 24 => LogicalType::Float32,
            _ => LogicalType::Float64,
        },
        other => return Err(QueryError::unsupported(format!("column type: {other:?}"))),
    })
}

/// The declared precision of a SQL `FLOAT(p)`, if any.
fn float_precision(info: &ExactNumberInfo) -> Option<u64> {
    match info {
        ExactNumberInfo::None => None,
        ExactNumberInfo::Precision(p) | ExactNumberInfo::PrecisionAndScale(p, _) => Some(*p),
    }
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
                // Unary minus folds into a constant integer literal — we
                // have no runtime arithmetic operator to lower it to, so a
                // non-constant operand is unsupported.
                UnaryOperator::Minus => negate_literal(expr, binder),
                other => Err(QueryError::unsupported(format!("unary op: {other:?}"))),
            },
            AstExpr::Nested(inner) => inner.bind(binder),
            AstExpr::TypedString(ts) => bind_typed_string(ts),
            other => Err(QueryError::unsupported(format!("expression: {other:?}"))),
        }
    }
}

/// Bind a typed string literal such as `DATE '2026-06-13'`. Only `DATE`
/// is supported today; other temporal literals (`TIME`, `TIMESTAMP`) are
/// rejected until those types land. The string is parsed and validated
/// here, so a bad date fails at bind time.
fn bind_typed_string(ts: &TypedString) -> Result<Expr, QueryError> {
    let as_string = |what: &str| {
        ts.value
            .clone()
            .into_string()
            .ok_or_else(|| QueryError::type_error(format!("{what} literal must be a string")))
    };
    match &ts.data_type {
        DataType::Date => {
            let days = temporal::parse_date(&as_string("DATE")?).map_err(QueryError::type_error)?;
            Ok(Expr::Literal {
                value: Value::Date(days),
            })
        }
        DataType::Timestamp(_, tz) if is_tz_aware(tz) => {
            let micros = temporal::parse_timestamptz(&as_string("TIMESTAMP")?)
                .map_err(QueryError::type_error)?;
            Ok(Expr::Literal {
                value: Value::Timestamp(micros),
            })
        }
        other => Err(QueryError::unsupported(format!("typed literal: {other:?}"))),
    }
}

/// Whether a SQL `TIMESTAMP`/`TIME` carries a time zone — the only
/// timestamp flavor we model (stored as a UTC instant).
fn is_tz_aware(tz: &TimezoneInfo) -> bool {
    matches!(tz, TimezoneInfo::WithTimeZone | TimezoneInfo::Tz)
}

/// Bind unary minus by negating a constant numeric literal. Only literals
/// are foldable today; anything else (a column reference) is unsupported
/// until we grow an arithmetic operator. `i*::MIN` has no positive
/// magnitude and overflows on negation — that surfaces as a type error
/// rather than a panic.
fn negate_literal(expr: &AstExpr, binder: &mut Binder) -> Result<Expr, QueryError> {
    let overflow = || QueryError::type_error("integer literal out of range");
    let value = match expr.bind(binder)? {
        Expr::Literal {
            value: Value::Int16(n),
        } => Value::Int16(n.checked_neg().ok_or_else(overflow)?),
        Expr::Literal {
            value: Value::Int32(n),
        } => Value::Int32(n.checked_neg().ok_or_else(overflow)?),
        Expr::Literal {
            value: Value::Int64(n),
        } => Value::Int64(n.checked_neg().ok_or_else(overflow)?),
        Expr::Literal {
            value: Value::Float64(f),
        } => Value::Float64(-f),
        other => return Err(QueryError::unsupported(format!("unary minus on {other:?}"))),
    };
    Ok(Expr::Literal { value })
}

fn bind_value(v: &AstValue) -> Result<Value, QueryError> {
    match v {
        // Integers bind as Int64; a literal with a fractional part or
        // exponent (or one too large for i64) binds as Float64. (Real
        // Postgres makes `1.5` NUMERIC, but we have no NUMERIC type yet.)
        AstValue::Number(s, _) => {
            if let Ok(n) = s.parse::<i64>() {
                Ok(Value::Int64(n))
            } else if let Ok(f) = s.parse::<f64>() {
                Ok(Value::Float64(f))
            } else {
                Err(QueryError::Internal(format!("bad numeric literal: {s}")))
            }
        }
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
