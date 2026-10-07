//! The Outcome of a loadout run and what the run driver observes of each
//! turn. Pure data: no storage, no daemon. See `docs/design/1.3-loadouts.md`
//! ("Outcome extensions").
//!
//! Owner: workstream `core`. Other workstreams fill these types (review-qa:
//! adjudications and reproductions; audit: findings and areas; runtime:
//! failure classes and not-covered entries; e2e: check reports; keep: keep
//! results) but change their shape only through `core`.

use serde::{Deserialize, Serialize};

/// `schema` of a serialized [`RunOutcome`].
pub const RUN_OUTCOME_SCHEMA: &str = "axocoatl.run-outcome/1";

/// Process exit codes of `axocoatl run`.
pub mod exit_code {
    /// Every required check passed, the required review (if any) approved,
    /// every review finding was adjudicated, nothing is not covered and no
    /// finding requires attention.
    pub const PASS: i32 = 0;
    /// A required check failed (or timed out) on the final candidate.
    pub const CHECKS_FAILED: i32 = 1;
    /// The run needs a person: review not passed, unadjudicated findings,
    /// not-covered areas, blocked, budget or wall-clock exhausted, or
    /// findings the loadout fails on.
    pub const NEEDS_ATTENTION: i32 = 2;
    /// Bad flags, unknown or invalid loadout, missing parameter.
    pub const USAGE: i32 = 3;
    /// The daemon could not be reached or refused the local API token.
    pub const DAEMON_UNAVAILABLE: i32 = 4;
    /// The run could not execute: environment, sandbox, provider setup, or
    /// a daemon error.
    pub const INFRASTRUCTURE: i32 = 5;
    /// The person interrupted the run (Ctrl-C); the turn was stopped.
    pub const INTERRUPTED: i32 = 6;
    /// Busy: another run, Session turn or operation holds the run's
    /// Workspace, so the run was not admitted or its next turn could not
    /// start. Nothing is wrong with the run itself; run it again later.
    pub const BUSY: i32 = 7;
}

/// The overall result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunVerdict {
    Pass,
    ChecksFailed,
    NeedsAttention,
    Interrupted,
    Error,
}

impl RunVerdict {
    /// The process exit code of this verdict.
    pub fn exit_code(self) -> i32 {
        match self {
            RunVerdict::Pass => exit_code::PASS,
            RunVerdict::ChecksFailed => exit_code::CHECKS_FAILED,
            RunVerdict::NeedsAttention => exit_code::NEEDS_ATTENTION,
            RunVerdict::Interrupted => exit_code::INTERRUPTED,
            RunVerdict::Error => exit_code::INFRASTRUCTURE,
        }
    }
}

/// An exact provider and model, as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelIdentity {
    pub provider: String,
    pub model: String,
    /// `native`, `claude-code` or `codex`.
    #[serde(default = "native_runtime")]
    pub runtime: String,
}

fn native_runtime() -> String {
    "native".into()
}

/// The loadout a run used, exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadoutRef {
    pub id: String,
    pub version: u32,
    /// `fix`, `qa`, `audit` or `custom`.
    pub kind: String,
    /// SHA-256 of the exact loadout file.
    pub digest: String,
    pub builtin: bool,
}

/// A warning shown in the UI, API, run output and record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunWarning {
    /// Stable code, such as [`SAME_MODEL_REVIEWER`].
    pub code: String,
    pub message: String,
}

/// Warning code: the reviewer runs the writer's model.
pub const SAME_MODEL_REVIEWER: &str = "same_model_reviewer";

/// The same-model warning when `reviewer` equals any of `writers`.
pub fn same_model_warning(
    writers: &[ModelIdentity],
    reviewer: &ModelIdentity,
) -> Option<RunWarning> {
    writers
        .iter()
        .any(|writer| writer.provider == reviewer.provider && writer.model == reviewer.model)
        .then(|| RunWarning {
            code: SAME_MODEL_REVIEWER.into(),
            message: format!(
                "The reviewer runs the writer's model ({}:{}). A same-model second look \
                 measured no gain; choose a different reviewer model.",
                reviewer.provider, reviewer.model
            ),
        })
}

/// State of one required check on the final candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Passed,
    Failed,
    TimedOut,
    /// Never ran on the final candidate (an earlier failure, a stop, budget).
    NotRun,
    /// Its record could not be read.
    Unavailable,
}

/// One test case from a check's JUnit or e2e `report.json` (workstream `e2e`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckTestCase {
    pub suite: String,
    pub name: String,
    /// `passed`, `failed`, `skipped` or `error`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// The parsed report a check produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckReport {
    /// `junit` or `e2e_report_json`.
    pub format: String,
    /// SHA-256 of the report bytes as read, bound to the check run's stdout
    /// marker (see the e2e section of the spec).
    pub sha256: String,
    pub tests: Vec<CheckTestCase>,
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    pub errors: u32,
    /// Test cases left out to stay within the bound.
    #[serde(default)]
    pub truncated: u32,
}

/// One required check's result in the Outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    /// The loadout check name, or `check-<n>` for a team without names.
    pub name: String,
    pub argv: Vec<String>,
    pub state: CheckState,
    pub timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Last bytes of stdout and stderr, bounded.
    #[serde(default)]
    pub stdout_tail: String,
    #[serde(default)]
    pub stderr_tail: String,
    /// The repository tree the check ran on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<CheckReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A reviewer's verdict, as recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdictKind {
    Approve,
    Changes,
    Unreadable,
}

/// One finding of one review round, with its id (`F1`, `F2`, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub id: String,
    pub text: String,
}

/// One review round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRound {
    pub round: u32,
    pub verdict: ReviewVerdictKind,
    pub passed: bool,
    /// The findings text as recorded, bounded.
    pub findings_text: String,
    /// The findings split by id (workstream `review-qa`).
    #[serde(default)]
    pub findings: Vec<ReviewFinding>,
    /// Whether the host sent the findings back to the writer.
    pub continued: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_sha256: Option<String>,
}

/// The required review in the Outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewOutcome {
    pub reviewer: ModelIdentity,
    pub max_rounds: u32,
    pub rounds: Vec<ReviewRound>,
    /// Whether the final verdict approves the final candidate.
    pub passed: bool,
    /// `approved`, `changes`, `failed`, `skipped`, `not_run` or `unavailable`
    /// (as `TurnReviewView::state`).
    pub state: String,
    pub reason: String,
}

/// What the writer answered to one finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdjudicationDecision {
    Accept,
    Reject,
    /// The writer's answer has no adjudication for this finding.
    Missing,
}

/// One adjudication: a review finding and the writer's answer to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Adjudication {
    /// The review round whose finding this answers.
    pub round: u32,
    pub finding_id: String,
    pub finding: String,
    pub decision: AdjudicationDecision,
    /// The writer's reason; empty only for `missing`.
    pub reason: String,
    /// The writer generation whose answer carried it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_generation: Option<u32>,
}

/// Where a finding came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSource {
    Reviewer,
    Explorer,
    AuditWorker,
    Integrator,
}

/// `low`, `medium`, `high` or `critical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

/// How a reproduction behaved on the build under test and the reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReproClassification {
    /// Fails on the build under test and passes on the reference.
    Confirmed,
    /// Fails on both: listed as "fails on clean build", never confirmed.
    FailsOnCleanBuild,
    /// Fails on the build under test; no reference is configured.
    Reproduced,
    /// Passes on the build under test.
    NotReproduced,
    /// The reproduction could not run (syntax error, timeout, browser error).
    ReproError,
    /// The finding names no reproduction, or the file is missing.
    Missing,
}

/// One run of a reproduction against one base URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReproRun {
    pub base_url: String,
    /// `passed`, `failed` or `error`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_error: Option<String>,
}

/// A finding's reproduction and its classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReproResult {
    /// Repository path of the Playwright test.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub classification: ReproClassification,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ReproRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<ReproRun>,
}

/// A finding in the Outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub source: FindingSource,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    /// Audit area or QA area.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
    /// `path:line` when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repro: Option<ReproResult>,
}

/// Why something was not covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// The provider refused, or a safety classifier stopped the stream.
    ProviderRefusal,
    /// Transient errors that survived the one retry, or a 5xx.
    ProviderFailure,
    /// 400/401/402/403: not retried.
    ProviderRejected,
    /// A grant, budget or wall-clock limit ran out.
    Budget,
    /// The Agent reported it could not proceed, or the host blocked it.
    Blocked,
    /// The Agent never reached the area.
    NotReached,
    /// A tool-round, context or other runtime limit.
    RuntimeLimit,
    /// Stopped by a person.
    Stopped,
    /// Anything else; the message says what.
    Other,
}

impl FailureClass {
    /// The class as serialized: `provider_refusal`, `not_reached`, ...
    pub fn as_str(self) -> &'static str {
        match self {
            FailureClass::ProviderRefusal => "provider_refusal",
            FailureClass::ProviderFailure => "provider_failure",
            FailureClass::ProviderRejected => "provider_rejected",
            FailureClass::Budget => "budget",
            FailureClass::Blocked => "blocked",
            FailureClass::NotReached => "not_reached",
            FailureClass::RuntimeLimit => "runtime_limit",
            FailureClass::Stopped => "stopped",
            FailureClass::Other => "other",
        }
    }
}

/// One area, helper, slot or check the run did not cover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotCovered {
    /// The area name, helper or slot id, or check name.
    pub area: String,
    pub class: FailureClass,
    pub detail: String,
    /// The node of the turn graph, when one is involved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

impl NotCovered {
    /// Why it was not covered, in one line: `<class>: <detail>`, the one
    /// rendering the run summary and the JUnit `coverage` suite use. The
    /// class is said once: a detail that already starts with it
    /// (`not_reached: ran out of steps`, `not reached: ...`) is not prefixed
    /// again, and an empty detail leaves the class alone.
    pub fn reason(&self) -> String {
        let class = self.class.as_str();
        let detail = self.detail.trim();
        let detail = match detail.split_once(':') {
            Some((head, rest))
                if head.trim().to_ascii_lowercase().replace([' ', '-'], "_") == class =>
            {
                rest.trim()
            }
            _ if detail.replace([' ', '-'], "_").eq_ignore_ascii_case(class) => "",
            _ => detail,
        };
        if detail.is_empty() {
            class.to_owned()
        } else {
            format!("{class}: {detail}")
        }
    }
}

/// Tokens and cost of the run, with completeness.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_microunits: u64,
    /// False when any call's usage was unknown; the numbers are then a known
    /// subtotal.
    pub complete: bool,
    /// Provider calls retried under the transient-error policy.
    #[serde(default)]
    pub retries: u32,
}

/// What the network record holds for the run's Session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkSummary {
    pub events: u64,
    pub allowed_connections: u64,
    pub refused_connections: u64,
    pub route_requests: u64,
    /// Route hosts and their request counts.
    #[serde(default)]
    pub routes: Vec<(String, u64)>,
}

/// The result of Keep as PR (workstream `keep`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepResult {
    pub branch: String,
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One turn the run started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunTurnRef {
    pub turn_id: String,
    /// What the turn was for: `run`, `audit_plan`, `audit_areas`,
    /// `audit_integrate`.
    pub purpose: String,
    pub state: TurnState,
}

/// The Outcome of one loadout run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunOutcome {
    pub schema: String,
    pub run_id: String,
    pub session_id: String,
    pub workspace_id: String,
    pub loadout: LoadoutRef,
    pub task: String,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub verdict: RunVerdict,
    pub exit_code: i32,
    /// Why the run needs attention, in words, one per reason.
    #[serde(default)]
    pub attention: Vec<String>,
    #[serde(default)]
    pub turns: Vec<RunTurnRef>,
    #[serde(default)]
    pub checks: Vec<CheckResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewOutcome>,
    #[serde(default)]
    pub adjudications: Vec<Adjudication>,
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub not_covered: Vec<NotCovered>,
    #[serde(default)]
    pub warnings: Vec<RunWarning>,
    #[serde(default)]
    pub usage: RunUsage,
    #[serde(default)]
    pub network: NetworkSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<KeepResult>,
    /// The infrastructure error that ended the run, when the verdict is
    /// `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Facts the verdict is decided from, apart from the Outcome lists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VerdictInputs {
    /// The loadout makes findings need attention (qa default; audit opt-in).
    pub fail_on_findings: bool,
    /// The final turn ended needing attention, failed or blocked.
    pub turn_needs_attention: bool,
    /// The wall clock or a budget ran out.
    pub budget_exhausted: bool,
    pub interrupted: bool,
}

impl RunOutcome {
    /// Decide the verdict and exit code from the Outcome's lists, fill
    /// `verdict`, `exit_code` and `attention`, and return the exit code.
    ///
    /// Precedence: error > interrupted > checks failed > needs attention >
    /// pass. A required check that failed or timed out on the final
    /// candidate is a checks failure; a check that never ran on it (or whose
    /// record cannot be read), a review that did not pass, a
    /// missing adjudication, anything not covered, an unfinished turn, an
    /// exhausted budget, or (with `fail_on_findings`) a confirmed or
    /// reproduced finding needs attention. Nothing is ever silently a pass.
    pub fn decide(&mut self, inputs: VerdictInputs) -> i32 {
        let mut attention = Vec::new();
        let checks_failed = self
            .checks
            .iter()
            .any(|check| matches!(check.state, CheckState::Failed | CheckState::TimedOut));
        for check in &self.checks {
            if matches!(check.state, CheckState::NotRun | CheckState::Unavailable) {
                attention.push(format!(
                    "The required check {} did not run on the final result",
                    check.name
                ));
            }
        }
        if let Some(review) = &self.review {
            if !review.passed {
                attention.push(format!(
                    "The required review did not pass: {}",
                    review.reason
                ));
            }
        }
        let missing = self
            .adjudications
            .iter()
            .filter(|a| a.decision == AdjudicationDecision::Missing)
            .count();
        if missing > 0 {
            attention.push(format!(
                "The writer did not answer {missing} review finding{}",
                if missing == 1 { "" } else { "s" }
            ));
        }
        // Areas, not entries: one area can have several entries (each part
        // of it a worker did not reach).
        let areas = self
            .not_covered
            .iter()
            .map(|entry| entry.area.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        if areas > 0 {
            attention.push(format!(
                "{areas} area{} not covered",
                if areas == 1 { " was" } else { "s were" }
            ));
        }
        if inputs.turn_needs_attention {
            attention.push("A turn ended needing attention".into());
        }
        if inputs.budget_exhausted {
            attention.push("A budget or the wall clock ran out".into());
        }
        if inputs.fail_on_findings {
            let counted = self
                .findings
                .iter()
                .filter(|finding| match &finding.repro {
                    Some(repro) => matches!(
                        repro.classification,
                        ReproClassification::Confirmed | ReproClassification::Reproduced
                    ),
                    None => finding.source != FindingSource::Explorer,
                })
                .count();
            if counted > 0 {
                attention.push(format!(
                    "{counted} finding{} need{} attention",
                    if counted == 1 { "" } else { "s" },
                    if counted == 1 { "s" } else { "" }
                ));
            }
        }
        let verdict = if self.error.is_some() {
            RunVerdict::Error
        } else if inputs.interrupted {
            RunVerdict::Interrupted
        } else if checks_failed {
            RunVerdict::ChecksFailed
        } else if !attention.is_empty() {
            RunVerdict::NeedsAttention
        } else {
            RunVerdict::Pass
        };
        self.verdict = verdict;
        self.exit_code = verdict.exit_code();
        self.attention = attention;
        self.exit_code
    }
}

/// How a turn ended, as the run driver observes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Running,
    Completed,
    NeedsAttention,
    Failed,
    Stopped,
    Interrupted,
}

/// How one node (Agent activation) ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    NeverStarted,
    Running,
    Accepted,
    Failed,
    Stopped,
    Blocked,
    Superseded,
}

/// One generation of a node: its state and accepted answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationObservation {
    pub generation: u32,
    pub state: NodeState,
    /// The accepted (or final partial) answer, bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<NodeFailure>,
}

/// Why a node failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeFailure {
    pub class: FailureClass,
    pub message: String,
}

/// One node of a turn graph as observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeObservation {
    pub node_id: String,
    /// The team slot, or the helper template for a delegated helper.
    pub slot_id: String,
    pub model: ModelIdentity,
    pub required: bool,
    /// `lead`, `helper`, `reviewer` or `slot`.
    pub kind: String,
    pub generations: Vec<GenerationObservation>,
}

impl NodeObservation {
    /// The latest generation, when there is one.
    pub fn latest(&self) -> Option<&GenerationObservation> {
        self.generations.iter().max_by_key(|g| g.generation)
    }
}

/// Everything the run driver reads of one finished turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnObservation {
    pub session_id: String,
    pub turn_id: String,
    pub state: TurnState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention_reason: Option<String>,
    pub nodes: Vec<NodeObservation>,
    pub checks: Vec<CheckResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewOutcome>,
    pub usage: RunUsage,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome() -> RunOutcome {
        RunOutcome {
            schema: RUN_OUTCOME_SCHEMA.into(),
            run_id: "run-1".into(),
            session_id: "ses-1".into(),
            workspace_id: "wsp-1".into(),
            loadout: LoadoutRef {
                id: "fix".into(),
                version: 1,
                kind: "fix".into(),
                digest: "0".repeat(64),
                builtin: true,
            },
            task: "t".into(),
            started_at_ms: 1,
            finished_at_ms: 2,
            verdict: RunVerdict::Pass,
            exit_code: 0,
            attention: Vec::new(),
            turns: Vec::new(),
            checks: Vec::new(),
            review: None,
            adjudications: Vec::new(),
            findings: Vec::new(),
            not_covered: Vec::new(),
            warnings: Vec::new(),
            usage: RunUsage::default(),
            network: NetworkSummary::default(),
            keep: None,
            error: None,
        }
    }

    fn check(state: CheckState) -> CheckResult {
        CheckResult {
            name: "tests".into(),
            argv: vec!["npm".into(), "test".into()],
            state,
            timeout_ms: 180_000,
            exit_code: None,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            candidate_sha256: None,
            report: None,
            reason: None,
        }
    }

    #[test]
    fn a_clean_run_passes() {
        let mut run = outcome();
        run.checks.push(check(CheckState::Passed));
        assert_eq!(run.decide(VerdictInputs::default()), exit_code::PASS);
        assert!(run.attention.is_empty());
    }

    #[test]
    fn checks_failure_outranks_attention() {
        let mut run = outcome();
        run.checks.push(check(CheckState::TimedOut));
        run.not_covered.push(NotCovered {
            area: "checkout".into(),
            class: FailureClass::ProviderRefusal,
            detail: "classifier stop".into(),
            node_id: None,
            turn_id: None,
        });
        assert_eq!(
            run.decide(VerdictInputs::default()),
            exit_code::CHECKS_FAILED
        );
        assert!(!run.attention.is_empty());
    }

    #[test]
    fn not_covered_and_missing_adjudications_need_attention() {
        let mut run = outcome();
        run.checks.push(check(CheckState::Passed));
        run.adjudications.push(Adjudication {
            round: 1,
            finding_id: "F1".into(),
            finding: "off by one".into(),
            decision: AdjudicationDecision::Missing,
            reason: String::new(),
            writer_generation: Some(2),
        });
        assert_eq!(
            run.decide(VerdictInputs::default()),
            exit_code::NEEDS_ATTENTION
        );
        let mut run = outcome();
        run.not_covered.push(NotCovered {
            area: "search".into(),
            class: FailureClass::Budget,
            detail: "budget".into(),
            node_id: None,
            turn_id: None,
        });
        assert_eq!(
            run.decide(VerdictInputs::default()),
            exit_code::NEEDS_ATTENTION
        );
    }

    /// The audit re-smoke's summary said "4 areas were not covered" when
    /// two areas had four entries between them.
    #[test]
    fn the_attention_line_counts_areas_not_entries() {
        let entry = |area: &str, detail: &str| NotCovered {
            area: area.into(),
            class: FailureClass::NotReached,
            detail: detail.into(),
            node_id: None,
            turn_id: None,
        };
        let mut run = outcome();
        run.not_covered = vec![
            entry("ingest", "ingest/src/lib.rs"),
            entry("ingest", "ingest/src/main.rs"),
            entry("ingest", "ingest/**/*.rs"),
            entry("notify", "the report could not be read"),
        ];
        assert_eq!(
            run.decide(VerdictInputs::default()),
            exit_code::NEEDS_ATTENTION
        );
        assert_eq!(run.attention, ["2 areas were not covered"]);
        run.not_covered.truncate(3);
        run.decide(VerdictInputs::default());
        assert_eq!(run.attention, ["1 area was not covered"]);
    }

    #[test]
    fn findings_that_fail_on_a_clean_build_do_not_count() {
        let finding = |classification| Finding {
            id: "B1".into(),
            source: FindingSource::Explorer,
            title: "t".into(),
            detail: String::new(),
            severity: None,
            area: None,
            location: None,
            repro: Some(ReproResult {
                path: ".axocoatl/qa/b1.spec.ts".into(),
                sha256: None,
                classification,
                target: None,
                reference: None,
            }),
        };
        let inputs = VerdictInputs {
            fail_on_findings: true,
            ..VerdictInputs::default()
        };
        let mut run = outcome();
        run.findings
            .push(finding(ReproClassification::FailsOnCleanBuild));
        assert_eq!(run.decide(inputs), exit_code::PASS);
        let mut run = outcome();
        run.findings.push(finding(ReproClassification::Confirmed));
        assert_eq!(run.decide(inputs), exit_code::NEEDS_ATTENTION);
    }

    #[test]
    fn an_error_outranks_everything() {
        let mut run = outcome();
        run.checks.push(check(CheckState::Failed));
        run.error = Some("environment failed".into());
        assert_eq!(
            run.decide(VerdictInputs::default()),
            exit_code::INFRASTRUCTURE
        );
        let mut run = outcome();
        assert_eq!(
            run.decide(VerdictInputs {
                interrupted: true,
                ..VerdictInputs::default()
            }),
            exit_code::INTERRUPTED
        );
    }

    #[test]
    fn same_model_reviewer_is_warned() {
        let writer = ModelIdentity {
            provider: "openrouter".into(),
            model: "qwen/qwen3-coder".into(),
            runtime: "native".into(),
        };
        let other = ModelIdentity {
            model: "openai/gpt-oss-120b".into(),
            ..writer.clone()
        };
        assert!(same_model_warning(std::slice::from_ref(&writer), &other).is_none());
        let warning = same_model_warning(std::slice::from_ref(&writer), &writer).unwrap();
        assert_eq!(warning.code, SAME_MODEL_REVIEWER);
    }

    #[test]
    fn failure_class_names_match_their_serialized_form() {
        use FailureClass::*;
        for class in [
            ProviderRefusal,
            ProviderFailure,
            ProviderRejected,
            Budget,
            Blocked,
            NotReached,
            RuntimeLimit,
            Stopped,
            Other,
        ] {
            assert_eq!(serde_json::to_value(class).unwrap(), class.as_str());
        }
    }

    /// The qa smoke test's summary read "checkout: not_reached: not_reached:
    /// ran out of steps": the detail already named the status.
    #[test]
    fn a_not_covered_reason_names_its_class_once() {
        let entry = |class, detail: &str| NotCovered {
            area: "checkout".into(),
            class,
            detail: detail.into(),
            node_id: None,
            turn_id: None,
        };
        use FailureClass::*;
        for (class, detail, reason) in [
            (
                NotReached,
                "not_reached: ran out of steps",
                "not_reached: ran out of steps",
            ),
            (NotReached, "Not reached: no time", "not_reached: no time"),
            (
                Blocked,
                "blocked: the page did not load",
                "blocked: the page did not load",
            ),
            (
                ProviderFailure,
                "provider_failure: the explorer did not finish (stream ended early: x: y)",
                "provider_failure: the explorer did not finish (stream ended early: x: y)",
            ),
            (NotReached, "not_reached", "not_reached"),
            (Other, "", "other"),
            (Other, "  ", "other"),
            // A different word before a colon is part of the detail.
            (
                Other,
                "skipped: not a valid status",
                "other: skipped: not a valid status",
            ),
            (
                ProviderRefusal,
                "classifier stop",
                "provider_refusal: classifier stop",
            ),
            (Budget, "budget:", "budget"),
        ] {
            assert_eq!(entry(class, detail).reason(), reason, "{detail:?}");
        }
    }

    #[test]
    fn outcome_round_trips_with_its_schema() {
        let mut run = outcome();
        run.decide(VerdictInputs::default());
        let text = serde_json::to_string(&run).unwrap();
        assert!(text.contains(RUN_OUTCOME_SCHEMA));
        let back: RunOutcome = serde_json::from_str(&text).unwrap();
        assert_eq!(back, run);
    }
}
