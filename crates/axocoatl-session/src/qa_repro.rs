//! The QA explorer's report (findings with reproductions, and coverage) and
//! the classification of each reproduction.
//!
//! The explorer's final answer carries two fenced JSON blocks headed
//! `FINDINGS` and `COVERAGE` (shapes in the spec, "qa"). The host re-runs
//! each reproduction with `browser_check` against the build under test and,
//! when configured, the reference build, and classifies it with
//! [`classify`].
//!
//! Owner: workstream `review-qa`.

use serde::{Deserialize, Serialize};

use crate::run_outcome::{ReproClassification, ReproRun, Severity};

pub const FINDINGS_HEADING: &str = "FINDINGS";
pub const COVERAGE_HEADING: &str = "COVERAGE";
/// Most findings one explorer report may carry.
pub const MAX_QA_FINDINGS: usize = 200;

/// One finding as the explorer reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportedFinding {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub area: Option<String>,
    #[serde(default)]
    pub severity: Option<Severity>,
    pub expected: String,
    pub actual: String,
    /// Repository path of the reproduction, under the loadout's `repro_dir`.
    pub repro: Option<String>,
}

/// One area's coverage as the explorer reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageEntry {
    pub area: String,
    /// `covered`, `not_reached` or `blocked`.
    pub status: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExplorerReport {
    pub findings: Vec<ReportedFinding>,
    pub coverage: Vec<CoverageEntry>,
}

#[derive(Debug, thiserror::Error)]
pub enum QaReportError {
    #[error("qa report: not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("qa report: {0}")]
    Invalid(String),
}

/// Read the explorer's `FINDINGS` and `COVERAGE` blocks.
pub fn parse_explorer_report(_answer: &str) -> Result<ExplorerReport, QaReportError> {
    Err(QaReportError::NotImplemented("parse_explorer_report"))
}

/// Classify a reproduction from its run on the build under test and, when
/// configured, on the reference build. This is the contract the spec fixes.
pub fn classify(target: &ReproRun, reference: Option<&ReproRun>) -> ReproClassification {
    match (target.status.as_str(), reference.map(|r| r.status.as_str())) {
        ("error", _) | ("failed", Some("error")) => ReproClassification::ReproError,
        ("passed", _) => ReproClassification::NotReproduced,
        ("failed", None) => ReproClassification::Reproduced,
        ("failed", Some("passed")) => ReproClassification::Confirmed,
        ("failed", Some("failed")) => ReproClassification::FailsOnCleanBuild,
        _ => ReproClassification::ReproError,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: &str) -> ReproRun {
        ReproRun {
            base_url: "http://localhost:3000".into(),
            status: status.into(),
            first_error: None,
        }
    }

    #[test]
    fn classification_follows_the_reference_rule() {
        use ReproClassification::*;
        assert_eq!(classify(&run("failed"), Some(&run("passed"))), Confirmed);
        assert_eq!(
            classify(&run("failed"), Some(&run("failed"))),
            FailsOnCleanBuild
        );
        assert_eq!(classify(&run("failed"), None), Reproduced);
        assert_eq!(
            classify(&run("passed"), Some(&run("passed"))),
            NotReproduced
        );
        assert_eq!(classify(&run("error"), Some(&run("passed"))), ReproError);
        assert_eq!(classify(&run("failed"), Some(&run("error"))), ReproError);
    }
}
