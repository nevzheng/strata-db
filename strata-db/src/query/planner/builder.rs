//! Builds a [`Planner`] by selecting an implementation for each phase
//! and registering optimizer rules.
//!
//! [`PlannerBuilder::default()`] wires the standard passes so callers
//! can customize a few slots without restating the rest;
//! [`Planner::builder()`] returns the same starting point.

use crate::query::QueryError;
use crate::query::stages::{AnalyzedQuery, LogicalQuery, ParsedQuery, PhysicalQuery, RawQuery};

use super::Planner;
use super::lower::Lower;
use super::pass::{Analyze, BuildLogical, OptimizeLogical, OptimizePhysical, Parse, Pass};
use super::rule::{LogicalRule, PhysicalRule};

#[derive(Debug)]
pub enum BuildError {
    MissingPass(&'static str),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::MissingPass(name) => write!(f, "planner: missing pass `{name}`"),
        }
    }
}

impl std::error::Error for BuildError {}

impl From<BuildError> for QueryError {
    fn from(e: BuildError) -> Self {
        QueryError::Internal(e.to_string())
    }
}

pub struct PlannerBuilder {
    parse: Option<Box<dyn Pass<Input = RawQuery, Output = ParsedQuery>>>,
    analyze: Option<Box<dyn Pass<Input = ParsedQuery, Output = AnalyzedQuery>>>,
    build_logical: Option<Box<dyn Pass<Input = AnalyzedQuery, Output = LogicalQuery>>>,
    logical_rules: Vec<Box<dyn LogicalRule>>,
    lower: Option<Box<dyn Pass<Input = LogicalQuery, Output = PhysicalQuery>>>,
    physical_rules: Vec<Box<dyn PhysicalRule>>,
}

impl PlannerBuilder {
    pub fn new() -> Self {
        Self {
            parse: None,
            analyze: None,
            build_logical: None,
            logical_rules: Vec::new(),
            lower: None,
            physical_rules: Vec::new(),
        }
    }

    pub fn parse<P>(mut self, pass: P) -> Self
    where
        P: Pass<Input = RawQuery, Output = ParsedQuery> + 'static,
    {
        self.parse = Some(Box::new(pass));
        self
    }

    pub fn analyze<P>(mut self, pass: P) -> Self
    where
        P: Pass<Input = ParsedQuery, Output = AnalyzedQuery> + 'static,
    {
        self.analyze = Some(Box::new(pass));
        self
    }

    pub fn build_logical<P>(mut self, pass: P) -> Self
    where
        P: Pass<Input = AnalyzedQuery, Output = LogicalQuery> + 'static,
    {
        self.build_logical = Some(Box::new(pass));
        self
    }

    pub fn logical_rule<R>(mut self, rule: R) -> Self
    where
        R: LogicalRule + 'static,
    {
        self.logical_rules.push(Box::new(rule));
        self
    }

    pub fn logical_rules<I, R>(mut self, rules: I) -> Self
    where
        I: IntoIterator<Item = R>,
        R: LogicalRule + 'static,
    {
        for rule in rules {
            self.logical_rules.push(Box::new(rule));
        }
        self
    }

    pub fn lower<P>(mut self, pass: P) -> Self
    where
        P: Pass<Input = LogicalQuery, Output = PhysicalQuery> + 'static,
    {
        self.lower = Some(Box::new(pass));
        self
    }

    pub fn physical_rule<R>(mut self, rule: R) -> Self
    where
        R: PhysicalRule + 'static,
    {
        self.physical_rules.push(Box::new(rule));
        self
    }

    pub fn physical_rules<I, R>(mut self, rules: I) -> Self
    where
        I: IntoIterator<Item = R>,
        R: PhysicalRule + 'static,
    {
        for rule in rules {
            self.physical_rules.push(Box::new(rule));
        }
        self
    }

    pub fn build(self) -> Result<Planner, BuildError> {
        Ok(Planner {
            parse: self.parse.ok_or(BuildError::MissingPass("parse"))?,
            analyze: self.analyze.ok_or(BuildError::MissingPass("analyze"))?,
            build_logical: self
                .build_logical
                .ok_or(BuildError::MissingPass("build_logical"))?,
            optimize_logical: OptimizeLogical::new(self.logical_rules),
            lower: self.lower.ok_or(BuildError::MissingPass("lower"))?,
            optimize_physical: OptimizePhysical::new(self.physical_rules),
        })
    }
}

impl Default for PlannerBuilder {
    fn default() -> Self {
        Self::new()
            .parse(Parse)
            .analyze(Analyze)
            .build_logical(BuildLogical)
            .lower(Lower)
    }
}
