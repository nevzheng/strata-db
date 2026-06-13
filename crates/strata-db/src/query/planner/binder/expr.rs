//! Binding for scalar expressions: column refs, literals, operators,
//! function calls, and typed-string literals.

use sqlparser::ast::{
    BinaryOperator as AstBinaryOperator, CaseWhen, CeilFloorKind, DataType, DateTimeField,
    Expr as AstExpr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, ObjectName,
    TrimWhereField, TypedString, UnaryOperator, Value as AstValue,
};

use rust_decimal::Decimal;
use uuid::Uuid;

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
                // Runtime arithmetic negation — works on any operand,
                // not just constant literals.
                UnaryOperator::Minus => Ok(Expr::Neg {
                    input: Box::new(expr.bind(binder)?),
                }),
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
            // `IS NULL` / `IS NOT NULL` — total predicates (never NULL).
            AstExpr::IsNull(e) => Ok(Expr::IsNull {
                expr: Box::new(e.bind(binder)?),
                negated: false,
            }),
            AstExpr::IsNotNull(e) => Ok(Expr::IsNull {
                expr: Box::new(e.bind(binder)?),
                negated: true,
            }),
            // BETWEEN / IN lower to expressions we already have; the
            // 3-valued-logic falls out of the comparison + AND/OR/NOT.
            AstExpr::Between {
                expr,
                negated,
                low,
                high,
            } => bind_between(expr, low, high, *negated, binder),
            AstExpr::InList {
                expr,
                list,
                negated,
            } => bind_in_list(expr, list, *negated, binder),
            AstExpr::Like {
                negated,
                any,
                expr,
                pattern,
                escape_char,
            } => {
                if *any || escape_char.is_some() {
                    return Err(QueryError::unsupported("LIKE ANY / ESCAPE"));
                }
                Ok(Expr::Like {
                    expr: Box::new(expr.bind(binder)?),
                    pattern: Box::new(pattern.bind(binder)?),
                    negated: *negated,
                })
            }
            // SUBSTRING / TRIM are their own AST nodes (not `Function`).
            AstExpr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => bind_substring(expr, substring_from, substring_for, binder),
            AstExpr::Trim {
                expr,
                trim_where,
                trim_what,
                trim_characters,
            } => bind_trim(expr, trim_where, trim_what, trim_characters, binder),
            AstExpr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => bind_case(operand, conditions, else_result, binder),
            AstExpr::Cast {
                expr, data_type, ..
            } => Ok(Expr::Cast {
                input: Box::new(expr.bind(binder)?),
                target: super::ddl::bind_data_type(data_type)?,
            }),
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

/// `x BETWEEN low AND high` lowered to `x >= low AND x <= high` (negated
/// wraps the result in `NOT`). `x` is bound once and cloned into both
/// bounds.
fn bind_between(
    expr: &AstExpr,
    low: &AstExpr,
    high: &AstExpr,
    negated: bool,
    binder: &mut Binder,
) -> Result<Expr, QueryError> {
    let value = expr.bind(binder)?;
    let ge = Expr::binary(BinaryOperator::GtEq, value.clone(), low.bind(binder)?);
    let le = Expr::binary(BinaryOperator::LtEq, value, high.bind(binder)?);
    let between = Expr::binary(BinaryOperator::And, ge, le);
    Ok(if negated {
        Expr::negate(between)
    } else {
        between
    })
}

/// `x IN (a, b, …)` lowered to `x = a OR x = b OR …`; an empty list is
/// `false` (so `NOT IN ()` is `true`). NULL semantics match Postgres for
/// free — the OR-chain of comparisons already does 3-valued logic.
fn bind_in_list(
    expr: &AstExpr,
    list: &[AstExpr],
    negated: bool,
    binder: &mut Binder,
) -> Result<Expr, QueryError> {
    let value = expr.bind(binder)?;
    let mut acc: Option<Expr> = None;
    for item in list {
        let eq = Expr::binary(BinaryOperator::Eq, value.clone(), item.bind(binder)?);
        acc = Some(match acc {
            None => eq,
            Some(prev) => Expr::binary(BinaryOperator::Or, prev, eq),
        });
    }
    let in_expr = acc.unwrap_or_else(|| Expr::lit(false));
    Ok(if negated {
        Expr::negate(in_expr)
    } else {
        in_expr
    })
}

/// `SUBSTRING(s [FROM start] [FOR len])` → a 3-arg `Substr` call with the
/// SQL defaults filled in: `start = 1`, `len = i64::MAX` ("to the end").
fn bind_substring(
    expr: &AstExpr,
    from: &Option<Box<AstExpr>>,
    for_: &Option<Box<AstExpr>>,
    binder: &mut Binder,
) -> Result<Expr, QueryError> {
    let s = expr.bind(binder)?;
    let start = match from {
        Some(e) => e.bind(binder)?,
        None => Expr::lit(1i64),
    };
    let len = match for_ {
        Some(e) => e.bind(binder)?,
        None => Expr::lit(i64::MAX),
    };
    Ok(Expr::Call {
        func: ScalarFunc::Substr,
        args: vec![s, start, len],
    })
}

/// `TRIM([BOTH|LEADING|TRAILING] [chars] FROM s)` → a 2-arg trim call,
/// defaulting the side to BOTH and the trim set to a single space. The
/// dialect-specific `TRIM(s, chars)` characters list isn't supported yet.
fn bind_trim(
    expr: &AstExpr,
    trim_where: &Option<TrimWhereField>,
    trim_what: &Option<Box<AstExpr>>,
    trim_characters: &Option<Vec<AstExpr>>,
    binder: &mut Binder,
) -> Result<Expr, QueryError> {
    if trim_characters.is_some() {
        return Err(QueryError::unsupported("TRIM(value, characters)"));
    }
    let func = match trim_where {
        None | Some(TrimWhereField::Both) => ScalarFunc::TrimBoth,
        Some(TrimWhereField::Leading) => ScalarFunc::TrimLeading,
        Some(TrimWhereField::Trailing) => ScalarFunc::TrimTrailing,
    };
    let s = expr.bind(binder)?;
    let chars = match trim_what {
        Some(e) => e.bind(binder)?,
        None => Expr::lit(" "),
    };
    Ok(Expr::Call {
        func,
        args: vec![s, chars],
    })
}

/// Bind a `CASE`. A simple `CASE x WHEN v THEN …` is lowered to searched
/// form by turning each branch condition into `x = v`, so the evaluator
/// only ever sees boolean conditions.
fn bind_case(
    operand: &Option<Box<AstExpr>>,
    conditions: &[CaseWhen],
    else_result: &Option<Box<AstExpr>>,
    binder: &mut Binder,
) -> Result<Expr, QueryError> {
    let operand = operand.as_deref().map(|o| o.bind(binder)).transpose()?;
    let mut branches = Vec::with_capacity(conditions.len());
    for when in conditions {
        let cond = match &operand {
            Some(op) => Expr::binary(BinaryOperator::Eq, op.clone(), when.condition.bind(binder)?),
            None => when.condition.bind(binder)?,
        };
        branches.push((cond, when.result.bind(binder)?));
    }
    let else_result = else_result
        .as_deref()
        .map(|e| e.bind(binder))
        .transpose()?
        .map(Box::new);
    Ok(Expr::Case {
        branches,
        else_result,
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
        ("NULLIF", 2) => Ok(ScalarFunc::Nullif),
        ("NULLIF", n) => Err(QueryError::type_error(format!(
            "NULLIF takes exactly 2 arguments, got {n}"
        ))),
        ("CONCAT", _) => Ok(ScalarFunc::Concat),
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
        DataType::Time(_, tz) if is_tz_aware(tz) => {
            Err(QueryError::unsupported("TIME WITH TIME ZONE literal"))
        }
        DataType::Time(_, _) => {
            let micros =
                temporal::parse_time(&as_string("TIME")?).map_err(QueryError::type_error)?;
            Ok(Expr::Literal {
                value: Value::Time(micros),
            })
        }
        DataType::Uuid => {
            let s = as_string("UUID")?;
            let u = Uuid::parse_str(s.trim())
                .map_err(|_| QueryError::type_error(format!("invalid UUID literal: {s:?}")))?;
            Ok(Expr::Literal {
                value: Value::Uuid(u),
            })
        }
        other => Err(QueryError::unsupported(format!("typed literal: {other:?}"))),
    }
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
        AstBinaryOperator::StringConcat => BinaryOperator::Concat,
        other => return Err(QueryError::unsupported(format!("binary op: {other:?}"))),
    })
}
