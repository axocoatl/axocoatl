//! Keep as PR: commit a Session's run changes to a new branch with host git
//! (an alternate index; the person's checkout, index and branch are never
//! changed), and, opt-in, push that branch and open a pull request with
//! host `gh`. Never force-pushes; never pushes to the default branch.
//!
//! Owner: workstream `keep`.

use serde::{Deserialize, Serialize};

/// Branch names Keep creates: `axocoatl/<loadout>-<run id prefix>`.
pub const KEEP_BRANCH_PREFIX: &str = "axocoatl/";

/// `POST /api/sessions/{id}/keep-pr`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeepPrRequest {
    /// The run whose changes to keep; its Outcome supplies the PR body.
    pub run_id: String,
    /// Default `axocoatl/<loadout>-<run id prefix>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Push the branch and open a PR (otherwise branch + commit only).
    #[serde(default)]
    pub open_pr: bool,
    /// The remote to push to; default `origin`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// Commit message subject; default from the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// What Keep as PR did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepPrResponse {
    pub branch: String,
    pub commit: String,
    /// Paths committed.
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request_url: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum KeepPrError {
    #[error("keep as PR: not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("keep as PR: {0}")]
    Refused(String),
    #[error("keep as PR: {0}")]
    Git(String),
}

/// The pull request body for a run's Outcome: check results, review verdict
/// and adjudications, not-covered list, record id.
pub fn pr_body(
    _outcome: &axocoatl_session::run_outcome::RunOutcome,
) -> Result<String, KeepPrError> {
    Err(KeepPrError::NotImplemented("keep_pr::pr_body"))
}
