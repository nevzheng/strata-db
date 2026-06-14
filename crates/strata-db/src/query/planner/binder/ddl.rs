//! Binding for DDL statements: `CREATE TABLE` and `CREATE SCHEMA`,
//! plus the column / type mapping they need.

use sqlparser::ast::{
    ArrayElemTypeDef, ColumnDef, ColumnOption, CreateTable as AstCreateTable, DataType,
    ExactNumberInfo, Expr as AstExpr, ObjectName, SchemaName, SqlOption,
};

use crate::catalog::consts::DEFAULT_PROJECT_NAME;
use crate::catalog::schema::Schema;
use crate::catalog::{CatalogError, ResourceKind};
use crate::query::QueryError;
use crate::query::logical_plan::{LogicalNode, LogicalPlan};
use crate::storage::types::{Field, FieldName, LogicalType};

use super::{BindNode, Binder, is_tz_aware, qualify_table_name};

// --- CREATE TABLE ----------------------------------------------------------

/// Bind a `CREATE TABLE` into a [`LogicalNode::CreateTable`]. The parent
/// project + dataset are resolved to ids here (read-side); the table row
/// is minted in the catalog at execution.
pub(super) fn bind_create_table(
    ct: &AstCreateTable,
    binder: &mut Binder,
) -> Result<LogicalPlan, QueryError> {
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

    let (project, dataset, table) = qualify_table_name(&ct.name)?;

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

    let schema = bind_schema(&ct.columns, binder)?;

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
// --- CREATE SCHEMA ---------------------------------------------------------

/// Bind a `CREATE SCHEMA` into a [`LogicalNode::CreateDataset`]. Follows
/// BigQuery: a schema *is* a dataset, named `[project.]dataset`. Anything
/// beyond the name (`WITH`, `OPTIONS`, `DEFAULT COLLATE`, authorization)
/// is rejected as unsupported.
pub(super) fn bind_create_schema(
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
fn bind_schema(columns: &[ColumnDef], binder: &mut Binder) -> Result<Schema, QueryError> {
    if columns.is_empty() {
        return Err(QueryError::unsupported("CREATE TABLE with no columns"));
    }
    let fields = columns
        .iter()
        .map(|col| bind_column(col, binder))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Schema { fields })
}

fn bind_column(col: &ColumnDef, binder: &mut Binder) -> Result<Field, QueryError> {
    let ty = bind_data_type(&col.data_type)?;
    // SQL columns are nullable unless `NOT NULL` is given. We accept the
    // null-related options and `DEFAULT`; anything else (generated
    // columns, inline PK/UNIQUE) is rejected so we don't silently ignore it.
    let mut nullable = true;
    let mut default = None;
    for opt in &col.options {
        match &opt.option {
            ColumnOption::Null => nullable = true,
            ColumnOption::NotNull => nullable = false,
            // Bind the default with no scope pushed: it can't reference
            // other columns (resolve_column fails without a scope) and
            // can't contain a subquery — matching Postgres' two
            // restrictions. We store the bound expression as JSON so the
            // storage layer stays free of any query-layer type; INSERT
            // deserializes and evaluates it per row.
            ColumnOption::Default(expr) => {
                let bound = expr.bind(binder)?;
                let json = serde_json::to_string(&bound).map_err(|e| {
                    QueryError::Internal(format!("serialize DEFAULT expression: {e}"))
                })?;
                default = Some(json);
            }
            other => {
                return Err(QueryError::unsupported(format!("column option: {other:?}")));
            }
        }
    }
    Ok(Field {
        name: FieldName::new(col.name.value.as_str()),
        ty,
        nullable,
        default,
    })
}

/// Map a SQL type to one of the engine's [`LogicalType`]s. Unknown or
/// unsupported types surface as `unsupported`.
pub(super) fn bind_data_type(ty: &DataType) -> Result<LogicalType, QueryError> {
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
        // Exact decimal. We don't enforce the declared precision/scale yet
        // (values keep their own scale); the backing decimal caps at ~28
        // significant digits.
        DataType::Numeric(_)
        | DataType::Decimal(_)
        | DataType::Dec(_)
        | DataType::BigNumeric(_)
        | DataType::BigDecimal(_) => LogicalType::Numeric,
        // TIME (without time zone) only; TIMETZ isn't modeled.
        DataType::Time(_, tz) if is_tz_aware(tz) => {
            return Err(QueryError::unsupported("TIME WITH TIME ZONE"));
        }
        DataType::Time(_, _) => LogicalType::Time,
        DataType::Uuid => LogicalType::Uuid,
        DataType::Interval { .. } => LogicalType::Interval,
        DataType::Array(def) => {
            let elem = match def {
                ArrayElemTypeDef::AngleBracket(t)
                | ArrayElemTypeDef::SquareBracket(t, _)
                | ArrayElemTypeDef::Parenthesis(t) => bind_data_type(t)?,
                ArrayElemTypeDef::None => {
                    return Err(QueryError::unsupported("ARRAY without an element type"));
                }
            };
            if matches!(elem, LogicalType::Array(_)) {
                return Err(QueryError::unsupported("nested arrays"));
            }
            LogicalType::Array(Box::new(elem))
        }
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
