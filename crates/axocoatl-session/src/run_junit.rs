//! JUnit XML of a run's Outcome, for CI (`axocoatl run --junit <file>` and
//! `GET /api/runs/{run_id}/junit`). The shape is fixed by the spec
//! ("JUnit shape"): one `<testsuites name="axocoatl">` with the suites
//! `checks`, one `check:<name>` per check report, `review`, `adjudications`,
//! `findings` and `coverage`.
//!
//! Owner: workstream `core`.

use crate::run_outcome::RunOutcome;

/// Largest JUnit document produced, in bytes; test cases past it are
/// summarized in one `truncated` test case.
pub const MAX_JUNIT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum JunitError {
    #[error("JUnit: not implemented: {0}")]
    NotImplemented(&'static str),
}

/// Render `outcome` as JUnit XML.
pub fn render_junit(_outcome: &RunOutcome) -> Result<String, JunitError> {
    Err(JunitError::NotImplemented("render_junit"))
}
