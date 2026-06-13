//! Pipeline passes.
//!
//! Each phase of the planner is a [`Pass`] — a typed step that consumes
//! one [`stages`](crate::query::stages) struct and produces the next.
//! The driver in [`super`] strings them together; the type system keeps
//! the order honest.

use crate::query::QueryContext;
use crate::query::QueryError;
use crate::query::stages::{AnalyzedQuery, LogicalQuery, ParsedQuery, PhysicalQuery, RawQuery};

use super::binder::Bind;
use super::rule::{LogicalRule, PhysicalRule};

pub trait Pass {
    type Input;
    type Output;

    fn name(&self) -> &'static str;
    fn run(&self, input: Self::Input, ctx: &QueryContext<'_>) -> Result<Self::Output, QueryError>;
}

/// Umbrella for the analysis phase. Chains substages — today only
/// [`Bind`], but later steps (type-check, etc.) plug in here without
/// changing the planner's top-level shape.
pub struct Analyze;

impl Pass for Analyze {
    type Input = ParsedQuery;
    type Output = AnalyzedQuery;

    fn name(&self) -> &'static str {
        "analyze"
    }

    fn run(&self, input: ParsedQuery, ctx: &QueryContext<'_>) -> Result<AnalyzedQuery, QueryError> {
        Bind.run(input, ctx)
    }
}

pub struct Parse;

impl Pass for Parse {
    type Input = RawQuery;
    type Output = ParsedQuery;

    fn name(&self) -> &'static str {
        "parse"
    }

    fn run(&self, input: RawQuery, _ctx: &QueryContext<'_>) -> Result<ParsedQuery, QueryError> {
        let ast = crate::sql::parse(&input.sql)?;
        Ok(ParsedQuery {
            sql: input.sql,
            ast,
        })
    }
}

/// Type-only reshape: the binding work happened in [`Bind`], so this
/// pass just lifts the logical plans out of [`AnalyzedQuery`] into the
/// next stage. Exists so the optimize-logical phase has a [`LogicalQuery`]
/// to read and so the pipeline stages stay one-to-one with the passes.
pub struct BuildLogical;

impl Pass for BuildLogical {
    type Input = AnalyzedQuery;
    type Output = LogicalQuery;

    fn name(&self) -> &'static str {
        "build_logical"
    }

    fn run(
        &self,
        input: AnalyzedQuery,
        _ctx: &QueryContext<'_>,
    ) -> Result<LogicalQuery, QueryError> {
        Ok(LogicalQuery {
            sql: input.sql,
            ast: input.ast,
            logical: input.logical,
        })
    }
}

/// No-op today — holds the (currently empty) logical rewrite rule set
/// so a future pass can iterate without changing the pipeline shape.
pub struct OptimizeLogical {
    rules: Vec<Box<dyn LogicalRule>>,
}

impl OptimizeLogical {
    pub fn new(rules: Vec<Box<dyn LogicalRule>>) -> Self {
        Self { rules }
    }
}

impl Default for OptimizeLogical {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl Pass for OptimizeLogical {
    type Input = LogicalQuery;
    type Output = LogicalQuery;

    fn name(&self) -> &'static str {
        "optimize_logical"
    }

    fn run(
        &self,
        input: LogicalQuery,
        _ctx: &QueryContext<'_>,
    ) -> Result<LogicalQuery, QueryError> {
        let _ = &self.rules;
        Ok(input)
    }
}

/// No-op today — symmetric to [`OptimizeLogical`] for the physical tree.
pub struct OptimizePhysical {
    rules: Vec<Box<dyn PhysicalRule>>,
}

impl OptimizePhysical {
    pub fn new(rules: Vec<Box<dyn PhysicalRule>>) -> Self {
        Self { rules }
    }
}

impl Default for OptimizePhysical {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl Pass for OptimizePhysical {
    type Input = PhysicalQuery;
    type Output = PhysicalQuery;

    fn name(&self) -> &'static str {
        "optimize_physical"
    }

    fn run(
        &self,
        input: PhysicalQuery,
        _ctx: &QueryContext<'_>,
    ) -> Result<PhysicalQuery, QueryError> {
        let _ = &self.rules;
        Ok(input)
    }
}
