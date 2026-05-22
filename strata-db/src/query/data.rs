//! The `Query` object: the unit of work that flows through the
//! planner, optimizer, and executor. Plain data, JSON-serializable so
//! it can be persisted in the `_queries` system table.

use crate::sql::Statement;

use super::logical_plan::LogicalPlan;
use super::physical_plan::PhysicalPlan;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum QueryStage {
    #[default]
    Created,
    Parsed,
    Bound,
    Optimized,
    Lowered,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Query {
    pub sql: String,
    pub stage: QueryStage,
    pub ast: Option<Vec<Statement>>,
    pub logical_plan: Option<LogicalPlan>,
    pub physical_plan: Option<PhysicalPlan>,
}

impl Query {
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            stage: QueryStage::Created,
            ast: None,
            logical_plan: None,
            physical_plan: None,
        }
    }
}
