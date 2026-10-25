//! Keep as PR: commit a Session's run changes to a new branch with host git
//! (an alternate index; the person's checkout, index and branch are never
//! changed), and, opt-in, push that branch and open a pull request with
//! host `gh`. Never force-pushes; never pushes to the default branch.
//!
//! What is committed is exactly the run's attributed paths: every path the
//! run's Agents changed according to their repository captures (Before and
//! After of each activation), that differs from `HEAD` in the working tree.
//! A path that had uncommitted changes before the run (the run manifest's
//! `dirty_paths`) is refused rather than mixed in. Other changes in the
//! working tree are reported as not committed. A submodule the run moved is
//! committed as the commit it holds, only as the run's receipt covers it; a
//! run whose repository holds a nested Git checkout (not a submodule), whose
//! files no capture reads, is refused.
//!
//! Host `git` and `gh` run with the person's own credentials; Axocoatl reads
//! none. Nothing a repository can configure runs: see `git_host`.
//!
//! When the run has a signed receipt (docs/design/1.4-receipts.md), Keep
//! commits only what it covers: each path it is about to commit must have
//! the content the receipt gives it (or, for a removed path, be absent). The
//! commit carries the trailers `Axocoatl-Run` and `Axocoatl-Receipt`, and the
//! pull request body a Receipt section and the receipt itself in an
//! `<!-- axocoatl-receipt:v1 … -->` block that the body's cut never cuts.
//!
//! Owner: workstream `keep`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use axocoatl_core::SecureDir;
use axocoatl_session::receipt::{self as receipt_v1, OpenedReceipt};
use axocoatl_session::run_outcome::{
    AdjudicationDecision, CheckState, FindingSource, KeepResult, ReproClassification,
    ReviewVerdictKind, RunOutcome, RunVerdict, Severity, RUN_OUTCOME_SCHEMA,
};
use axocoatl_session::run_record::{RunEvent, RunManifest, RunRecordError, RunRecordStore};
use serde::{Deserialize, Serialize};

use crate::git_host::{
    self, gh_command, plain_git, repository_git, run_bounded, run_bounded_with_input, HostTools,
    ProtectedGit, ProtectedGitSpec, KEEP_GH_TIMEOUT, KEEP_LOCAL_TIMEOUT, KEEP_NETWORK_TIMEOUT,
};

/// Branch names Keep creates: `axocoatl/<loadout>-<run id prefix>`.
pub const KEEP_BRANCH_PREFIX: &str = "axocoatl/";
/// The `RunEvent::Phase` name of a Keep result; its `detail` is the
/// [`KeepResult`] as JSON ([`keep_event`], [`keep_result_of`]).
pub use axocoatl_session::run_record::KEEP_PHASE;
/// The remote Keep pushes to when the request names none.
pub const DEFAULT_REMOTE: &str = "origin";

/// Reading the working tree against a fresh index of `HEAD`.
const KEEP_STATUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const MAX_TITLE_CHARS: usize = 200;
const MAX_BRANCH_BYTES: usize = 200;
const MAX_LISTED_PATHS: usize = 200;
const MAX_MESSAGE_CHARS: usize = 2048;
const MAX_EXCLUDE_BYTES: u64 = 1024 * 1024;
const MAX_BODY_CHARS: usize = 60_000;
/// Largest receipt block a pull request body carries; a larger receipt is
/// left to the run's record bundle, and the body says so.
const MAX_RECEIPT_BLOCK_CHARS: usize = 40_000;
/// The commit trailer that names the run.
pub const RUN_TRAILER: &str = "Axocoatl-Run";
/// The commit trailer that names the receipt's statement digest.
pub const RECEIPT_TRAILER: &str = "Axocoatl-Receipt";
const MAX_TABLE_ROWS: usize = 100;

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
    /// The branch the pull request merges into: the remote's default branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Paths that differ from `HEAD` in the working tree but that no Agent of
    /// the run is recorded changing; not committed (bounded list).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_committed: Vec<String>,
    /// What the person should know about this Keep.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// `sha256:<hex>` of the receipt's statement the commit names, when the
    /// run has a receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<String>,
}

impl KeepPrResponse {
    /// The run record's view of this Keep.
    pub fn keep_result(&self) -> KeepResult {
        KeepResult {
            branch: self.branch.clone(),
            commit: self.commit.clone(),
            pull_request_url: self.pull_request_url.clone(),
            error: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KeepPrError {
    #[error("keep as PR: not implemented: {0}")]
    NotImplemented(&'static str),
    /// The run or the repository is not in a state Keep may act on (409).
    #[error("keep as PR: {0}")]
    Refused(String),
    /// The request itself is malformed (422).
    #[error("keep as PR: {0}")]
    Invalid(String),
    /// Host git or gh failed.
    #[error("keep as PR: {0}")]
    Git(String),
    /// The branch and its commit exist; a later step failed. Keeping the run
    /// again continues from that branch.
    #[error("keep as PR: created branch {branch} at {commit}, then {message}")]
    AfterBranch {
        branch: String,
        commit: String,
        message: String,
    },
    /// The run record could not be read or written.
    #[error("keep as PR: run record: {0}")]
    Record(String),
}

impl KeepPrError {
    /// The run record's view of a failed Keep: the branch and commit when
    /// they were created.
    pub fn keep_result(&self, branch: &str) -> KeepResult {
        match self {
            KeepPrError::AfterBranch { branch, commit, .. } => KeepResult {
                branch: branch.clone(),
                commit: commit.clone(),
                pull_request_url: None,
                error: Some(self.to_string()),
            },
            _ => KeepResult {
                branch: branch.to_string(),
                commit: String::new(),
                pull_request_url: None,
                error: Some(self.to_string()),
            },
        }
    }
}

impl From<KeepPrError> for crate::DaemonError {
    fn from(error: KeepPrError) -> Self {
        match error {
            KeepPrError::NotImplemented(what) => crate::DaemonError::NotImplemented(what),
            KeepPrError::Refused(_) => crate::DaemonError::SessionConflict(error.to_string()),
            KeepPrError::Invalid(_) => crate::DaemonError::InvalidRequest(error.to_string()),
            KeepPrError::Git(_) | KeepPrError::AfterBranch { .. } | KeepPrError::Record(_) => {
                crate::DaemonError::Session(error.to_string())
            }
        }
    }
}

fn refused(message: impl Into<String>) -> KeepPrError {
    KeepPrError::Refused(message.into())
}

fn bounded(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

fn listed(paths: &[String]) -> String {
    let shown: Vec<&str> = paths.iter().take(20).map(String::as_str).collect();
    let more = paths.len().saturating_sub(shown.len());
    if more == 0 {
        shown.join(", ")
    } else {
        format!("{} and {more} more", shown.join(", "))
    }
}

// ---------------------------------------------------------------------------
// Request

/// Check the request's own fields (422 when wrong); nothing is read.
pub fn validate_request(request: &KeepPrRequest) -> Result<(), KeepPrError> {
    if !axocoatl_session::run_record::is_run_id(&request.run_id) {
        return Err(KeepPrError::Invalid(format!(
            "{:?} is not a run id (run-<uuid>)",
            bounded(&request.run_id, 80)
        )));
    }
    if let Some(branch) = &request.branch {
        check_branch_name(branch).map_err(KeepPrError::Invalid)?;
    }
    if let Some(remote) = &request.remote {
        check_remote_name(remote).map_err(KeepPrError::Invalid)?;
    }
    if let Some(title) = &request.title {
        check_title(title).map_err(KeepPrError::Invalid)?;
    }
    Ok(())
}

/// A branch name Keep creates: a valid Git branch name of printable ASCII
/// without leading `-`, `.` or `/` (`git check-ref-format` also runs).
pub fn check_branch_name(name: &str) -> Result<(), String> {
    let shown = bounded(name, 80);
    let reason = if name.is_empty() {
        Some("is empty")
    } else if name.len() > MAX_BRANCH_BYTES {
        Some("is longer than 200 bytes")
    } else if !name.bytes().all(|byte| byte.is_ascii_graphic()) {
        Some("may contain only printable ASCII without spaces")
    } else if name.starts_with(['-', '/', '.']) || name.ends_with(['/', '.']) {
        Some("may not start with '-', '/' or '.', or end with '/' or '.'")
    } else if name == "HEAD" || name == "@" {
        Some("names HEAD")
    } else if ["..", "//", "@{"].iter().any(|bad| name.contains(bad))
        || name
            .chars()
            .any(|ch| matches!(ch, '\\' | '~' | '^' | ':' | '?' | '*' | '['))
    {
        Some("contains a sequence Git refuses in a branch name")
    } else if name
        .split('/')
        .any(|part| part.starts_with('.') || part.ends_with(".lock"))
    {
        Some("has a component starting with '.' or ending with '.lock'")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(format!("the branch name {shown:?} {reason}")),
        None => Ok(()),
    }
}

fn check_remote_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|ch: char| ch.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "the remote name {:?} must be letters, digits, '.', '_' or '-' (at most 64)",
            bounded(name, 80)
        ))
    }
}

fn check_title(title: &str) -> Result<(), String> {
    if title.trim().is_empty() {
        return Err("the title is empty".into());
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(format!(
            "the title is longer than {MAX_TITLE_CHARS} characters"
        ));
    }
    if title.chars().any(char::is_control) {
        return Err("the title must be one line without control characters".into());
    }
    Ok(())
}

/// The branch Keep creates for `request`: the requested name, else
/// `axocoatl/<loadout>-<first 8 of the run uuid>`.
pub fn branch_name(request: &KeepPrRequest, loadout_id: &str) -> String {
    match &request.branch {
        Some(branch) => branch.clone(),
        None => {
            let uuid = request
                .run_id
                .strip_prefix("run-")
                .unwrap_or(&request.run_id);
            let prefix: String = uuid.chars().take(8).collect();
            format!("{KEEP_BRANCH_PREFIX}{loadout_id}-{prefix}")
        }
    }
}

/// The commit subject: the requested title, else `<loadout>: <first line of
/// the task>` within 72 characters.
pub fn commit_title(request: &KeepPrRequest, outcome: &RunOutcome) -> String {
    if let Some(title) = &request.title {
        return title.trim().to_string();
    }
    let line = outcome
        .task
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let clean: String = line
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let title = if clean.trim().is_empty() {
        format!("{} run {}", outcome.loadout.id, outcome.run_id)
    } else {
        format!("{}: {}", outcome.loadout.id, clean.trim())
    };
    match title.char_indices().nth(72) {
        Some((cut, _)) => format!("{}…", title[..cut].trim_end()),
        None => title,
    }
}

// ---------------------------------------------------------------------------
// Pull request body

fn verdict_words(verdict: RunVerdict) -> &'static str {
    match verdict {
        RunVerdict::Pass => "passed",
        RunVerdict::ChecksFailed => "required checks failed",
        RunVerdict::NeedsAttention => "needs attention",
        RunVerdict::Interrupted => "interrupted",
        RunVerdict::Error => "error",
    }
}

fn check_words(state: CheckState) -> &'static str {
    match state {
        CheckState::Passed => "passed",
        CheckState::Failed => "failed",
        CheckState::TimedOut => "timed out",
        CheckState::NotRun => "not run",
        CheckState::Unavailable => "unavailable",
    }
}

fn review_verdict_words(verdict: ReviewVerdictKind) -> &'static str {
    match verdict {
        ReviewVerdictKind::Approve => "approve",
        ReviewVerdictKind::Changes => "changes",
        ReviewVerdictKind::Unreadable => "unreadable",
    }
}

fn decision_words(decision: AdjudicationDecision) -> &'static str {
    match decision {
        AdjudicationDecision::Accept => "accept",
        AdjudicationDecision::Reject => "reject",
        AdjudicationDecision::Missing => "**missing**",
    }
}

/// How a finding's reproduction is shown; "fails on clean build" is never
/// shown as confirmed.
pub fn repro_words(classification: ReproClassification) -> &'static str {
    match classification {
        ReproClassification::Confirmed => "confirmed",
        ReproClassification::FailsOnCleanBuild => "fails on clean build",
        ReproClassification::Reproduced => "reproduced (no reference build)",
        ReproClassification::NotReproduced => "not reproduced",
        ReproClassification::ReproError => "reproduction error",
        ReproClassification::Missing => "no reproduction",
    }
}

fn source_words(source: FindingSource) -> &'static str {
    match source {
        FindingSource::Reviewer => "reviewer",
        FindingSource::Explorer => "explorer",
        FindingSource::AuditWorker => "audit worker",
        FindingSource::Integrator => "integrator",
    }
}

fn severity_words(severity: Severity) -> &'static str {
    match severity {
        Severity::Low => "low",
        Severity::Medium => "medium",
        Severity::High => "high",
        Severity::Critical => "critical",
    }
}

/// Text from a run (task, findings, reasons) as one Markdown-inert line:
/// whitespace collapsed, Markdown and HTML escaped, `@` mentions defused,
/// at most `max_chars` characters.
fn md(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    let mut space = false;
    let mut cut = false;
    for ch in text.trim().chars() {
        if count >= max_chars {
            cut = true;
            break;
        }
        let ch = if ch.is_whitespace() || ch.is_control() {
            ' '
        } else {
            ch
        };
        if ch == ' ' {
            if space {
                continue;
            }
            space = true;
        } else {
            space = false;
        }
        match ch {
            '\\' | '`' | '*' | '_' | '[' | ']' | '|' | '#' | '~' | '!' => {
                out.push('\\');
                out.push(ch);
            }
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '@' => out.push_str("&#64;"),
            _ => out.push(ch),
        }
        count += 1;
    }
    let mut out = out.trim().to_string();
    if cut {
        out.push_str(" …");
    }
    if out.is_empty() {
        "—".into()
    } else {
        out
    }
}

/// An identifier as inline code (no backticks or control characters).
fn code(text: &str) -> String {
    let clean: String = text
        .chars()
        .filter(|ch| !ch.is_control())
        .map(|ch| if ch == '`' { '\'' } else { ch })
        .take(200)
        .collect();
    format!("`{clean}`")
}

fn table_rows<T>(
    body: &mut String,
    items: &[T],
    header: &str,
    row: impl Fn(&T) -> String,
    noun: &str,
) {
    body.push_str(header);
    for item in items.iter().take(MAX_TABLE_ROWS) {
        body.push_str(&row(item));
        body.push('\n');
    }
    if items.len() > MAX_TABLE_ROWS {
        body.push_str(&format!(
            "\n…and {} more {noun} in the run record.\n",
            items.len() - MAX_TABLE_ROWS
        ));
    }
}

/// The Receipt section of a pull request body: what the receipt covers and
/// how to verify it, or why there is none.
fn receipt_section(outcome: &RunOutcome, receipt: Option<&OpenedReceipt>) -> String {
    let mut section = String::from("\n### Receipt\n\n");
    let Some(receipt) = receipt else {
        let why = outcome
            .warnings
            .iter()
            .find(|warning| warning.code == receipt_v1::RECEIPT_UNAVAILABLE)
            .map(|warning| format!(" ({})", md(&warning.message, 300)))
            .unwrap_or_default();
        section.push_str(&format!(
            "This run has no signed receipt{why}, so this pull request carries none.\n"
        ));
        return section;
    };
    let statement = &receipt.statement;
    let predicate = &statement.predicate;
    let checks = if predicate.checks.is_empty() {
        "none required".to_string()
    } else {
        predicate
            .checks
            .iter()
            .map(|check| {
                format!(
                    "{} {}",
                    md(&check.name, 60),
                    md(&check.state.replace('_', " "), 20)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let review = match &predicate.review {
        Some(review) => format!(
            "{}: {}",
            code(&format!("{}:{}", review.provider, review.model)),
            md(&review.verdict, 40)
        ),
        None => "none required".into(),
    };
    let writers: Vec<String> = predicate
        .agents
        .iter()
        .filter(|agent| agent.role == "writer")
        .map(|agent| {
            let program = match &agent.program_version {
                Some(version) => format!("{} {version} ", md(&agent.runtime, 40)),
                None if agent.runtime == "native" => String::new(),
                None => format!("{} ", md(&agent.runtime, 40)),
            };
            format!(
                "{program}{}",
                code(&format!("{}:{}", agent.provider, agent.model))
            )
        })
        .collect();
    let source = match predicate.files.source.as_str() {
        receipt_v1::source::JUDGED_CAPTURE => "as the required checks judged them",
        receipt_v1::source::AFTER_CAPTURE => "as the last writer left them",
        _ => "as read when the run finished",
    };
    section.push_str(&format!(
        "- Signed by key {} ({})\n- Verdict: **{}** (exit code {})\n- Checks: {checks}\n- Review: {review}\n- Writer: {}\n- Files covered: {} ({}), removed: {}{}\n- Statement: {}\n",
        code(&receipt_v1::short_key_id(&predicate.signer.key_id)),
        code(&predicate.signer.key_id),
        verdict_words(predicate.verdict),
        predicate.exit_code,
        if writers.is_empty() {
            "—".to_string()
        } else {
            writers.join(", ")
        },
        statement.subject.len(),
        source,
        predicate.files.removed.len(),
        if predicate.files.complete {
            ""
        } else {
            "; the receipt does not cover every path the run changed"
        },
        code(&receipt.payload_digest()),
    ));
    section.push_str(
        "\nThe receipt is the `axocoatl-receipt:v1` block at the end of this description. Save \
         the description, check out this branch, and verify it against the keys you trust \
         (lines `axocoatl receipt key` prints):\n\n```sh\naxocoatl receipt verify body.md \
         --keys trusted-keys --repo .\n```\n\nA receipt proves which key signed these facts and \
         that the covered files are the ones judged; it does not prove the change is correct.\n",
    );
    section
}

/// The pull request body for a run's Outcome: verdict, the Receipt section,
/// check results, review verdict and rounds, adjudications, findings,
/// not-covered list, warnings, usage, the record id with how to verify the
/// record bundle, and at the end the receipt block, which the cut to
/// [`MAX_BODY_CHARS`] never cuts.
pub fn pr_body(
    outcome: &RunOutcome,
    receipt: Option<&OpenedReceipt>,
) -> Result<String, KeepPrError> {
    if outcome.schema != RUN_OUTCOME_SCHEMA {
        return Err(refused(format!(
            "the run's Outcome has schema {:?}, not {RUN_OUTCOME_SCHEMA}",
            bounded(&outcome.schema, 60)
        )));
    }
    let mut body = String::new();
    let run = code(&outcome.run_id);
    body.push_str(&format!(
        "## Axocoatl run {}: {}\n\n",
        run,
        verdict_words(outcome.verdict)
    ));
    body.push_str(&format!(
        "- Verdict: **{}** (exit code {})\n- Loadout: {} ({}) {}\n- Session: {}\n- Record id: {}\n",
        verdict_words(outcome.verdict),
        outcome.exit_code,
        code(&format!(
            "{}@{}",
            outcome.loadout.id, outcome.loadout.version
        )),
        if outcome.loadout.builtin {
            "built-in"
        } else {
            "user loadout"
        },
        code(&format!(
            "sha256:{}",
            outcome.loadout.digest.chars().take(16).collect::<String>()
        )),
        code(&outcome.session_id),
        run,
    ));
    for reason in &outcome.attention {
        body.push_str(&format!("- Needs attention: {}\n", md(reason, 300)));
    }
    if let Some(error) = &outcome.error {
        body.push_str(&format!("- Error: {}\n", md(error, 300)));
    }

    body.push_str(&receipt_section(outcome, receipt));
    let block = receipt
        .map(|receipt| receipt_v1::pr_block(&receipt.envelope))
        .filter(|block| block.chars().count() <= MAX_RECEIPT_BLOCK_CHARS);
    if receipt.is_some() && block.is_none() {
        body.push_str(&format!(
            "\nThe receipt is larger than a pull request description holds, so it is not \
             here: verify it from the run's record bundle (`axocoatl run … --receipt FILE` \
             writes it; the daemon serves it at `GET /api/runs/{}/receipt`).\n",
            outcome.run_id.replace('`', "'")
        ));
    }

    body.push_str("\n### Record\n\n");
    body.push_str(&format!(
        "Everything this run did is in its record, {run}. Export the record bundle \
         (`axocoatl run … --record FILE` writes it; the daemon serves it at \
         `GET /api/runs/{}/record`) and verify it:\n\n```sh\naxocoatl record verify {}.axorecord.jsonl\n```\n",
        outcome.run_id.replace('`', "'"),
        outcome.run_id.replace('`', "'"),
    ));

    body.push_str("\n### Task\n\n");
    let lines: Vec<&str> = outcome
        .task
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        body.push_str("> —\n");
    }
    for line in lines.iter().take(12) {
        body.push_str(&format!("> {}\n", md(line, 300)));
    }
    if lines.len() > 12 {
        body.push_str("> …\n");
    }

    body.push_str("\n### Checks\n\n");
    if outcome.checks.is_empty() {
        body.push_str("This run had no required checks.\n");
    } else {
        table_rows(
            &mut body,
            &outcome.checks,
            "| Check | Result | Exit code | Report | Command |\n|---|---|---|---|---|\n",
            |check| {
                let report = match (&check.report, &check.reason) {
                    (Some(report), _) => format!(
                        "{} passed, {} failed, {} skipped, {} errors ({})",
                        report.passed,
                        report.failed,
                        report.skipped,
                        report.errors,
                        md(&report.format, 40)
                    ),
                    (None, Some(reason)) => md(reason, 160),
                    (None, None) => "—".into(),
                };
                format!(
                    "| {} | {} | {} | {} | {} |",
                    md(&check.name, 80),
                    check_words(check.state),
                    check
                        .exit_code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "—".into()),
                    report,
                    md(&check.argv.join(" "), 160)
                )
            },
            "checks",
        );
    }

    body.push_str("\n### Review\n\n");
    match &outcome.review {
        None => body.push_str("This run had no required review.\n"),
        Some(review) => {
            let runtime = if review.reviewer.runtime == "native" {
                String::new()
            } else {
                format!(" ({})", md(&review.reviewer.runtime, 40))
            };
            body.push_str(&format!(
                "Reviewer {}{runtime}: **{}** ({}), {} of {} rounds.",
                code(&format!(
                    "{}:{}",
                    review.reviewer.provider, review.reviewer.model
                )),
                if review.passed {
                    "approved"
                } else {
                    "not approved"
                },
                md(&review.state, 40),
                review.rounds.len(),
                review.max_rounds
            ));
            if !review.reason.trim().is_empty() {
                body.push_str(&format!(" {}", md(&review.reason, 300)));
            }
            body.push_str("\n\n");
            if !review.rounds.is_empty() {
                table_rows(
                    &mut body,
                    &review.rounds,
                    "| Round | Verdict | Findings | Sent back to the writer |\n|---|---|---|---|\n",
                    |round| {
                        let ids: Vec<&str> = round
                            .findings
                            .iter()
                            .map(|finding| finding.id.as_str())
                            .collect();
                        format!(
                            "| {} | {} | {} | {} |",
                            round.round,
                            review_verdict_words(round.verdict),
                            if ids.is_empty() {
                                "none".to_string()
                            } else {
                                md(&ids.join(", "), 200)
                            },
                            if round.continued { "yes" } else { "no" }
                        )
                    },
                    "rounds",
                );
            }
        }
    }

    body.push_str("\n### Adjudications\n\n");
    if outcome.adjudications.is_empty() {
        body.push_str(
            "No review finding was sent back to the writer, so there was nothing to adjudicate.\n",
        );
    } else {
        table_rows(
            &mut body,
            &outcome.adjudications,
            "| Round | Finding | Decision | Reason |\n|---|---|---|---|\n",
            |item| {
                format!(
                    "| {} | {}: {} | {} | {} |",
                    item.round,
                    md(&item.finding_id, 20),
                    md(&item.finding, 160),
                    decision_words(item.decision),
                    md(&item.reason, 300)
                )
            },
            "adjudications",
        );
    }

    body.push_str("\n### Findings\n\n");
    if outcome.findings.is_empty() {
        body.push_str("No findings.\n");
    } else {
        table_rows(
            &mut body,
            &outcome.findings,
            "| Id | Title | Source | Area | Severity | Reproduction |\n|---|---|---|---|---|---|\n",
            |finding| {
                format!(
                    "| {} | {} | {} | {} | {} | {} |",
                    md(&finding.id, 20),
                    md(&finding.title, 160),
                    source_words(finding.source),
                    finding
                        .area
                        .as_deref()
                        .map(|area| md(area, 60))
                        .unwrap_or_else(|| "—".into()),
                    finding.severity.map(severity_words).unwrap_or("—"),
                    finding
                        .repro
                        .as_ref()
                        .map(|repro| format!(
                            "{}: {}",
                            repro_words(repro.classification),
                            md(&repro.path, 120)
                        ))
                        .unwrap_or_else(|| "—".into())
                )
            },
            "findings",
        );
    }

    body.push_str("\n### Not covered\n\n");
    if outcome.not_covered.is_empty() {
        body.push_str(
            "Nothing: every area, helper, slot and check of the run reported a result.\n",
        );
    } else {
        // The Outcome's one rendering of why (`NotCovered::reason`), as the
        // run summary, its progress lines, JUnit and the Run outcome panel
        // show it.
        for entry in outcome.not_covered.iter().take(MAX_TABLE_ROWS) {
            body.push_str(&format!(
                "- **{}**: {}\n",
                md(&entry.area, 80),
                md(&entry.reason(), 300)
            ));
        }
        if outcome.not_covered.len() > MAX_TABLE_ROWS {
            body.push_str(&format!(
                "- …and {} more in the run record.\n",
                outcome.not_covered.len() - MAX_TABLE_ROWS
            ));
        }
    }

    body.push_str("\n### Warnings\n\n");
    if outcome.warnings.is_empty() {
        body.push_str("None.\n");
    } else {
        for warning in outcome.warnings.iter().take(MAX_TABLE_ROWS) {
            body.push_str(&format!(
                "- {}: {}\n",
                code(&warning.code),
                md(&warning.message, 400)
            ));
        }
    }

    body.push_str("\n### Usage\n\n");
    let usage = &outcome.usage;
    body.push_str(&format!(
        "{} input and {} output tokens, {}{}. Provider retries: {}.\n",
        usage.input_tokens,
        usage.output_tokens,
        usage.cost_text(),
        if usage.complete {
            ""
        } else {
            " (known subtotal: some calls reported no usage)"
        },
        usage.retries
    ));
    let network = &outcome.network;
    body.push_str(&format!(
        "Network record: {} events, {} allowed and {} refused connections, {} route requests.\n",
        network.events,
        network.allowed_connections,
        network.refused_connections,
        network.route_requests
    ));
    body.push_str("\n---\nKept with Axocoatl Keep as PR. Findings, reasons and the task are quoted from the run.\n");

    // The receipt block is never cut: the rest of the body makes room.
    let room =
        MAX_BODY_CHARS.saturating_sub(block.as_ref().map_or(0, |block| block.chars().count() + 2));
    if body.chars().count() > room {
        let cut = body
            .char_indices()
            .nth(room)
            .map(|(index, _)| index)
            .unwrap_or(body.len());
        body.truncate(cut);
        body.push_str(&format!(
            "\n\n…cut to fit a pull request body; the run record {run} has everything.\n"
        ));
    }
    if let Some(block) = block {
        body.push('\n');
        body.push_str(&block);
        body.push('\n');
    }
    Ok(body)
}

/// The commit message's trailers: the run, and the receipt's statement
/// digest when the run has one.
pub fn commit_trailers(run_id: &str, receipt: Option<&OpenedReceipt>) -> String {
    let mut trailers = format!("{RUN_TRAILER}: {run_id}");
    if let Some(receipt) = receipt {
        trailers.push_str(&format!(
            "\n{RECEIPT_TRAILER}: {}",
            receipt.payload_digest()
        ));
    }
    trailers
}

/// Each path Keep is about to commit, checked against the run's receipt:
/// its content in the working tree must be the content the receipt covers
/// (a removed path must be absent). Refused, naming each path, otherwise.
fn check_against_receipt(
    receipt: &OpenedReceipt,
    root: &Path,
    paths: &[String],
) -> Result<(), KeepPrError> {
    let statement = &receipt.statement;
    let judged = match statement.predicate.files.source.as_str() {
        receipt_v1::source::JUDGED_CAPTURE => "changed after the run's checks judged it",
        receipt_v1::source::AFTER_CAPTURE => "changed after the run's last writer left it",
        _ => "changed after the run finished",
    };
    let mut changed = Vec::new();
    let mut uncovered = Vec::new();
    for path in paths {
        if let Some(mismatch) = receipt_v1::check_path(statement, root, path) {
            if statement
                .subject
                .iter()
                .any(|subject| subject.name == *path)
                || statement.predicate.files.removed.contains(path)
            {
                changed.push(format!("{path}: {}", mismatch.problem));
            } else {
                uncovered.push(path.clone());
            }
        }
    }
    if !changed.is_empty() {
        return Err(refused(format!(
            "{} {judged}; Keep commits only what the receipt covers: {}",
            if changed.len() == 1 {
                "a path"
            } else {
                "paths"
            },
            listed(&changed)
        )));
    }
    if !uncovered.is_empty() {
        return Err(refused(format!(
            "the run's receipt does not cover {}; Keep commits only what the receipt covers",
            listed(&uncovered)
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Run record

/// What Keep reads and writes of a run's record (core's `RunRecordStore`;
/// tests use an in-memory record).
pub trait KeepRunRecord: Send + Sync {
    fn manifest(&self, run_id: &str) -> Result<RunManifest, KeepPrError>;
    fn outcome(&self, run_id: &str) -> Result<Option<RunOutcome>, KeepPrError>;
    /// Every Keep result recorded for the run, oldest first.
    fn keep_results(&self, run_id: &str) -> Result<Vec<KeepResult>, KeepPrError>;
    /// Append one Keep result as `RunEvent::Phase("keep", …)`.
    fn record_keep(&self, run_id: &str, result: &KeepResult) -> Result<(), KeepPrError>;
    /// The run's signed receipt as written, when it has one.
    fn receipt(&self, _run_id: &str) -> Result<Option<Vec<u8>>, KeepPrError> {
        Ok(None)
    }
}

/// Map a run record error: a missing run is a refusal, an unfinished hook is
/// not implemented.
pub fn record_error(error: RunRecordError) -> KeepPrError {
    match error {
        RunRecordError::NotImplemented(what) => KeepPrError::NotImplemented(what),
        RunRecordError::NotFound(run) => refused(format!("there is no run {}", bounded(&run, 80))),
        other => KeepPrError::Record(other.to_string()),
    }
}

/// The run event that records a Keep result.
pub fn keep_event(result: &KeepResult, at_ms: u64) -> RunEvent {
    RunEvent::Phase {
        at_ms,
        phase: KEEP_PHASE.into(),
        detail: serde_json::to_string(result).unwrap_or_default(),
    }
}

/// The Keep result an event records, if it records one.
pub fn keep_result_of(event: &RunEvent) -> Option<KeepResult> {
    match event {
        RunEvent::Phase { phase, detail, .. } if phase == KEEP_PHASE => {
            serde_json::from_str(detail).ok()
        }
        _ => None,
    }
}

/// The latest recorded Keep that created a branch and commit: what a later
/// Keep of the same run may continue (push, pull request).
pub fn last_kept_branch(results: &[KeepResult]) -> Option<&KeepResult> {
    results
        .iter()
        .rev()
        .find(|result| !result.branch.is_empty() && !result.commit.is_empty())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

impl KeepRunRecord for RunRecordStore {
    fn manifest(&self, run_id: &str) -> Result<RunManifest, KeepPrError> {
        RunRecordStore::manifest(self, run_id).map_err(record_error)
    }

    fn outcome(&self, run_id: &str) -> Result<Option<RunOutcome>, KeepPrError> {
        RunRecordStore::outcome(self, run_id).map_err(record_error)
    }

    fn keep_results(&self, run_id: &str) -> Result<Vec<KeepResult>, KeepPrError> {
        let mut results = Vec::new();
        let mut after = None;
        let pages = axocoatl_session::run_record::MAX_RUN_EVENTS / 1000 + 1;
        for _ in 0..pages {
            let page = self.events(run_id, after, 1000).map_err(record_error)?;
            let Some((last, _)) = page.last() else {
                break;
            };
            after = Some(*last);
            results.extend(page.iter().filter_map(|(_, event)| keep_result_of(event)));
        }
        Ok(results)
    }

    fn record_keep(&self, run_id: &str, result: &KeepResult) -> Result<(), KeepPrError> {
        self.append_after_end(run_id, &keep_event(result, now_ms()))
            .map(|_| ())
            .map_err(record_error)
    }

    fn receipt(&self, run_id: &str) -> Result<Option<Vec<u8>>, KeepPrError> {
        RunRecordStore::receipt(self, run_id).map_err(record_error)
    }
}

/// The run's receipt for Keep: `None` without one; a receipt that cannot be
/// read, or that is not this run's, is refused rather than ignored.
pub fn load_receipt(
    record: &dyn KeepRunRecord,
    run_id: &str,
) -> Result<Option<OpenedReceipt>, KeepPrError> {
    let Some(bytes) = record.receipt(run_id)? else {
        return Ok(None);
    };
    let opened = receipt_v1::open_receipt(&bytes)
        .map_err(|error| refused(format!("the run's receipt cannot be read: {error}")))?;
    if opened.statement.predicate.run.run_id != run_id {
        return Err(refused(format!(
            "the receipt stored for run {run_id} is of run {}",
            opened.statement.predicate.run.run_id
        )));
    }
    Ok(Some(opened))
}

/// Load a run for Keep: it must be a run of `session_id`, finished, and its
/// verdict must be `pass`.
pub fn load_run(
    record: &dyn KeepRunRecord,
    session_id: &str,
    run_id: &str,
) -> Result<(RunManifest, RunOutcome), KeepPrError> {
    let manifest = record.manifest(run_id)?;
    if manifest.run_id != run_id || manifest.session_id != session_id {
        return Err(refused(format!(
            "run {run_id} is not a run of Session {session_id}"
        )));
    }
    let outcome = record
        .outcome(run_id)?
        .ok_or_else(|| refused(format!("run {run_id} has not finished")))?;
    if outcome.run_id != run_id || outcome.session_id != session_id {
        return Err(refused(format!(
            "the Outcome of run {run_id} does not name Session {session_id}"
        )));
    }
    require_pass(&outcome)?;
    Ok((manifest, outcome))
}

fn require_pass(outcome: &RunOutcome) -> Result<(), KeepPrError> {
    if outcome.verdict == RunVerdict::Pass && outcome.exit_code == 0 {
        return Ok(());
    }
    let reasons = if outcome.attention.is_empty() {
        outcome
            .error
            .clone()
            .unwrap_or_else(|| "no reason recorded".into())
    } else {
        outcome.attention.join("; ")
    };
    Err(refused(format!(
        "run {} did not pass (verdict {}, exit code {}): {}. Keep as PR keeps only a passing run",
        outcome.run_id,
        verdict_words(outcome.verdict),
        outcome.exit_code,
        bounded(&reasons, 600)
    )))
}

// ---------------------------------------------------------------------------
// Attribution: `crate::run_attribution`, shared with receipts.

use crate::run_attribution::is_git_internal;
pub use crate::run_attribution::RunAttribution;
#[cfg(test)]
pub(crate) use crate::run_attribution::{
    attribute_activation, ActivationChanges, CaptureFacts, CapturePhase,
};

/// A repository-relative path Keep can name to Git.
fn is_plain_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Whether `path` had uncommitted changes before the run: listed exactly,
/// below a listed directory, or as either side of a listed rename.
fn was_dirty(path: &str, dirty: &[String]) -> bool {
    dirty.iter().any(|entry| {
        let entry = entry.trim();
        entry
            .split(" -> ")
            .map(|side| side.trim().trim_matches('"'))
            .filter(|side| !side.is_empty())
            .any(|side| {
                let directory = side.trim_end_matches('/');
                path == directory || path.starts_with(&format!("{directory}/"))
            })
    })
}

// ---------------------------------------------------------------------------
// Keep

/// Everything Keep needs about one run, loaded and checked to belong to the
/// Session ([`load_run`]).
pub struct KeepJob<'a> {
    /// The Session's folder; it must be the top level of its repository.
    pub session_root: &'a Path,
    /// Axocoatl's data root: the protected Git directory lives below it.
    pub control_root: &'a SecureDir,
    pub request: &'a KeepPrRequest,
    pub manifest: &'a RunManifest,
    pub outcome: &'a RunOutcome,
    pub attribution: &'a RunAttribution,
    /// What the Session's turns outside the run changed (a turn sent after
    /// it finished, say). A run path one of them changed too is refused.
    pub outside: &'a RunAttribution,
    /// The latest recorded Keep of this run that created its branch.
    pub previous: Option<&'a KeepResult>,
    /// The run's signed receipt, when it has one ([`load_receipt`]).
    pub receipt: Option<&'a OpenedReceipt>,
    pub tools: &'a HostTools,
}

/// An error's own words, without the "keep as PR" prefix.
fn message_of(error: KeepPrError) -> String {
    match error {
        KeepPrError::Refused(message)
        | KeepPrError::Invalid(message)
        | KeepPrError::Git(message)
        | KeepPrError::Record(message) => message,
        other => other.to_string(),
    }
}

fn first_line(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn stderr_of(output: &std::process::Output) -> String {
    bounded(
        &git_host::redact_url_credentials(String::from_utf8_lossy(&output.stderr).trim()),
        MAX_MESSAGE_CHARS,
    )
}

fn git_failure(what: &str, output: &std::process::Output) -> KeepPrError {
    let stderr = stderr_of(output);
    KeepPrError::Git(if stderr.is_empty() {
        format!("{what} failed")
    } else {
        format!("{what} failed: {stderr}")
    })
}

async fn local(
    command: tokio::process::Command,
    what: &str,
) -> Result<std::process::Output, KeepPrError> {
    run_bounded(command, KEEP_LOCAL_TIMEOUT, what)
        .await
        .map_err(KeepPrError::Git)
}

/// What Keep reads of the person's repository before changing anything.
struct RepoFacts {
    root: PathBuf,
    head: String,
    git_dir: PathBuf,
    common_dir: PathBuf,
    objects: PathBuf,
    shallow: Option<PathBuf>,
    object_format: String,
    current_branch: Option<String>,
    settings: Vec<(String, String)>,
    exclude: Option<Vec<u8>>,
    person: Option<Vec<String>>,
}

impl RepoFacts {
    async fn read(tools: &HostTools, session_root: &Path) -> Result<Self, KeepPrError> {
        let root = std::fs::canonicalize(session_root).map_err(|error| {
            refused(format!(
                "the Session's folder {} cannot be opened: {error}",
                session_root.display()
            ))
        })?;
        let top = local(
            repository_git(tools, &root, &["rev-parse", "--show-toplevel"]),
            "git rev-parse",
        )
        .await?;
        if !top.status.success() {
            return Err(refused(format!(
                "the Session's folder {} is not a Git repository",
                root.display()
            )));
        }
        let top_path = PathBuf::from(first_line(&top));
        let top = std::fs::canonicalize(&top_path).unwrap_or(top_path);
        if top != root {
            return Err(refused(format!(
                "Keep as PR needs the Session's folder {} to be the top of its Git repository, which is {}",
                root.display(),
                top.display()
            )));
        }
        let head = local(
            repository_git(
                tools,
                &root,
                &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
            ),
            "git rev-parse",
        )
        .await?;
        let head = first_line(&head);
        if head.is_empty() {
            return Err(refused(
                "the repository has no commit to branch from; make a first commit",
            ));
        }
        let dirs = local(
            repository_git(
                tools,
                &root,
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-dir",
                    "--git-common-dir",
                    "--git-path",
                    "objects",
                    "--git-path",
                    "shallow",
                ],
            ),
            "git rev-parse",
        )
        .await?;
        if !dirs.status.success() {
            return Err(git_failure(
                "reading the repository's Git directories",
                &dirs,
            ));
        }
        let dirs: Vec<PathBuf> = String::from_utf8_lossy(&dirs.stdout)
            .lines()
            .map(|line| PathBuf::from(line.trim()))
            .collect();
        let [git_dir, common_dir, objects, shallow] = dirs.as_slice() else {
            return Err(KeepPrError::Git(
                "git did not name the repository's directories".into(),
            ));
        };
        if !objects.is_absolute() || !git_dir.is_absolute() || !common_dir.is_absolute() {
            return Err(KeepPrError::Git(
                "git named a relative repository directory".into(),
            ));
        }
        let format = local(
            repository_git(tools, &root, &["rev-parse", "--show-object-format"]),
            "git rev-parse",
        )
        .await?;
        let object_format = match first_line(&format).as_str() {
            "sha256" => "sha256".to_string(),
            "sha1" => "sha1".to_string(),
            _ if head.len() == 64 => "sha256".to_string(),
            _ => "sha1".to_string(),
        };
        let current = local(
            repository_git(
                tools,
                &root,
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
            ),
            "git symbolic-ref",
        )
        .await?;
        let current_branch = current
            .status
            .success()
            .then(|| first_line(&current))
            .filter(|branch| !branch.is_empty());
        let mut settings = Vec::new();
        for key in ["core.ignorecase", "core.precomposeunicode"] {
            let value = local(
                repository_git(tools, &root, &["config", "--type=bool", "--get", key]),
                "git config",
            )
            .await?;
            if value.status.success() {
                settings.push((key.to_string(), first_line(&value)));
            }
        }
        let listing = local(
            repository_git(
                tools,
                &root,
                &["config", "--list", "--show-scope", "--show-origin", "-z"],
            ),
            "git config",
        )
        .await?;
        let person = listing
            .status
            .success()
            .then(|| git_host::person_settings(&listing.stdout, &root))
            .flatten();
        let exclude_path = common_dir.join("info/exclude");
        let exclude = match std::fs::symlink_metadata(&exclude_path) {
            Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_EXCLUDE_BYTES => {
                std::fs::read(&exclude_path).ok()
            }
            _ => None,
        };
        Ok(Self {
            root,
            head,
            git_dir: git_dir.clone(),
            common_dir: common_dir.clone(),
            objects: objects.clone(),
            shallow: shallow.is_file().then(|| shallow.clone()),
            object_format,
            current_branch,
            settings,
            exclude,
            person,
        })
    }

    /// Git's settings files that changed after `since_ms`. Remote URLs and
    /// identity are read from them, and an Agent could write them.
    fn settings_changed_since(&self, since_ms: u64) -> Result<Vec<String>, KeepPrError> {
        let mut candidates = vec![
            self.common_dir.join("config"),
            self.common_dir.join("config.worktree"),
            self.git_dir.join("config"),
            self.git_dir.join("config.worktree"),
        ];
        let dot_git = self.root.join(".git");
        if std::fs::symlink_metadata(&dot_git).is_ok_and(|metadata| !metadata.is_dir()) {
            candidates.push(dot_git);
        }
        candidates.sort();
        candidates.dedup();
        let mut changed = Vec::new();
        for path in candidates {
            if git_host::changed_since(&path, since_ms).map_err(|error| {
                KeepPrError::Git(format!("could not read {}: {error}", path.display()))
            })? {
                changed.push(path.display().to_string());
            }
        }
        Ok(changed)
    }

    async fn local_branch(
        &self,
        tools: &HostTools,
        branch: &str,
    ) -> Result<Option<String>, KeepPrError> {
        let reference = format!("refs/heads/{branch}");
        let output = local(
            repository_git(
                tools,
                &self.root,
                &["rev-parse", "--verify", "--quiet", &reference],
            ),
            "git rev-parse",
        )
        .await?;
        Ok(output
            .status
            .success()
            .then(|| first_line(&output))
            .filter(|oid| !oid.is_empty()))
    }

    async fn config_value(
        &self,
        tools: &HostTools,
        key: &str,
    ) -> Result<Option<String>, KeepPrError> {
        let output = local(
            repository_git(tools, &self.root, &["config", "--get", key]),
            "git config",
        )
        .await?;
        match output.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&output.stdout)
                    .trim_end_matches(['\n', '\r'])
                    .to_string(),
            )),
            Some(1) => Ok(None),
            Some(2) => Err(refused(format!(
                "the repository sets {key} more than once; Keep as PR needs exactly one"
            ))),
            _ => Err(git_failure("git config", &output)),
        }
    }

    fn protected(&self, control_root: &SecureDir) -> Result<ProtectedGit, KeepPrError> {
        ProtectedGit::create(
            control_root,
            ProtectedGitSpec {
                work_tree: &self.root,
                objects: &self.objects,
                shallow: self.shallow.as_deref(),
                head: &self.head,
                object_format: &self.object_format,
                settings: &self.settings,
                exclude: self.exclude.clone(),
                person: self.person.clone(),
            },
        )
        .map_err(KeepPrError::Git)
    }
}

/// Where the pull request goes.
struct RemoteTarget {
    name: String,
    /// `HOST/OWNER/REPO` for `gh --repo`.
    slug: String,
    default_branch: String,
    /// The branch's object id on the remote, when it exists there.
    existing: Option<String>,
}

impl RemoteTarget {
    async fn resolve(
        tools: &HostTools,
        repo: &RepoFacts,
        protected: &ProtectedGit,
        name: &str,
        branch: &str,
    ) -> Result<Self, KeepPrError> {
        let url = match repo
            .config_value(tools, &format!("remote.{name}.pushurl"))
            .await?
        {
            Some(url) => url,
            None => repo
                .config_value(tools, &format!("remote.{name}.url"))
                .await?
                .ok_or_else(|| refused(format!("the repository has no remote named {name}")))?,
        };
        let shown = git_host::redact_url_credentials(&url);
        if url.is_empty() || url.chars().any(char::is_control) {
            return Err(refused(format!("the URL of remote {name} is not usable")));
        }
        let slug = git_host::hosted_repository_slug(&url).ok_or_else(|| {
            refused(format!(
                "gh cannot open a pull request for remote {name} ({shown}): its URL does not name a hosted repository (host/owner/repository)"
            ))
        })?;
        // The remote lives in the protected directory's own settings, so the
        // person's global URL rewrites apply and the repository's do not.
        let key = format!("remote.{name}.url");
        let named = local(
            protected.command(tools, &["config", &key, &url]),
            "git config",
        )
        .await?;
        if !named.status.success() {
            return Err(git_failure("naming the remote", &named));
        }
        // Every URL the push would go to, after the person's own rewrites.
        let effective = local(
            protected.command(tools, &["remote", "get-url", "--push", "--all", name]),
            "git remote get-url",
        )
        .await?;
        if !effective.status.success() {
            return Err(git_failure("resolving the push URL", &effective));
        }
        let urls: Vec<String> = String::from_utf8_lossy(&effective.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        if urls.is_empty() {
            return Err(refused(format!("remote {name} has no push URL")));
        }
        for url in &urls {
            git_host::check_push_url(url, &repo.root).map_err(KeepPrError::Refused)?;
        }
        let reference = format!("refs/heads/{branch}");
        let listed = run_bounded(
            protected.command(tools, &["ls-remote", "--symref", name, "HEAD", &reference]),
            KEEP_NETWORK_TIMEOUT,
            "git ls-remote",
        )
        .await
        .map_err(KeepPrError::Git)?;
        if !listed.status.success() {
            return Err(git_failure(&format!("reading remote {name}"), &listed));
        }
        let mut default_branch = None;
        let mut existing = None;
        for line in String::from_utf8_lossy(&listed.stdout).lines() {
            if let Some(symref) = line.strip_prefix("ref: ") {
                if let Some((target, "HEAD")) = symref.split_once('\t') {
                    default_branch = target.strip_prefix("refs/heads/").map(str::to_string);
                }
            } else if let Some((oid, name)) = line.split_once('\t') {
                if name == reference {
                    existing = Some(oid.trim().to_string());
                }
            }
        }
        let default_branch = default_branch.ok_or_else(|| {
            refused(format!(
                "the default branch of remote {name} cannot be read (it names no HEAD branch)"
            ))
        })?;
        Ok(Self {
            name: name.to_string(),
            slug,
            default_branch,
            existing,
        })
    }
}

/// A commit Keep built.
struct Built {
    commit: String,
    paths: Vec<String>,
    not_committed: Vec<String>,
}

/// Parse `git status --porcelain=v1 -z`: every path it names.
fn porcelain_paths(stdout: &[u8]) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    let mut entries = stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty());
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let status = &entry[..2];
        // Git lists an untracked repository inside the checkout (a
        // submodule not yet committed) as its directory, `dir/`.
        let path = String::from_utf8_lossy(&entry[3..]).into_owned();
        paths.insert(match path.strip_suffix('/') {
            Some(directory) => directory.to_owned(),
            None => path,
        });
        if status.contains(&b'R') || status.contains(&b'C') {
            if let Some(original) = entries.next() {
                paths.insert(String::from_utf8_lossy(original).into_owned());
            }
        }
    }
    paths
}

fn nul_paths(stdout: &[u8]) -> Vec<String> {
    stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
        .collect()
}

async fn identity(
    tools: &HostTools,
    repo: &RepoFacts,
) -> Result<Vec<(String, String)>, KeepPrError> {
    let mut env = Vec::new();
    for (key, variables) in [
        ("user.name", ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"]),
        ("user.email", ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"]),
    ] {
        let output = local(
            repository_git(tools, &repo.root, &["config", "--get", key]),
            "git config",
        )
        .await?;
        let value = first_line(&output);
        if output.status.success() && !value.is_empty() && !value.chars().any(char::is_control) {
            for variable in variables {
                env.push((variable.to_string(), value.clone()));
            }
        }
    }
    Ok(env)
}

async fn build_commit(
    job: &KeepJob<'_>,
    repo: &RepoFacts,
    protected: &ProtectedGit,
    branch: &str,
    title: &str,
) -> Result<Built, KeepPrError> {
    let tools = job.tools;
    let read = local(
        protected.command(tools, &["read-tree", &repo.head]),
        "git read-tree",
    )
    .await?;
    if !read.status.success() {
        return Err(git_failure("reading HEAD into a temporary index", &read));
    }
    // A fresh index has no stat cache, so this hashes the working tree.
    let status = run_bounded(
        protected.command(
            tools,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                // A submodule whose commit moved, never what is inside it:
                // Git does not run status there.
                "--ignore-submodules=dirty",
                "--no-renames",
            ],
        ),
        KEEP_STATUS_TIMEOUT,
        "git status",
    )
    .await
    .map_err(KeepPrError::Git)?;
    if !status.status.success() {
        return Err(git_failure("reading the working tree", &status));
    }
    let changed = porcelain_paths(&status.stdout);
    let attributed = &job.attribution.paths;
    let run_paths: Vec<String> = changed
        .iter()
        .filter(|path| attributed.contains(*path))
        .cloned()
        .collect();
    let dirty: Vec<String> = run_paths
        .iter()
        .filter(|path| was_dirty(path, &job.manifest.dirty_paths))
        .cloned()
        .collect();
    if !dirty.is_empty() {
        return Err(refused(format!(
            "{} had uncommitted changes before the run started, so the run's work on {} cannot be kept apart from them: {}",
            if dirty.len() == 1 { "a path the run changed" } else { "paths the run changed" },
            if dirty.len() == 1 { "it" } else { "them" },
            listed(&dirty)
        )));
    }
    // A submodule the run moved is committed as the commit it holds (a
    // gitlink), and only as the run's receipt covers it: its directory's
    // times say nothing about which commit it holds.
    let submodules: Vec<String> = run_paths
        .iter()
        .filter(|path| {
            std::fs::symlink_metadata(repo.root.join(path)).is_ok_and(|meta| meta.is_dir())
        })
        .cloned()
        .collect();
    if !submodules.is_empty() && job.receipt.is_none() {
        return Err(refused(format!(
            "the run moved the {} {} to another commit, and without a receipt that covers {} \
             Keep as PR cannot tell that {} the commit the run left; commit {} yourself",
            if submodules.len() == 1 { "submodule" } else { "submodules" },
            listed(&submodules),
            if submodules.len() == 1 { "it" } else { "them" },
            if submodules.len() == 1 { "it still holds" } else { "they still hold" },
            if submodules.len() == 1 { "it" } else { "them" },
        )));
    }
    // With a receipt, each path's content must be the one it covers.
    if let Some(receipt) = job.receipt {
        check_against_receipt(receipt, &repo.root, &run_paths)?;
    }
    // The run's checks and review judged the files as the run left them. A
    // run path whose file changed after the run ended (by its status-change
    // time, which no process can set back) holds work they never saw, such
    // as an edit made by hand afterwards: it is refused, never committed.
    let mut later = Vec::new();
    for path in run_paths.iter().filter(|path| !submodules.contains(path)) {
        let after_end = git_host::changed_since(&repo.root.join(path), job.outcome.finished_at_ms)
            .map_err(|error| KeepPrError::Git(format!("could not read {path}: {error}")))?;
        if after_end {
            later.push(path.clone());
        }
    }
    if !later.is_empty() {
        return Err(refused(format!(
            "{} changed after the run ended, so a commit would hold work its checks and review never saw: {}; commit {} yourself, or start a new run",
            if later.len() == 1 { "a path the run changed" } else { "paths the run changed" },
            listed(&later),
            if later.len() == 1 { "it" } else { "them" },
        )));
    }

    let not_committed: Vec<String> = changed
        .iter()
        .filter(|path| !attributed.contains(*path) && !was_dirty(path, &job.manifest.dirty_paths))
        .take(MAX_LISTED_PATHS)
        .cloned()
        .collect();
    if run_paths.is_empty() {
        return Err(refused(if not_committed.is_empty() {
            "there is nothing to keep: no path the run's Agents changed differs from HEAD"
                .to_string()
        } else {
            format!(
                "there is nothing to keep: no path the run's Agents changed differs from HEAD (changed, but not by the run's Agents: {})",
                listed(&not_committed)
            )
        }));
    }
    let mut input = Vec::new();
    for path in &run_paths {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    let updated = run_bounded_with_input(
        protected.command(
            tools,
            &["update-index", "--add", "--remove", "-z", "--stdin"],
        ),
        input,
        KEEP_LOCAL_TIMEOUT,
        "git update-index",
    )
    .await
    .map_err(KeepPrError::Git)?;
    if !updated.status.success() {
        return Err(git_failure(
            "staging the run's paths in a temporary index",
            &updated,
        ));
    }
    let tree = local(protected.command(tools, &["write-tree"]), "git write-tree").await?;
    if !tree.status.success() {
        return Err(git_failure("writing the tree", &tree));
    }
    let tree = first_line(&tree);
    let diff = local(
        protected.command(
            tools,
            &[
                "diff-tree",
                "-r",
                "-z",
                "--no-renames",
                "--name-only",
                "--no-commit-id",
                &repo.head,
                &tree,
            ],
        ),
        "git diff-tree",
    )
    .await?;
    if !diff.status.success() {
        return Err(git_failure("listing the committed paths", &diff));
    }
    let paths = nul_paths(&diff.stdout);
    if paths.is_empty() {
        return Err(refused(
            "there is nothing to keep: the run's paths hold no change against HEAD",
        ));
    }
    let message = format!("Axocoatl run {}", job.outcome.run_id);
    let trailers = commit_trailers(&job.outcome.run_id, job.receipt);
    let mut command = protected.command(
        tools,
        &[
            "commit-tree",
            "--no-gpg-sign",
            "-p",
            &repo.head,
            "-m",
            title,
            "-m",
            &message,
            "-m",
            &trailers,
            &tree,
        ],
    );
    for (name, value) in identity(tools, repo).await? {
        command.env(name, value);
    }
    let commit = local(command, "git commit-tree").await?;
    if !commit.status.success() {
        return Err(git_failure("committing", &commit));
    }
    let commit = first_line(&commit);
    let zero = "0".repeat(repo.head.len());
    let reference = format!("refs/heads/{branch}");
    let reflog = format!("axocoatl: keep run {}", job.outcome.run_id);
    let created = local(
        repository_git(
            tools,
            &repo.root,
            &["update-ref", "-m", &reflog, &reference, &commit, &zero],
        ),
        "git update-ref",
    )
    .await?;
    if !created.status.success() {
        if repo.local_branch(tools, branch).await?.is_some() {
            return Err(refused(format!("the branch {branch} already exists")));
        }
        return Err(git_failure("creating the branch", &created));
    }
    Ok(Built {
        commit,
        paths,
        not_committed,
    })
}

async fn kept_paths(
    tools: &HostTools,
    protected: &ProtectedGit,
    head: &str,
    commit: &str,
) -> Result<Vec<String>, KeepPrError> {
    let parent = local(
        protected.command(
            tools,
            &["rev-parse", "--verify", "--quiet", &format!("{commit}^1")],
        ),
        "git rev-parse",
    )
    .await?;
    if first_line(&parent) != head {
        return Err(refused(format!(
            "the branch Keep created earlier ({commit}) is not based on HEAD {head}; keep it yourself or start a new run"
        )));
    }
    let diff = local(
        protected.command(
            tools,
            &[
                "diff-tree",
                "-r",
                "-z",
                "--no-renames",
                "--name-only",
                "--no-commit-id",
                head,
                commit,
            ],
        ),
        "git diff-tree",
    )
    .await?;
    if !diff.status.success() {
        return Err(git_failure("listing the kept paths", &diff));
    }
    Ok(nul_paths(&diff.stdout))
}

/// Commit the run's attributed paths to a new branch without touching the
/// person's checkout and, with `open_pr`, push it and open a pull request.
pub async fn keep(job: KeepJob<'_>) -> Result<KeepPrResponse, KeepPrError> {
    let request = job.request;
    let tools = job.tools;
    validate_request(request)?;
    if job.outcome.run_id != request.run_id || job.manifest.run_id != request.run_id {
        return Err(refused(format!(
            "the record loaded for Keep is not run {}",
            request.run_id
        )));
    }
    require_pass(job.outcome)?;
    if !job.attribution.unattributable.is_empty() {
        return Err(refused(format!(
            "the run's changes cannot be attributed exactly, so Keep as PR does not guess: {}",
            bounded(&job.attribution.unattributable.join("; "), 1200)
        )));
    }
    if let Some(note) = crate::run_attribution::unread_note(&job.attribution.unread) {
        return Err(refused(format!(
            "{}, so Keep as PR does not guess; ignore {} in .gitignore, or make {} {}",
            bounded(&note, 1200),
            if job.attribution.unread.len() == 1 { "it" } else { "them" },
            if job.attribution.unread.len() == 1 { "it" } else { "them" },
            if job.attribution.unread.len() == 1 { "a submodule" } else { "submodules" },
        )));
    }
    let git_paths: Vec<String> = job
        .attribution
        .paths
        .iter()
        .filter(|path| is_git_internal(path))
        .cloned()
        .collect();
    if !git_paths.is_empty() {
        return Err(refused(format!(
            "the run changed Git's own files ({}); Keep as PR does not run host git on a repository whose Git settings or hooks the run changed",
            listed(&git_paths)
        )));
    }
    let shared: Vec<String> = job
        .outside
        .paths
        .intersection(&job.attribution.paths)
        .cloned()
        .collect();
    if !shared.is_empty() {
        return Err(refused(format!(
            "turns of the Session outside the run also changed {}, so the run's work cannot be kept apart from theirs",
            listed(&shared)
        )));
    }
    if !job.outside.unattributable.is_empty() {
        return Err(refused(format!(
            "a turn of the Session outside the run changed files that cannot be attributed, so they could be among the run's paths: {}",
            bounded(&job.outside.unattributable.join("; "), 1200)
        )));
    }
    if let Some(path) = job
        .attribution
        .paths
        .iter()
        .find(|path| !is_plain_path(path))
    {
        return Err(refused(format!(
            "the run changed a path Keep cannot name to Git: {path:?}"
        )));
    }

    let repo = RepoFacts::read(tools, job.session_root).await?;
    match &job.manifest.repo_head {
        Some(head) if *head == repo.head => {}
        Some(head) => {
            return Err(refused(format!(
            "HEAD moved from {head} to {} after the run started; the run's checks ran on {head}",
            repo.head
        )))
        }
        None => return Err(refused(
            "the repository had no commit when the run started, so there is no base to branch from",
        )),
    }
    let changed_settings = repo.settings_changed_since(job.manifest.started_at_ms)?;
    if !changed_settings.is_empty() {
        return Err(refused(format!(
            "Git's settings changed after the run started ({}); check them, then keep the change yourself",
            changed_settings.join(", ")
        )));
    }

    let branch = branch_name(request, &job.outcome.loadout.id);
    check_branch_name(&branch).map_err(KeepPrError::Invalid)?;
    let checked = local(
        plain_git(
            tools,
            &["check-ref-format", &format!("refs/heads/{branch}")],
        ),
        "git check-ref-format",
    )
    .await?;
    if !checked.status.success() {
        return Err(KeepPrError::Invalid(format!(
            "Git refuses the branch name {branch:?}"
        )));
    }
    if repo.current_branch.as_deref() == Some(branch.as_str()) {
        return Err(refused(format!(
            "{branch} is the branch checked out in the repository"
        )));
    }
    let title = commit_title(request, job.outcome);

    // An existing branch is refused unless an earlier Keep of this run
    // created it at the commit it still names.
    let reused = match repo.local_branch(tools, &branch).await? {
        None => None,
        Some(oid) => match job.previous {
            Some(previous) if previous.branch == branch && previous.commit == oid => Some(oid),
            _ => return Err(refused(format!("the branch {branch} already exists"))),
        },
    };
    if let (Some(commit), Some(previous)) = (&reused, job.previous) {
        if !request.open_pr || previous.pull_request_url.is_some() {
            let protected = repo.protected(job.control_root)?;
            let paths = kept_paths(tools, &protected, &repo.head, commit).await?;
            return Ok(KeepPrResponse {
                branch,
                commit: commit.clone(),
                paths,
                pushed_to: None,
                pull_request_url: if request.open_pr {
                    previous.pull_request_url.clone()
                } else {
                    None
                },
                base: None,
                not_committed: Vec::new(),
                warnings: vec![format!(
                    "An earlier Keep of this run already created {}; nothing new was created.",
                    previous.branch
                )],
                receipt: job.receipt.map(OpenedReceipt::payload_digest),
            });
        }
    }

    let protected = repo.protected(job.control_root)?;
    let remote_name = request.remote.as_deref().unwrap_or(DEFAULT_REMOTE);
    let remote = if request.open_pr {
        let remote = RemoteTarget::resolve(tools, &repo, &protected, remote_name, &branch).await?;
        if branch == remote.default_branch {
            return Err(refused(format!(
                "{branch} is the default branch of remote {remote_name}; Keep as PR never pushes to it"
            )));
        }
        if let Some(existing) = &remote.existing {
            if reused.as_deref() != Some(existing.as_str()) {
                return Err(refused(format!(
                    "the branch {branch} already exists on remote {remote_name}; Keep as PR never overwrites a remote branch"
                )));
            }
        }
        Some(remote)
    } else {
        None
    };

    let mut warnings = job.attribution.notes.clone();
    if job.receipt.is_none() {
        warnings.push(
            "The run has no signed receipt, so the commit and pull request carry none.".into(),
        );
    }
    let (commit, paths, not_committed) = match &reused {
        Some(commit) => (
            commit.clone(),
            kept_paths(tools, &protected, &repo.head, commit).await?,
            Vec::new(),
        ),
        None => {
            let built = build_commit(&job, &repo, &protected, &branch, &title).await?;
            (built.commit, built.paths, built.not_committed)
        }
    };
    if !not_committed.is_empty() {
        warnings.push(format!(
            "Changed in the working tree but not by the run's Agents, so not committed: {}",
            listed(&not_committed)
        ));
    }
    let mut response = KeepPrResponse {
        branch: branch.clone(),
        commit: commit.clone(),
        paths,
        pushed_to: None,
        pull_request_url: None,
        base: None,
        not_committed,
        warnings,
        receipt: job.receipt.map(OpenedReceipt::payload_digest),
    };
    let Some(remote) = remote else {
        return Ok(response);
    };

    let after_branch = |message: String| KeepPrError::AfterBranch {
        branch: branch.clone(),
        commit: commit.clone(),
        message,
    };
    let reference = format!("refs/heads/{branch}");
    if remote.existing.is_none() {
        let staged = local(
            protected.command(tools, &["update-ref", &reference, &commit]),
            "git update-ref",
        )
        .await
        .map_err(|error| after_branch(message_of(error)))?;
        if !staged.status.success() {
            return Err(after_branch(format!(
                "preparing the push failed: {}",
                stderr_of(&staged)
            )));
        }
        let refspec = format!("{branch}:refs/heads/{branch}");
        let pushed = run_bounded(
            protected.command(tools, &["push", "--porcelain", &remote.name, &refspec]),
            KEEP_NETWORK_TIMEOUT,
            "git push",
        )
        .await
        .map_err(after_branch)?;
        if !pushed.status.success() {
            return Err(after_branch(format!(
                "pushing to {} failed: {}",
                remote.name,
                stderr_of(&pushed)
            )));
        }
    }
    response.pushed_to = Some(format!("{}/{branch}", remote.name));
    response.base = Some(remote.default_branch.clone());

    let body =
        pr_body(job.outcome, job.receipt).map_err(|error| after_branch(message_of(error)))?;
    let body_file = protected
        .write("pull-request-body.md", body.as_bytes())
        .map_err(after_branch)?;
    let body_file = body_file.to_string_lossy().into_owned();
    let created = run_bounded(
        gh_command(
            tools,
            protected.gh_dir(),
            &[
                "pr",
                "create",
                "--repo",
                &remote.slug,
                "--head",
                &branch,
                "--base",
                &remote.default_branch,
                "--title",
                &title,
                "--body-file",
                &body_file,
            ],
        ),
        KEEP_GH_TIMEOUT,
        "gh pr create",
    )
    .await
    .map_err(after_branch)?;
    if !created.status.success() {
        return Err(after_branch(format!(
            "gh pr create failed: {}",
            stderr_of(&created)
        )));
    }
    let url = String::from_utf8_lossy(&created.stdout)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with("https://") && !line.contains(char::is_whitespace))
        .map(|line| bounded(line, 2048));
    match url {
        Some(url) => response.pull_request_url = Some(url),
        None => {
            return Err(after_branch(
                "gh pr create printed no pull request URL".into(),
            ))
        }
    }
    Ok(response)
}

#[cfg(test)]
#[path = "keep_pr_tests.rs"]
pub(crate) mod tests;
