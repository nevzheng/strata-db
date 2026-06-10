//! Scalar expressions.
//!
//! A tree that computes a [`Value`] from an input [`Tuple`]. Lives
//! inside plan nodes that need per-row logic — `Filter` holds a
//! predicate, `Project` holds an expression per output column.
//!
//! Expressions are *data*, not closures. That's the whole reason for
//! this representation: the optimizer can push them down past joins, a
//! pretty-printer can render them, a future JIT can walk the tree to
//! emit code. Closures would be opaque to all of that.

use crate::storage::types::{Tuple, Value};

use super::QueryError;

/// Binary operators. These are the *constants* of the expression
/// language — the verbs that combine two sub-expressions into one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BinaryOperator {
    // Comparison
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    // Logical
    And,
    Or,
}

/// A scalar expression. Each variant is a "basic operation" carrying
/// its own data.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Expr {
    /// Reference column `index` in the input tuple.
    Column { index: usize },
    /// A constant value.
    Literal { value: Value },
    /// A binary operation on two sub-expressions.
    Binary {
        op: BinaryOperator,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// Logical negation.
    Not { input: Box<Expr> },
}

impl Expr {
    pub fn column(index: usize) -> Self {
        Expr::Column { index }
    }

    pub fn lit(value: impl Into<Value>) -> Self {
        Expr::Literal {
            value: value.into(),
        }
    }

    pub fn binary(op: BinaryOperator, lhs: Expr, rhs: Expr) -> Self {
        Expr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }

    pub fn negate(input: Expr) -> Self {
        Expr::Not {
            input: Box::new(input),
        }
    }

    /// Evaluate the expression against `tuple`.
    ///
    /// Null semantics follow SQL three-valued logic where it's cheap:
    /// `NULL` propagates through comparisons (`NULL = x` → `NULL`), and
    /// `AND` / `OR` short-circuit on `false` / `true` even with the
    /// other side `NULL`; otherwise they return `NULL`.
    ///
    /// Comparisons coerce across integer widths (so a column compares
    /// against an `Int64` literal); a non-numeric type mismatch is a
    /// [`QueryError::Type`] surfaced to the user.
    pub fn eval(&self, tuple: &Tuple) -> Result<Value, QueryError> {
        match self {
            Expr::Column { index } => tuple.values.get(*index).cloned().ok_or_else(|| {
                QueryError::Internal(format!(
                    "column index {index} out of bounds (arity {})",
                    tuple.values.len()
                ))
            }),
            Expr::Literal { value } => Ok(value.clone()),
            Expr::Not { input } => match input.eval(tuple)? {
                Value::Null => Ok(Value::Null),
                Value::Bool(b) => Ok(Value::Bool(!b)),
                other => Err(QueryError::Internal(format!(
                    "NOT expects Bool, got {other:?}"
                ))),
            },
            Expr::Binary { op, lhs, rhs } => {
                let l = lhs.eval(tuple)?;
                let r = rhs.eval(tuple)?;
                eval_binary(*op, l, r)
            }
        }
    }
}

fn eval_binary(op: BinaryOperator, lhs: Value, rhs: Value) -> Result<Value, QueryError> {
    use BinaryOperator::*;

    // SQL 3VL short-circuit for AND/OR before null propagation.
    match (op, &lhs, &rhs) {
        (And, Value::Bool(false), _) | (And, _, Value::Bool(false)) => {
            return Ok(Value::Bool(false));
        }
        (Or, Value::Bool(true), _) | (Or, _, Value::Bool(true)) => {
            return Ok(Value::Bool(true));
        }
        _ => {}
    }

    if matches!(lhs, Value::Null) || matches!(rhs, Value::Null) {
        return Ok(Value::Null);
    }

    match op {
        Eq => Ok(Value::Bool(values_eq(&lhs, &rhs))),
        NotEq => Ok(Value::Bool(!values_eq(&lhs, &rhs))),
        Lt => Ok(Value::Bool(cmp_values(&lhs, &rhs)?.is_lt())),
        LtEq => Ok(Value::Bool(cmp_values(&lhs, &rhs)?.is_le())),
        Gt => Ok(Value::Bool(cmp_values(&lhs, &rhs)?.is_gt())),
        GtEq => Ok(Value::Bool(cmp_values(&lhs, &rhs)?.is_ge())),
        And => match (lhs, rhs) {
            (Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a && b)),
            (l, r) => Err(QueryError::Internal(format!(
                "AND expects Bool, got {l:?} / {r:?}"
            ))),
        },
        Or => match (lhs, rhs) {
            (Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a || b)),
            (l, r) => Err(QueryError::Internal(format!(
                "OR expects Bool, got {l:?} / {r:?}"
            ))),
        },
    }
}

/// The integer value of `v`, widened to `i64`, if it is any integer
/// type. Lets comparisons mix integer widths — notably a column against
/// a SQL literal, which always binds as `Int64`.
fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int16(n) => Some(*n as i64),
        Value::Int32(n) => Some(*n as i64),
        Value::Int64(n) => Some(*n),
        _ => None,
    }
}

/// Equality with numeric coercion: any two integers compare by value
/// regardless of width; otherwise fall back to structural equality
/// (`Text == Text`, `Bytes == Bytes`, mismatched types → not equal).
fn values_eq(lhs: &Value, rhs: &Value) -> bool {
    match (as_i64(lhs), as_i64(rhs)) {
        (Some(a), Some(b)) => a == b,
        _ => lhs == rhs,
    }
}

/// Total order for ordered comparisons. Integers compare across widths
/// (coerced to `i64`); same-typed bools and text compare directly.
/// Anything else — including a non-numeric vs numeric mix — is a type
/// error the caller surfaces to the user.
fn cmp_values(lhs: &Value, rhs: &Value) -> Result<std::cmp::Ordering, QueryError> {
    if let (Some(a), Some(b)) = (as_i64(lhs), as_i64(rhs)) {
        return Ok(a.cmp(&b));
    }
    match (lhs, rhs) {
        (Value::Bool(a), Value::Bool(b)) => Ok(a.cmp(b)),
        (Value::Text(a), Value::Text(b)) => Ok(a.cmp(b)),
        (l, r) => Err(QueryError::type_error(format!(
            "cannot compare {l:?} and {r:?}"
        ))),
    }
}

// Conveniences for building literals.
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<i16> for Value {
    fn from(v: i16) -> Self {
        Value::Int16(v)
    }
}
impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Int32(v)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int64(v)
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(values: Vec<Value>) -> Tuple {
        Tuple { values }
    }

    #[test]
    fn eval_column_and_literal() {
        let tuple = t(vec![Value::Int32(10), Value::Text("hi".into())]);
        assert_eq!(Expr::column(0).eval(&tuple).unwrap(), Value::Int32(10));
        assert_eq!(Expr::lit(7i32).eval(&tuple).unwrap(), Value::Int32(7));
    }

    #[test]
    fn eval_comparison() {
        let tuple = t(vec![Value::Int32(10)]);
        let pred = Expr::binary(BinaryOperator::Lt, Expr::column(0), Expr::lit(20i32));
        assert_eq!(pred.eval(&tuple).unwrap(), Value::Bool(true));
    }

    #[test]
    fn eval_compares_across_integer_widths() {
        // An Int32 column against an Int64 literal (how SQL literals bind).
        let tuple = t(vec![Value::Int32(2)]);
        let eq = Expr::binary(BinaryOperator::Eq, Expr::column(0), Expr::lit(2i64));
        assert_eq!(eq.eval(&tuple).unwrap(), Value::Bool(true));
        let gt = Expr::binary(BinaryOperator::Gt, Expr::column(0), Expr::lit(1i64));
        assert_eq!(gt.eval(&tuple).unwrap(), Value::Bool(true));
    }

    #[test]
    fn eval_non_numeric_comparison_mismatch_is_type_error() {
        let tuple = t(vec![Value::Text("x".into())]);
        let pred = Expr::binary(BinaryOperator::Lt, Expr::column(0), Expr::lit(5i64));
        assert!(matches!(pred.eval(&tuple), Err(QueryError::Type(_))));
    }

    #[test]
    fn eval_and_short_circuits_on_null() {
        // false AND NULL -> false (3VL short-circuit, even though one side is null)
        let expr = Expr::binary(
            BinaryOperator::And,
            Expr::lit(false),
            Expr::lit(Value::Null),
        );
        assert_eq!(expr.eval(&t(vec![])).unwrap(), Value::Bool(false));
    }

    #[test]
    fn eval_null_propagates_through_comparison() {
        let expr = Expr::binary(BinaryOperator::Eq, Expr::lit(Value::Null), Expr::lit(1i32));
        assert_eq!(expr.eval(&t(vec![])).unwrap(), Value::Null);
    }

    #[test]
    fn eval_type_mismatch_is_error() {
        // bool vs int: not coercible, so a type error.
        let expr = Expr::binary(BinaryOperator::Lt, Expr::lit(true), Expr::lit(1i32));
        assert!(matches!(expr.eval(&t(vec![])), Err(QueryError::Type(_))));
    }

    #[test]
    fn eval_column_index_out_of_bounds_is_error() {
        let tuple = t(vec![Value::Int32(0)]);
        let res = Expr::column(5).eval(&tuple);
        assert!(matches!(res, Err(QueryError::Internal(_))));
    }
}
