//! The audit planner's areas and the workers' and integrator's findings.
//!
//! The planner answers with one fenced JSON block headed `AREAS`:
//! `{"areas": [{"name": "...", "scope": "...", "paths": ["src/**"]}]}` with
//! 2 to 8 areas (the loadout's `min_areas`..`max_areas`). Each area worker
//! and the integrator answer with a `FINDINGS` block of
//! `[{"id", "title", "detail", "severity", "location", "area"}]` and the
//! worker a `NOT_REACHED` list.
//!
//! Owner: workstream `audit`.

use serde::{Deserialize, Serialize};

use crate::run_outcome::Finding;

pub const AREAS_HEADING: &str = "AREAS";
pub const NOT_REACHED_HEADING: &str = "NOT_REACHED";

/// One area of an audit plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditArea {
    /// `[a-z][a-z0-9-]{0,31}`, unique; becomes the worker's slot id suffix.
    pub name: String,
    pub scope: String,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditPlan {
    pub areas: Vec<AuditArea>,
}

/// One area worker's report.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AreaReport {
    pub findings: Vec<Finding>,
    pub not_reached: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuditPlanError {
    #[error("audit plan: not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("audit plan: {0}")]
    Invalid(String),
}

/// Read and validate the planner's `AREAS` block.
pub fn parse_plan(_answer: &str, _min: u32, _max: u32) -> Result<AuditPlan, AuditPlanError> {
    Err(AuditPlanError::NotImplemented("parse_plan"))
}

/// Read one area worker's report.
pub fn parse_area_report(_answer: &str, _area: &str) -> Result<AreaReport, AuditPlanError> {
    Err(AuditPlanError::NotImplemented("parse_area_report"))
}

/// Read the integrator's merged findings.
pub fn parse_integrated(_answer: &str) -> Result<Vec<Finding>, AuditPlanError> {
    Err(AuditPlanError::NotImplemented("parse_integrated"))
}
