//! Reports a required check writes (JUnit XML, or tester-army/e2e's
//! `report.json`), parsed into the Outcome's check results.
//!
//! A report counts only when it is bound to the exact check run: the check
//! prints `AXOCOATL-CHECK-REPORT sha256=<hex>` as its last stdout line, and
//! the host reads the file from the Session container and accepts it only
//! when its SHA-256 matches the line in the run's recorded stdout.
//!
//! Owner: workstream `e2e`.

use crate::run_outcome::CheckReport;

/// Prefix of the stdout line that binds a report to its check run.
pub const REPORT_MARKER_PREFIX: &str = "AXOCOATL-CHECK-REPORT sha256=";
/// Largest report read, in bytes.
pub const MAX_REPORT_BYTES: usize = 4 * 1024 * 1024;
/// Most test cases kept per report.
pub const MAX_REPORT_TESTS: usize = 2_000;

#[derive(Debug, thiserror::Error)]
pub enum CheckReportError {
    #[error("check report: not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("check report: {0}")]
    Invalid(String),
}

/// The digest named by the last marker line of `stdout`, when there is one.
pub fn report_marker(_stdout: &str) -> Option<String> {
    None
}

/// Parse a JUnit XML document.
pub fn parse_junit(_bytes: &[u8]) -> Result<CheckReport, CheckReportError> {
    Err(CheckReportError::NotImplemented("parse_junit"))
}

/// Parse tester-army/e2e's `report.json`.
pub fn parse_e2e_report_json(_bytes: &[u8]) -> Result<CheckReport, CheckReportError> {
    Err(CheckReportError::NotImplemented("parse_e2e_report_json"))
}
