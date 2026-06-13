//! Typed query stages that flow through the planner pipeline.
//!
//! Each [`Pass`](crate::query::planner::pass::Pass) takes the previous
//! stage and produces the next, so the order is checked at the type
//! level — you can't accidentally hand a `RawQuery` to the lowerer.

use crate::sql::Statement;

use super::logical_plan::LogicalPlan;
use super::physical_plan::PhysicalPlan;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RawQuery {
    pub sql: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ParsedQuery {
    pub sql: String,
    pub ast: Vec<Statement>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AnalyzedQuery {
    pub sql: String,
    pub ast: Vec<Statement>,
    pub logical: Vec<LogicalPlan>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LogicalQuery {
    pub sql: String,
    pub ast: Vec<Statement>,
    pub logical: Vec<LogicalPlan>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PhysicalQuery {
    pub sql: String,
    pub ast: Vec<Statement>,
    pub logical: Vec<LogicalPlan>,
    pub physical: Vec<PhysicalPlan>,
}
