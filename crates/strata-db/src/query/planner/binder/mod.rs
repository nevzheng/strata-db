//! Bind substage + the AST walk that later phases reuse.
//!
//! Two things live here:
//! - [`BindNode`], a trait implemented across the sqlparser AST so each
//!   node produces its logical-plan fragment, with [`Binder`] carrying
//!   the shared catalog handle and scope stack.
//! - [`Bind`], one substage of analysis, run from
//!   [`Analyze`](super::pass::Analyze).
//!
//! The per-construct binding logic lives in focused submodules — [`ddl`]
//! (CREATE), [`dml`] (INSERT), [`query`] (SELECT), and [`expr`] (scalar
//! expressions). This module owns the `Binder`, the trait, statement
//! dispatch, the shared name helper, and the analysis [`Pass`].

mod ddl;
mod dml;
mod expr;
mod query;
mod scope;

use sqlparser::ast::{ObjectName, Statement, TimezoneInfo};

use crate::catalog::consts::{DEFAULT_DATASET_NAME, DEFAULT_PROJECT_NAME};
use crate::query::logical_plan::LogicalPlan;
use crate::query::stages::{AnalyzedQuery, ParsedQuery};
use crate::query::{QueryContext, QueryError};

use super::pass::Pass;
use scope::Scope;

pub(super) struct Binder<'a, 'db> {
    ctx: &'a QueryContext<'db>,
    /// Stack of binding scopes. Each entry describes the columns
    /// visible at one nesting level (one outer query, one subquery,
    /// etc.). `current_scope()` returns the identifiers that resolve
    /// right now — pushed when we enter a FROM, popped when we leave.
    scopes: Vec<Scope>,
}

impl<'a, 'db> Binder<'a, 'db> {
    pub(super) fn new(ctx: &'a QueryContext<'db>) -> Self {
        Self {
            ctx,
            scopes: Vec::new(),
        }
    }

    fn push_scope(&mut self, scope: Scope) {
        self.scopes.push(scope);
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn current_scope(&self) -> Option<&Scope> {
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
            Statement::Insert(insert) => dml::bind_insert(insert, binder),
            Statement::CreateTable(ct) => ddl::bind_create_table(ct, binder),
            Statement::CreateSchema {
                schema_name,
                if_not_exists,
                with,
                options,
                default_collate_spec,
                ..
            } => ddl::bind_create_schema(
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

/// The identifier segments of an object name (`a.b.c` → `[a, b, c]`).
/// Errors `unsupported` if any segment isn't a plain identifier.
fn name_idents(name: &ObjectName) -> Result<Vec<&str>, QueryError> {
    name.0
        .iter()
        .map(|p| p.as_ident().map(|i| i.value.as_str()))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| QueryError::unsupported(format!("non-identifier in name: {name}")))
}

/// Resolve a (possibly under-qualified) table name to `(project, dataset,
/// table)`, filling missing leading parts from the session search path —
/// fixed at [`DEFAULT_PROJECT_NAME`].[`DEFAULT_DATASET_NAME`] (`strata.public`)
/// for now. So a bare `t` is `strata.public.t`, `d.t` is `strata.d.t`, and a
/// full `p.d.t` is itself. This is what lets a pgwire client address `public.t`
/// (or just `t`) on connect. Shared by the DDL, DML, and query binders.
/// (Per-session `SET search_path` is future work.)
fn qualify_table_name(name: &ObjectName) -> Result<(&str, &str, &str), QueryError> {
    match name_idents(name)?.as_slice() {
        [t] => Ok((DEFAULT_PROJECT_NAME, DEFAULT_DATASET_NAME, *t)),
        [d, t] => Ok((DEFAULT_PROJECT_NAME, *d, *t)),
        [p, d, t] => Ok((*p, *d, *t)),
        _ => Err(QueryError::unsupported(format!(
            "table name has too many parts (max project.dataset.table): {name}"
        ))),
    }
}

/// Whether a SQL `TIMESTAMP`/`TIME` carries a time zone — the only
/// timestamp flavor we model (stored as a UTC instant). Shared by the
/// type mapping (DDL) and the typed-literal binder (expr).
fn is_tz_aware(tz: &TimezoneInfo) -> bool {
    matches!(tz, TimezoneInfo::WithTimeZone | TimezoneInfo::Tz)
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
