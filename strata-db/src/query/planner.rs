//! The planner: SQL string → executable plan.
//!
//! Pipeline:
//!
//! ```text
//! sql ─► parse ─► bind ─► optimize ─► lower ─► PhysicalPlan
//! ```
//!
//! Each phase is a free function that takes a [`Query`] by `&mut`,
//! pushes it one stage forward, and returns `()`. The `Query` itself
//! is plain data: it accumulates every artifact the engine produces
//! along the way (original SQL, AST, logical plan, physical plan) and
//! is JSON-serializable so it can be persisted in the `_queries`
//! system table after the fact.
//!
//! Plans are data, the planner is the only place behavior lives. That
//! split is what lets `EXPLAIN`, durable query records, and replay all
//! share the same representation.

use crate::sql::Statement;

use super::QueryContext;
use super::QueryError;
use super::logical_plan::LogicalPlan;
use super::physical_plan::PhysicalPlan;

/// Where a [`Query`] is in the planning pipeline. The current variant
/// also implies which `Option<...>` fields on `Query` are populated;
/// it's explicit so callers (EXPLAIN, the queries table, an executor
/// dispatch) have one thing to switch on rather than reading the
/// shape of every field.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Stage {
    /// Just the SQL string. Nothing has run yet.
    Created,
    /// AST is populated.
    Parsed,
    /// Logical plan is populated. Covers both initial binding and any
    /// subsequent optimizer rewrites — optimization is a transform on
    /// the same artifact, not a new one.
    Bound,
    /// Physical plan is populated; the query is executable.
    Lowered,
}

/// The unit of work the engine plans, executes, and records. Built
/// incrementally as each phase runs; meant to be cheap to inspect, log,
/// and (eventually) write to the `_queries` system table.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Query {
    /// Original SQL text. Source of truth — everything else can be
    /// rebuilt from it.
    pub sql: String,
    /// Where we are in the pipeline.
    pub stage: Stage,
    /// Parsed statements (from `sqlparser`). `None` before `parse`.
    pub ast: Option<Vec<Statement>>,
    /// Logical plan after binding (and any optimizer passes). `None`
    /// before `bind`.
    pub logical_plan: Option<LogicalPlan>,
    /// Physical plan after lowering. `None` before `lower`.
    pub physical_plan: Option<PhysicalPlan>,
}

impl Query {
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            stage: Stage::Created,
            ast: None,
            logical_plan: None,
            physical_plan: None,
        }
    }
}

/// Drive a SQL string through the full planning pipeline. Returns the
/// completed [`Query`]; if any phase errors, the partially-built query
/// is dropped (we'll surface it via the error path once we decide what
/// "failure" looks like on the queries table).
pub fn plan(sql: &str, ctx: &mut QueryContext<'_>) -> Result<Query, QueryError> {
    let mut query = Query::new(sql);
    parse(&mut query)?;
    bind(&mut query, ctx)?;
    optimize(&mut query)?;
    lower(&mut query)?;
    Ok(query)
}

fn parse(_query: &mut Query) -> Result<(), QueryError> {
    todo!("parse: sql -> ast; set stage = Parsed")
}

fn bind(_query: &mut Query, _ctx: &mut QueryContext<'_>) -> Result<(), QueryError> {
    todo!("bind: ast -> logical_plan via catalog scans; set stage = Bound")
}

fn optimize(_query: &mut Query) -> Result<(), QueryError> {
    todo!("optimize: rewrite logical_plan in place; stage stays Bound")
}

fn lower(_query: &mut Query) -> Result<(), QueryError> {
    todo!("lower: logical_plan -> physical_plan; set stage = Lowered")
}
