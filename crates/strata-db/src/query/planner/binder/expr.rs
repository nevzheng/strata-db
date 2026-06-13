//! Binding for scalar expressions: column refs, literals, operators,
//! function calls, and typed-string literals.

use sqlparser::ast::{
    BinaryOperator as AstBinaryOperator, CeilFloorKind, DataType, DateTimeField, Expr as AstExpr,
    Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, ObjectName, TypedString,
    UnaryOperator, Value as AstValue,
};

use rust_decimal::Decimal;

use crate::query::QueryError;
use crate::query::expression::{BinaryOperator, Expr, ScalarFunc};
use crate::storage::temporal;
use crate::storage::types::Value;

use super::{BindNode, Binder, is_tz_aware};

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
            AstExpr::Function(func) => bind_function(func, binder),
            // sqlparser models CEIL/FLOOR as their own nodes (they accept
            // `CEIL(x TO field)`), so they don't arrive as `Function`.
            AstExpr::Ceil { expr, field } => bind_ceil_floor(expr, field, ScalarFunc::Ceil, binder),
            AstExpr::Floor { expr, field } => {
                bind_ceil_floor(expr, field, ScalarFunc::Floor, binder)
            }
            other => Err(QueryError::unsupported(format!("expression: {other:?}"))),
        }
    }
}

/// Bind plain `CEIL(x)` / `FLOOR(x)`. The `TO field` and scale forms
/// (`CEIL(x TO HOUR)`, `CEIL(x, 2)`) aren't supported yet.
fn bind_ceil_floor(
    expr: &AstExpr,
    field: &CeilFloorKind,
    func: ScalarFunc,
    binder: &mut Binder,
) -> Result<Expr, QueryError> {
    if !matches!(
        field,
        CeilFloorKind::DateTimeField(DateTimeField::NoDateTime)
    ) {
        return Err(QueryError::unsupported("CEIL/FLOOR with TO / scale"));
    }
    Ok(Expr::Call {
        func,
        args: vec![expr.bind(binder)?],
    })
}

/// Bind a function call to an [`Expr::Call`]. Only plain scalar calls are
/// supported — no `FILTER`, `OVER` (window), `DISTINCT`/`ORDER BY` in the
/// argument list, or named/wildcard arguments.
fn bind_function(func: &Function, binder: &mut Binder) -> Result<Expr, QueryError> {
    let name = single_ident_upper(&func.name)?;
    if func.filter.is_some()
        || func.over.is_some()
        || func.null_treatment.is_some()
        || !func.within_group.is_empty()
    {
        return Err(QueryError::unsupported(format!("{name} with a clause")));
    }
    let FunctionArguments::List(list) = &func.args else {
        return Err(QueryError::unsupported(format!("{name} call form")));
    };
    if list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
        return Err(QueryError::unsupported(format!(
            "{name} with DISTINCT / ORDER BY"
        )));
    }

    let mut args = Vec::with_capacity(list.args.len());
    for arg in &list.args {
        let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg else {
            return Err(QueryError::unsupported(format!("{name} argument form")));
        };
        args.push(expr.bind(binder)?);
    }

    let resolved = resolve_scalar_func(&name, args.len())?;
    Ok(Expr::Call {
        func: resolved,
        args,
    })
}

/// Resolve a function name + arity to a scalar function. `MAX`/`MIN` with
/// two or more arguments are the row-wise `GREATEST`/`LEAST`; the one-arg
/// aggregate form needs `GROUP BY`, which doesn't exist yet.
fn resolve_scalar_func(name: &str, argc: usize) -> Result<ScalarFunc, QueryError> {
    match (name, argc) {
        ("ABS", 1) => Ok(ScalarFunc::Abs),
        ("ABS", n) => Err(QueryError::type_error(format!(
            "ABS takes exactly 1 argument, got {n}"
        ))),
        ("GREATEST", n) | ("MAX", n) if n >= 2 => Ok(ScalarFunc::Greatest),
        ("LEAST", n) | ("MIN", n) if n >= 2 => Ok(ScalarFunc::Least),
        ("GREATEST", _) => Ok(ScalarFunc::Greatest), // single-arg GREATEST is valid
        ("LEAST", _) => Ok(ScalarFunc::Least),
        ("MAX" | "MIN", _) => Err(QueryError::unsupported(format!(
            "{name} as an aggregate (needs GROUP BY); {name}(a, b, …) is supported"
        ))),
        // CEIL/FLOOR arrive as their own AST nodes (see `bind_ceil_floor`),
        // not here. ROUND(x, n) (round to n places) isn't supported yet.
        ("ROUND", 1) => Ok(ScalarFunc::Round),
        ("COALESCE", n) if n >= 1 => Ok(ScalarFunc::Coalesce),
        ("LENGTH" | "CHAR_LENGTH" | "CHARACTER_LENGTH", 1) => Ok(ScalarFunc::Length),
        ("REPEAT", 2) => Ok(ScalarFunc::Repeat),
        ("UPPER" | "UCASE", 1) => Ok(ScalarFunc::Upper),
        ("LOWER" | "LCASE", 1) => Ok(ScalarFunc::Lower),
        // Recognized names with the wrong arity get a clear arity error.
        ("ROUND" | "LENGTH" | "UPPER" | "LOWER", n) => Err(QueryError::type_error(format!(
            "{name} takes exactly 1 argument, got {n}"
        ))),
        ("REPEAT", n) => Err(QueryError::type_error(format!(
            "REPEAT takes exactly 2 arguments, got {n}"
        ))),
        _ => Err(QueryError::unsupported(format!("function {name}"))),
    }
}

/// Extract a single, unqualified function name, upper-cased for matching.
fn single_ident_upper(name: &ObjectName) -> Result<String, QueryError> {
    match name.0.as_slice() {
        [part] => part
            .as_ident()
            .map(|i| i.value.to_uppercase())
            .ok_or_else(|| QueryError::unsupported(format!("function name: {name}"))),
        _ => Err(QueryError::unsupported(format!(
            "qualified function name: {name}"
        ))),
    }
}

/// Bind a typed string literal: `DATE '…'`, `TIMESTAMP WITH TIME ZONE '…'`,
/// or `NUMERIC '…'`. The string is parsed and validated here, so a bad
/// literal fails at bind time. Other typed strings are unsupported.
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
        DataType::Numeric(_)
        | DataType::Decimal(_)
        | DataType::Dec(_)
        | DataType::BigNumeric(_)
        | DataType::BigDecimal(_) => {
            let s = as_string("NUMERIC")?;
            let d = s
                .parse::<Decimal>()
                .map_err(|_| QueryError::type_error(format!("invalid NUMERIC literal: {s:?}")))?;
            Ok(Expr::Literal {
                value: Value::Numeric(d),
            })
        }
        other => Err(QueryError::unsupported(format!("typed literal: {other:?}"))),
    }
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
        Expr::Literal {
            value: Value::Numeric(d),
        } => Value::Numeric(-d),
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
