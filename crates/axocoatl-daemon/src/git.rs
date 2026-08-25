//! Git status/diff types and parsers for directory sessions.
//!
//! A session is (optionally auto-) a git repo; the daemon drives git inside the
//! session's sandbox container (`AxocoatlDaemon::session_git`), and these pure
//! parsers turn git's porcelain output into the shapes the dashboard's git pane
//! renders. Kept here (separate from the daemon impl) so the parsers are unit-
//! testable without a container.

use serde::{Deserialize, Serialize};

/// One changed path in the working tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitFile {
    pub path: String,
    /// `added` | `modified` | `deleted` | `renamed` | `untracked`.
    pub state: String,
    /// Lines added, when known.
    ///
    /// Porcelain status reports *that* a file changed, never how much. A
    /// reviewer deciding what to look at first needs the size of the change,
    /// and "3 files" tells them nothing about whether that is a typo fix or a
    /// rewrite. Filled from `--numstat`; `None` for binaries and for files git
    /// cannot diff, which is different from zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<u32>,
    /// Lines removed, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed: Option<u32>,
    /// The index holds a change to this file.
    ///
    /// Porcelain reports two independent columns — what is staged (X) and what
    /// is only in the working tree (Y) — and a file can be in both at once: an
    /// edit staged, then edited again. Collapsing them, as this did, makes
    /// staging unrepresentable, which is why there was no way to stage anything.
    #[serde(default)]
    pub staged: bool,
    /// The working tree holds a change that is not staged.
    #[serde(default)]
    pub unstaged: bool,
    /// The agent wrote this file in its most recent turn.
    ///
    /// Carried on the file rather than served as a separate list so "what the
    /// agent just did" is a *filter over git* instead of a parallel universe
    /// with its own vocabulary and its own way of being stale.
    #[serde(default)]
    pub last_turn: bool,
}

/// Working-tree status: current branch + changed files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitStatus {
    pub branch: String,
    pub files: Vec<GitFile>,
    pub clean: bool,
}

/// One file's before/after content — fed straight to Monaco's diff editor.
///
/// `binary` / `too_large` are escape hatches: when either is set, `old` and
/// `new` are blanked (the daemon never streams raw bytes or a multi-megabyte
/// blob into the JSON response or Monaco) and the pane shows a sentinel instead
/// of an inline diff.
#[derive(Debug, Clone, Serialize)]
pub struct GitDiff {
    pub path: String,
    pub old: String,
    pub new: String,
    pub binary: bool,
    pub too_large: bool,
}

/// Largest file (either side) the daemon will inline as a diff. Beyond this we
/// report `too_large` rather than shipping the content. 512 KiB.
pub const DIFF_MAX_BYTES: usize = 512 * 1024;

/// Heuristic binary check: a NUL byte in the first 8 KiB. Matches how git
/// itself decides "binary" for diffs, and survives the lossy-UTF-8 decode the
/// sandbox applies to command output (a real NUL stays a NUL).
pub fn looks_binary(s: &str) -> bool {
    s.as_bytes().iter().take(8192).any(|&b| b == 0)
}

/// Branch list + the current branch.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GitBranches {
    pub current: String,
    pub branches: Vec<String>,
}

/// How one lane of a variants run executes. Lanes are heterogeneous: a plan
/// produced by an expensive model can be executed by several cheaper ones in
/// parallel, and running the *same* task against *different* models is itself a
/// quality strategy (diverse approaches to choose between).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LaneConfig {
    /// Model this lane runs. `None` uses the agent's configured model.
    #[serde(default)]
    pub model: Option<String>,
    /// Agent this lane runs — its prompt, tools and memory, not just its model.
    /// `None` uses the session's agent.
    ///
    /// This is what turns a variants run from "the same agent with different
    /// models" into "different agents on the same task". Designing an agent by
    /// reasoning about it is guesswork; running three and letting the project's
    /// own checks decide is evidence.
    #[serde(default)]
    pub agent: Option<String>,
}

/// One parallel exploration: a `git worktree` on its own branch where a
/// variant agent runs, isolated from the other variants and from the
/// session's primary checkout.
// Deserialize as well as Serialize: the roster is written to disk beside the
// worktrees and read back, so a reload can still say what each lane is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Variant {
    /// 0-based lane index.
    pub index: usize,
    /// Set-scoped branch name. The attempt-set id and lane index together keep a
    /// later run from reusing this lane's branch.
    pub branch: String,
    /// Absolute path to this lane's set-scoped worktree.
    pub worktree: String,
    /// Model this lane ran, when known. Carried on the response so a comparison
    /// view can label each candidate and price its usage accurately.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Agent this lane ran. Present whenever lanes differ by agent, so a
    /// scoreboard can name the thing being compared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Provider that served this lane, when known. Older persisted rosters did
    /// not record it, so absence remains a valid backwards-compatible value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// Lifecycle of one attempt set as a whole.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptSetState {
    Preparing,
    Running,
    Ready,
    /// Repository checks are running or were interrupted and may be retried.
    Checking,
    Verified,
    Judged,
    Failed,
    /// Discard/rollback was durably authorized; exact cleanup may be retried.
    Discarding,
    /// Keep was authorized and its patch may be in the process of applying.
    Applying,
    /// The selected delta is present in the primary workspace.
    Applied,
    /// The chosen answer is durable in the session transcript; cleanup may be retried.
    TranscriptRecorded,
}

/// Execution lifecycle of one lane within an attempt set.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptLaneState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

/// Persisted lifecycle facts for one lane.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttemptLaneStatus {
    pub index: usize,
    pub state: AttemptLaneState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
}

/// Durable identity and immutable inputs for one set of parallel attempts.
///
/// Runtime status is recorded separately as [`AttemptLaneStatus`] so lane
/// completion can be updated without rewriting the identity of the set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttemptSet {
    pub id: String,
    pub session_id: String,
    pub task: String,
    pub instruction: String,
    pub base_sha: String,
    pub base_tree: String,
    pub state: AttemptSetState,
    /// Lane selected by a resumable Keep transaction. Persisting this before
    /// apply prevents a retry from switching candidates mid-transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_index: Option<usize>,
    pub created_at: u64,
    pub lanes: Vec<Variant>,
}

/// How one lane fared against the project's own check command.
///
/// This is the *fan-in* half of a variants run: N candidates are generated in
/// parallel, then each is judged by the repository's real checks (tests, build,
/// typecheck) so the failures are eliminated before a human reads a single diff.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LaneVerdict {
    /// 0-based lane index, matching [`Variant::index`].
    pub index: usize,
    /// True when the check command exited 0 — the lane survives.
    pub passed: bool,
    pub exit_code: i32,
    /// Tail of the check's combined output (capped by [`VERDICT_OUTPUT_MAX`]),
    /// so a failing lane can explain itself without shipping a whole test log.
    pub output: String,
    /// How many files this lane changed.
    ///
    /// Zero is the important case: a lane that changed nothing passes every
    /// check trivially, so `passed` alone would present it as a success. It
    /// happens for real — a model whose tool calls the runtime cannot parse
    /// completes a turn, burns tokens, and edits nothing, with no error
    /// anywhere. Carrying the count lets that be shown for what it is.
    #[serde(default)]
    pub changed_files: usize,
    /// Test files this lane changed.
    ///
    /// A passing check only means something if the tests were an *independent*
    /// arbiter. A lane that rewrote them graded its own work, and "passed" would
    /// be actively misleading — so this is reported rather than folded into
    /// `passed`, and the decision is left to a human who can see it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touched_tests: Vec<String>,
    /// Digest of the exact base-relative binary patch that passed this check.
    /// Keep recomputes it after freezing the lane and refuses stale verdicts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_sha256: Option<String>,
}

/// Whether a repo path looks like a test the check command would run.
///
/// A heuristic, deliberately broad: false positives merely prompt a human to
/// look, while a false negative would let a lane quietly mark its own homework.
pub fn looks_like_test(path: &str) -> bool {
    let p = path.to_lowercase();
    let file = p.rsplit('/').next().unwrap_or(&p);
    p.split('/').any(|seg| {
        matches!(
            seg,
            "test" | "tests" | "__tests__" | "spec" | "specs" | "e2e"
        )
    }) || file.contains(".test.")
        || file.contains("_test.")
        || file.contains(".spec.")
        || file.contains("_spec.")
        || file.starts_with("test_")
}

/// Most check output carried back per lane. Failures are what matter and the
/// interesting part is at the end, so this keeps the tail.
pub const VERDICT_OUTPUT_MAX: usize = 8 * 1024;

/// Keep the last [`VERDICT_OUTPUT_MAX`] bytes of check output, on a char
/// boundary so the result is always valid UTF-8.
pub fn verdict_tail(s: &str) -> String {
    if s.len() <= VERDICT_OUTPUT_MAX {
        return s.to_string();
    }
    let mut cut = s.len() - VERDICT_OUTPUT_MAX;
    while cut < s.len() && !s.is_char_boundary(cut) {
        cut += 1;
    }
    s[cut..].to_string()
}

/// Whether a model can actually drive a lane.
///
/// A lane model must return **structured** tool calls. Some models emit a
/// perfectly correct call as ordinary text instead, which the runtime never
/// sees — so the agent reads the prompt, answers in a few tokens, edits nothing,
/// and the run reports success because there was nothing to check. Establishing
/// this in one cheap call beats discovering it after a lane has burned minutes
/// producing an empty diff.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelProbe {
    pub model: String,
    /// True when the model returned a tool call in the structured field.
    pub usable: bool,
    /// What happened, in a sentence a user can act on.
    pub detail: String,
    /// Provider work performed by this preflight. Model checks are real,
    /// billable control-plane calls and are intentionally separate from Way
    /// execution totals.
    #[serde(default)]
    pub control_usage: ControlUsage,
}

/// Usage incurred outside the attempts themselves while preparing or judging
/// an Explore several ways run.
///
/// These calls are visible but deliberately not folded into lane economics:
/// doing so would attribute one shared planning, preflight, or judging call to
/// an arbitrary candidate. `token_usage_known == false` means at least one
/// dispatched call lacks a complete terminal measurement, not that it was free.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlUsage {
    /// Configured Agent used for the call. Model probes target a provider/model
    /// pair directly and therefore leave this unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub calls: usize,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub token_usage_known: bool,
}

impl ControlUsage {
    pub fn known(
        agent_id: Option<String>,
        calls: usize,
        usage: &axocoatl_core::TokenUsageStats,
    ) -> Self {
        Self::measured(agent_id, calls, usage, true)
    }

    pub fn measured(
        agent_id: Option<String>,
        calls: usize,
        usage: &axocoatl_core::TokenUsageStats,
        token_usage_known: bool,
    ) -> Self {
        Self {
            agent_id,
            calls,
            input_tokens: usage.input_tokens as u64,
            output_tokens: usage.output_tokens as u64,
            reasoning_tokens: usage.reasoning_tokens.unwrap_or(0) as u64,
            token_usage_known,
        }
    }

    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.reasoning_tokens)
    }

    /// Add another control-plane operation while keeping completeness sticky.
    /// A missing Usage response can never be repaired by a later successful
    /// call, so a combined total remains a lower bound once either side is.
    pub fn merge(&mut self, other: &Self) {
        if self.agent_id.is_none() {
            self.agent_id = other.agent_id.clone();
        }
        self.calls = self.calls.saturating_add(other.calls);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        self.token_usage_known &= other.token_usage_known;
    }
}

/// One file the plan expects to change, and what to do in it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanStep {
    pub path: String,
    /// The change, specific enough to act on without re-deriving intent.
    pub change: String,
}

/// A task turned into a spec precise enough for a weak model to execute.
///
/// The observed failure mode is not incapacity — it is ambiguity. Given a bare
/// task, a small model produced nothing at all, or syntactically broken code,
/// where a larger one inferred the missing detail and succeeded. The plan is
/// that inference, written down once by an expensive model so several cheap ones
/// can execute it: which files, what change, what not to touch, and how you know
/// it is done.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    /// The intent in a sentence.
    pub summary: String,
    pub steps: Vec<PlanStep>,
    /// What must NOT change — the guardrails a weak model otherwise wanders past.
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Observable conditions that mean the work is finished.
    #[serde(default)]
    pub acceptance: Vec<String>,
}

impl Plan {
    /// Render the plan as the instruction each lane receives.
    ///
    /// Deliberately plain imperative prose rather than JSON: this is read by the
    /// executing model, and the prompts that actually worked in testing looked
    /// like this — name the file, name the change, name what not to touch.
    pub fn render(&self, task: &str) -> String {
        let mut s = format!("{task}\n\nPlan:\n{}\n", self.summary);
        if !self.steps.is_empty() {
            s.push_str("\nMake these changes:\n");
            for step in &self.steps {
                s.push_str(&format!("- In {}: {}\n", step.path, step.change));
            }
        }
        if !self.constraints.is_empty() {
            s.push_str("\nDo not:\n");
            for c in &self.constraints {
                s.push_str(&format!("- {c}\n"));
            }
        }
        if !self.acceptance.is_empty() {
            s.push_str("\nDone when:\n");
            for a in &self.acceptance {
                s.push_str(&format!("- {a}\n"));
            }
        }
        s
    }
}

/// What one lane spent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LaneUsage {
    pub index: usize,
    /// Model this lane ran; `None` means the agent's configured default.
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Provider-reported reasoning tokens, billed at the configured output
    /// rate. Older attempt records predate this field and default to zero.
    #[serde(default)]
    pub reasoning_tokens: u64,
    /// Whether the token totals are complete. A failed remote request can cost
    /// something before returning no usage data; its persisted zeroes are an
    /// unknown volume, not evidence that no tokens were billed.
    #[serde(default)]
    pub token_usage_known: bool,
    /// What those tokens cost at this model's price. Local models are 0.
    pub cost_usd: f64,
    /// Whether `cost_usd` was resolved from a known price. False distinguishes
    /// an unknown price from a genuinely free local model.
    #[serde(default)]
    pub cost_known: bool,
    /// Wall-clock the lane's agent ran for, measured in the daemon.
    ///
    /// Timed here rather than in the viewer so a reload mid-run does not lose
    /// it, and so the number is the lane's actual work rather than the gap
    /// between two frames arriving over a socket.
    #[serde(default)]
    pub duration_ms: u64,
}

/// One attempt's durable natural-language outcome.
///
/// Code changes remain Git's source of truth, but the answer is part of the
/// visible session outcome and must survive reloads before the user chooses
/// which attempt to keep.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttemptLaneOutput {
    pub index: usize,
    pub content: String,
}

/// The economics of a variants run: what it cost, against what the same work
/// would have cost run entirely on one expensive model.
///
/// This is the argument the product rests on, so it is computed from real token
/// counts rather than estimated. The counterfactual is deliberately generous to
/// the alternative — it prices *the same* token volume at the baseline model's
/// rate, without assuming the frontier model would have needed several attempts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunCost {
    pub lanes: Vec<LaneUsage>,
    /// Sum of what the lanes actually cost.
    pub total_usd: f64,
    /// True only when every lane had a known price. If false, `total_usd` is
    /// the known subtotal and must not be presented as the run's full cost.
    #[serde(default)]
    pub actual_cost_known: bool,
    /// The model the comparison is drawn against.
    pub baseline_model: String,
    /// What the same tokens would have cost on `baseline_model`.
    pub baseline_usd: f64,
    /// Whether the baseline model has a configured (or known-local) price.
    #[serde(default)]
    pub baseline_cost_known: bool,
    /// `baseline_usd - total_usd`, floored at 0.
    pub saved_usd: f64,
    /// True when every lane has a known zero price — i.e. the run was entirely
    /// local, rather than containing an unpriced remote model.
    pub all_local: bool,
}

/// Price of a model, in dollars per million tokens.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelPrice {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

impl ModelPrice {
    /// Cost of a token count at this price. Callers include any separately
    /// reported reasoning tokens in `billable_output_tokens` because current
    /// provider pricing treats them as output spend.
    pub fn cost(&self, input_tokens: u64, billable_output_tokens: u64) -> f64 {
        (input_tokens as f64 / 1_000_000.0) * self.input_per_mtok
            + (billable_output_tokens as f64 / 1_000_000.0) * self.output_per_mtok
    }
}

/// One candidate's assessment in a [`Judgment`].
///
/// The useful output of judging N attempts is not a score — it is *why you would
/// pick this one*. A rank with no reasoning just moves the reading problem
/// around; the trade-off is the thing a reviewer actually needs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CandidateRationale {
    /// Lane index this assesses.
    pub index: usize,
    /// 1 = best.
    pub rank: usize,
    /// What this candidate did, in a sentence.
    pub approach: String,
    /// Why you would choose it — or wouldn't.
    pub tradeoffs: String,
}

/// A ranking over the candidates that survived verification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Judgment {
    /// Lane index of the recommended candidate.
    pub winner: usize,
    /// Every candidate assessed, best first.
    pub candidates: Vec<CandidateRationale>,
    /// The comparison in prose — what actually separates them.
    pub reasoning: String,
    /// The Judge Agent call. This is persisted with the judgment but kept out
    /// of per-Way execution cost.
    #[serde(default)]
    pub control_usage: ControlUsage,
}

/// Validate a judge response against the exact lanes that survived Checks.
///
/// Model-generated JSON is untrusted even after it deserializes: a judge can
/// omit a candidate, invent one, duplicate a rank, or recommend something that
/// was not ranked first. Persisting any of those would make the comparison lie
/// about the set it actually judged, so the daemon rejects them at the seam.
pub fn validate_judgment(judgment: &Judgment, expected_survivors: &[usize]) -> Result<(), String> {
    use std::collections::BTreeSet;

    let expected: BTreeSet<usize> = expected_survivors.iter().copied().collect();
    if expected.len() != expected_survivors.len() {
        return Err("expected survivor indices contain duplicates".to_string());
    }
    if expected.is_empty() {
        return Err("a judgment requires at least one surviving candidate".to_string());
    }

    let candidates: BTreeSet<usize> = judgment
        .candidates
        .iter()
        .map(|candidate| candidate.index)
        .collect();
    if candidates.len() != judgment.candidates.len() {
        return Err("the judgment contains a candidate more than once".to_string());
    }
    if candidates != expected {
        let missing: Vec<usize> = expected.difference(&candidates).copied().collect();
        let unexpected: Vec<usize> = candidates.difference(&expected).copied().collect();
        return Err(format!(
            "the judgment does not match the surviving candidates (missing: {missing:?}; unexpected: {unexpected:?})"
        ));
    }

    let ranks: BTreeSet<usize> = judgment
        .candidates
        .iter()
        .map(|candidate| candidate.rank)
        .collect();
    let expected_ranks: BTreeSet<usize> = (1..=expected.len()).collect();
    if ranks.len() != judgment.candidates.len() || ranks != expected_ranks {
        return Err(format!(
            "candidate ranks must be unique and cover 1 through {}",
            expected.len()
        ));
    }

    if !expected.contains(&judgment.winner) {
        return Err(format!(
            "winner {} is not a surviving candidate",
            judgment.winner
        ));
    }
    let winner_rank = judgment
        .candidates
        .iter()
        .find(|candidate| candidate.index == judgment.winner)
        .map(|candidate| candidate.rank);
    if winner_rank != Some(1) {
        return Err(format!(
            "winner {} must be the candidate ranked 1",
            judgment.winner
        ));
    }

    Ok(())
}

/// Everything a scoreboard needs about one comparison, in a single read.
///
/// A variants run outlives the page that started it: worktrees persist, checks
/// take minutes, and reloading a tab mid-run is ordinary. Without this the
/// comparison would come back as an empty table even though every result still
/// exists — so the daemon holds the answers and hands them back on request,
/// rather than the browser being the only place they were ever assembled.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RunResults {
    /// The attempt-set identity and immutable inputs. Absent for results written
    /// before attempt sets became first-class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_set: Option<AttemptSet>,
    /// The lanes, with the model or agent each one ran.
    ///
    /// Kept alongside `attempt_set.lanes` for wire compatibility with existing
    /// clients while they migrate to the set-scoped shape.
    pub lanes: Vec<Variant>,
    /// Current per-lane execution state. Older results did not persist it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lane_states: Vec<AttemptLaneStatus>,
    /// Verdicts from the last `verify`, empty if it has not been run.
    pub verdicts: Vec<LaneVerdict>,
    /// Token spend and wall-clock per lane, for lanes that have finished.
    pub usage: Vec<LaneUsage>,
    /// Completed natural-language outcomes, for lanes that produced one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<AttemptLaneOutput>,
    /// The last ranking, if a judge has been run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judgment: Option<Judgment>,
}

/// Derive the externally visible attempt-set state from its durable lane facts
/// and fan-in results.
///
/// Active work always wins over stale fan-in metadata. An all-terminal set in
/// which no lane completed is a failed set. Otherwise judgment outranks
/// verification, and a terminal set with at least one completed lane is ready
/// for fan-in.
pub fn derive_attempt_set_state(
    lanes: &[AttemptLaneStatus],
    verdicts: &[LaneVerdict],
    judgment: Option<&Judgment>,
) -> AttemptSetState {
    if lanes.iter().any(|lane| {
        matches!(
            lane.state,
            AttemptLaneState::Queued | AttemptLaneState::Running
        )
    }) {
        return AttemptSetState::Running;
    }

    let all_terminal_without_completion = !lanes.is_empty()
        && lanes.iter().all(|lane| {
            matches!(
                lane.state,
                AttemptLaneState::Failed
                    | AttemptLaneState::Cancelled
                    | AttemptLaneState::Interrupted
            )
        });
    if all_terminal_without_completion {
        return AttemptSetState::Failed;
    }
    if judgment.is_some() {
        return AttemptSetState::Judged;
    }
    if !verdicts.is_empty() {
        return AttemptSetState::Verified;
    }
    AttemptSetState::Ready
}

/// Largest patch carried into the judge prompt, per lane. Enough for a real
/// change; short of pasting a refactor of the whole tree into the context.
pub const JUDGE_PATCH_MAX: usize = 24 * 1024;

/// Bound a patch for the judge prompt without splitting a UTF-8 code point.
///
/// Returns the borrowed prefix and whether any bytes were omitted. Callers can
/// use the flag to add an explicit truncation marker without comparing lengths
/// or slicing the original string a second time.
pub fn truncate_judge_patch(patch: &str) -> (&str, bool) {
    if patch.len() <= JUDGE_PATCH_MAX {
        return (patch, false);
    }

    let mut end = JUDGE_PATCH_MAX;
    while !patch.is_char_boundary(end) {
        end -= 1;
    }
    (&patch[..end], true)
}

/// Strip a ``` fence if a model wrapped its JSON in one, so the payload parses.
pub fn unfence_json(s: &str) -> &str {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    // Drop an optional language tag on the opening fence.
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    rest.trim_start()
        .strip_suffix("```")
        .map(str::trim)
        .unwrap_or_else(|| rest.trim())
}

/// An attempt plus the changed paths shown by Compare. Before Checks this may
/// be a process-owned live worktree; afterwards it is the protected checked
/// candidate that Judge and Keep consume.
#[derive(Debug, Clone, Serialize)]
pub struct VariantStatus {
    pub index: usize,
    pub branch: String,
    pub worktree: String,
    pub status: GitStatus,
    /// A lane-specific protected-review failure. Keeping this on the lane lets
    /// Compare continue showing surviving checked candidates without calling a
    /// stopped lane's setup merely to fill in missing paths.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_error: Option<String>,
    /// Model this lane ran, recovered from the persisted roster.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Agent this lane ran, recovered from the persisted roster.
    ///
    /// Carried here because a lane's *identity* has to outlive the response
    /// that created it. The worktrees survive a reload and a daemon restart; if
    /// the labels did not, a comparison would come back as "lane 0 vs lane 1"
    /// and stop being a comparison of anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// Merge `git diff --numstat` output into a status, by path.
///
/// numstat reports `adds\tdels\tpath`, with `-` for binary files — which is
/// why the counts are optional rather than defaulting to zero: "we cannot count
/// this" and "nothing changed" are different facts and a reviewer reads them
/// differently.
pub fn apply_numstat(status: &mut GitStatus, numstat: &str) {
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        let (Some(a), Some(d), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        // Renames read either `old => new` or `dir/{old => new}/file`; rebuild
        // the destination path so it matches `--name-status`'s explicit new path.
        let path = numstat_destination_path(path);
        if let Some(f) = status.files.iter_mut().find(|f| f.path == path) {
            f.added = a.parse().ok();
            f.removed = d.parse().ok();
        }
    }
}

fn numstat_destination_path(path: &str) -> String {
    if let Some(open) = path.find('{') {
        if let Some(close_offset) = path[open + 1..].find('}') {
            let close = open + 1 + close_offset;
            if let Some((_, destination)) = path[open + 1..close].split_once(" => ") {
                return format!("{}{}{}", &path[..open], destination, &path[close + 1..]);
            }
        }
    }
    path.rsplit_once(" => ")
        .map(|(_, destination)| destination.to_string())
        .unwrap_or_else(|| path.to_string())
}

/// Build an attempt lane's changed-file status relative to its snapshot base.
///
/// Unlike porcelain status, `git diff <base>` includes changes already committed
/// on the lane branch. `name_status` should be the output of
/// `git diff --name-status <base> --`; `numstat` should come from the same diff.
/// Staging flags remain false because these two base-relative reports cannot say
/// whether a change currently lives in HEAD, the index, or the working tree.
pub fn parse_base_diff_status(branch: &str, name_status: &str, numstat: &str) -> GitStatus {
    let mut files = Vec::new();
    for line in name_status.lines() {
        let mut parts = line.split('\t');
        let Some(status) = parts.next().filter(|status| !status.is_empty()) else {
            continue;
        };
        let Some(first_path) = parts.next() else {
            continue;
        };
        let code = status.chars().next().unwrap_or('M');
        let (path, state) = match code {
            'A' => (first_path, "added"),
            'D' => (first_path, "deleted"),
            'R' => {
                let Some(destination) = parts.next() else {
                    continue;
                };
                (destination, "renamed")
            }
            // A detected copy leaves its source in place, so the destination is
            // an addition rather than a rename in the existing wire vocabulary.
            'C' => {
                let Some(destination) = parts.next() else {
                    continue;
                };
                (destination, "added")
            }
            _ => (first_path, "modified"),
        };
        files.push(GitFile {
            path: path.to_string(),
            state: state.to_string(),
            added: None,
            removed: None,
            staged: false,
            unstaged: false,
            last_turn: false,
        });
    }

    let mut status = GitStatus {
        branch: branch.trim().to_string(),
        clean: files.is_empty(),
        files,
    };
    apply_numstat(&mut status, numstat);
    status
}

/// One hunk of a unified diff, with everything needed to apply it alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hunk {
    /// Position in the file's diff, 0-based — how a caller names one.
    pub index: usize,
    /// The `@@ -a,b +c,d @@` line, verbatim.
    pub header: String,
    /// The hunk's body lines, verbatim, including their leading ` `/`+`/`-`.
    pub lines: Vec<String>,
    pub added: u32,
    pub removed: u32,
}

/// Split a file's unified diff into its preamble and hunks.
///
/// The preamble is every line before the first `@@` — `diff --git`, `index`,
/// `---`/`+++`, and any `new file mode`. Applying one hunk means re-emitting
/// that preamble with only that hunk under it, so git still knows which file is
/// being patched and how.
pub fn parse_hunks(diff: &str) -> (String, Vec<Hunk>) {
    let mut preamble = Vec::new();
    let mut hunks: Vec<Hunk> = Vec::new();
    for line in diff.lines() {
        if line.starts_with("@@") {
            hunks.push(Hunk {
                index: hunks.len(),
                header: line.to_string(),
                lines: Vec::new(),
                added: 0,
                removed: 0,
            });
            continue;
        }
        match hunks.last_mut() {
            None => preamble.push(line.to_string()),
            Some(h) => {
                // "\ No newline at end of file" is part of the hunk and must be
                // carried through, or applying it corrupts the file's ending.
                if line.starts_with('+') {
                    h.added += 1;
                } else if line.starts_with('-') {
                    h.removed += 1;
                }
                h.lines.push(line.to_string());
            }
        }
    }
    (preamble.join("\n"), hunks)
}

/// Rebuild a patch containing exactly one hunk.
///
/// Trailing newline is required: git rejects a patch that does not end in one,
/// with an error that reads like a corrupt patch rather than a missing byte.
pub fn one_hunk_patch(preamble: &str, hunk: &Hunk) -> String {
    let mut out = String::new();
    if !preamble.is_empty() {
        out.push_str(preamble);
        out.push('\n');
    }
    out.push_str(&hunk.header);
    out.push('\n');
    for l in &hunk.lines {
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// Mark the files an agent turn wrote.
///
/// Paths come from the turn's recorded actions, which are repo-relative, so they
/// compare directly against status paths. A file the agent wrote and the user
/// has since staged is still a file the agent wrote — the flags are independent
/// facts, not a state machine.
pub fn mark_last_turn(status: &mut GitStatus, touched: &[String]) {
    for f in status.files.iter_mut() {
        f.last_turn = touched.iter().any(|t| t == &f.path);
    }
}

/// Parse `git status --porcelain=v1 -b --untracked-files=all`.
pub fn parse_status(stdout: &str) -> GitStatus {
    let mut branch = String::new();
    let mut files = Vec::new();
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            // `main`, `main...origin/main [ahead 1]`, `No commits yet on main`,
            // or `HEAD (no branch)`.
            let b = rest
                .split("...")
                .next()
                .unwrap_or(rest)
                .split(" [")
                .next()
                .unwrap_or(rest);
            branch = b
                .trim_start_matches("No commits yet on ")
                .trim()
                .to_string();
            continue;
        }
        if line.len() < 4 {
            continue;
        }
        let xy = &line[..2];
        let mut chars = xy.chars();
        let (x, y) = (chars.next().unwrap_or(' '), chars.next().unwrap_or(' '));
        let mut path = line[3..].to_string();
        let untracked = xy == "??";
        // X is the index, Y the working tree. Untracked files are in neither
        // until added, so they count as unstaged rather than as both.
        let staged = !untracked && x != ' ';
        let unstaged = untracked || y != ' ';
        // The state names what happened to the file; prefer whichever column
        // actually carries a code, since a staged-only change has a blank Y.
        let code = if x != ' ' && !untracked { x } else { y };
        let state = if untracked {
            "untracked"
        } else {
            match code {
                'A' => "added",
                'D' => "deleted",
                'R' => "renamed",
                _ => "modified",
            }
        };
        // Renamed entries read `R  old -> new`; show the new path.
        if state == "renamed" {
            if let Some(idx) = path.find(" -> ") {
                path = path[idx + 4..].to_string();
            }
        }
        files.push(GitFile {
            path,
            state: state.to_string(),
            // Counts come from a separate numstat pass; porcelain has none.
            added: None,
            removed: None,
            staged,
            unstaged,
            last_turn: false,
        });
    }
    let clean = files.is_empty();
    GitStatus {
        branch,
        files,
        clean,
    }
}

/// Build the branch list from `git branch --format=%(refname:short)` plus the
/// current branch from `git rev-parse --abbrev-ref HEAD`.
pub fn parse_branches(current: &str, list: &str) -> GitBranches {
    GitBranches {
        current: current.trim().to_string(),
        branches: list
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parses_branch_and_states() {
        // Written without line continuations on purpose: `\` strips the leading
        // whitespace of the next line, which silently turns " D" (deleted in the
        // working tree) into "D " (deleted in the index) — the two columns this
        // parser now has to tell apart.
        let out =
            "## main...origin/main [ahead 1]\nM  src/lib.rs\n?? new.txt\nA  added.rs\n D gone.rs\n";
        let s = parse_status(out);
        assert_eq!(s.branch, "main");
        assert!(!s.clean);
        assert_eq!(s.files.len(), 4);
        assert_eq!(
            s.files[0],
            GitFile {
                path: "src/lib.rs".into(),
                state: "modified".into(),
                added: None,
                removed: None,
                staged: true,
                unstaged: false,
                last_turn: false,
            }
        );
        assert_eq!(s.files[1].state, "untracked");
        assert_eq!(s.files[2].state, "added");
        assert_eq!(s.files[3].state, "deleted");
        assert!(
            s.files[3].unstaged && !s.files[3].staged,
            "deleted in the working tree"
        );
    }

    #[test]
    fn status_handles_no_commits_and_clean() {
        let s = parse_status("## No commits yet on main\n");
        assert_eq!(s.branch, "main");
        assert!(s.clean);
        assert!(s.files.is_empty());
    }

    #[test]
    fn status_rename_takes_new_path() {
        let s = parse_status("## main\nR  old.rs -> new.rs\n");
        assert_eq!(
            s.files[0],
            GitFile {
                path: "new.rs".into(),
                state: "renamed".into(),
                added: None,
                removed: None,
                staged: true,
                unstaged: false,
                last_turn: false,
            }
        );
    }

    #[test]
    fn binary_heuristic() {
        assert!(looks_binary("text\0more"));
        assert!(looks_binary(&format!("{}\0", "a".repeat(8000))));
        assert!(!looks_binary("fn main() {}\nlet x = 1;\n"));
        assert!(!looks_binary(""));
        // A NUL past the 8 KiB scan window is not flagged.
        assert!(!looks_binary(&format!("{}\0", "a".repeat(9000))));
    }

    #[test]
    fn branches_parse() {
        let b = parse_branches("main\n", "main\naxo/variant-0\naxo/variant-1\n");
        assert_eq!(b.current, "main");
        assert_eq!(b.branches, vec!["main", "axo/variant-0", "axo/variant-1"]);
    }

    #[test]
    fn lane_config_parses_agents_as_well_as_models() {
        // The three comparison modes the API has to express: same agent with
        // different models, different agents entirely, and a mix.
        let lanes: Vec<LaneConfig> = serde_json::from_str(
            r#"[{"model":"qwen3-coder:30b"},{"agent":"careful"},{"agent":"fast","model":"qwen3:8b"},{}]"#,
        )
        .expect("lane configs parse");
        assert_eq!(lanes[0].model.as_deref(), Some("qwen3-coder:30b"));
        assert_eq!(lanes[0].agent, None);
        assert_eq!(lanes[1].agent.as_deref(), Some("careful"));
        assert_eq!(lanes[1].model, None, "an agent lane keeps its own model");
        assert_eq!(lanes[2].agent.as_deref(), Some("fast"));
        assert_eq!(lanes[2].model.as_deref(), Some("qwen3:8b"));
        assert!(lanes[3].agent.is_none() && lanes[3].model.is_none());
    }

    #[test]
    fn variant_labels_the_agent_that_produced_it() {
        let v = Variant {
            index: 0,
            branch: "axo/variant-0".into(),
            worktree: "/w/.axo-variants/0".into(),
            model: None,
            agent: Some("careful".into()),
            provider: None,
        };
        let j = serde_json::to_string(&v).unwrap();
        assert!(
            j.contains("careful"),
            "a scoreboard must be able to name it"
        );
        assert!(
            !j.contains("model"),
            "an unset model still stays off the wire"
        );
    }

    // No line continuations: `\` strips the next line's leading whitespace, and
    // in a diff that leading space *is* the context marker. The same trap made a
    // status fixture silently test the wrong thing.
    const TWO_HUNK_DIFF: &str = concat!(
        "diff --git a/lib/a.ts b/lib/a.ts\n",
        "index 111..222 100644\n",
        "--- a/lib/a.ts\n",
        "+++ b/lib/a.ts\n",
        "@@ -1,3 +1,4 @@\n",
        " const x = 1;\n",
        "+const y = 2;\n",
        " const z = 3;\n",
        " \n",
        "@@ -20,4 +21,3 @@ function f() {\n",
        "   return 1;\n",
        "-  // dead\n",
        " }\n",
    );

    #[test]
    fn hunks_split_on_at_at_and_keep_the_preamble() {
        let (pre, hunks) = parse_hunks(TWO_HUNK_DIFF);
        assert!(pre.starts_with("diff --git"), "preamble names the file");
        assert!(pre.contains("+++ b/lib/a.ts"));
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].added, 1);
        assert_eq!(hunks[0].removed, 0);
        assert_eq!(hunks[1].added, 0);
        assert_eq!(hunks[1].removed, 1);
        assert_eq!(hunks[1].index, 1);
    }

    #[test]
    fn one_hunk_patch_carries_the_preamble_and_ends_in_a_newline() {
        // Without the preamble git does not know which file this patches; with
        // no trailing newline it reports the patch as corrupt.
        let (pre, hunks) = parse_hunks(TWO_HUNK_DIFF);
        let patch = one_hunk_patch(&pre, &hunks[1]);
        assert!(patch.starts_with("diff --git a/lib/a.ts"));
        assert!(patch.contains("@@ -20,4 +21,3 @@"));
        assert!(!patch.contains("+const y = 2;"), "only the chosen hunk");
        assert!(patch.ends_with('\n'));
    }

    #[test]
    fn hunk_body_is_kept_verbatim() {
        // Context lines can be a bare space, and a stripped or trimmed line
        // makes the patch fail to apply for reasons that are hard to see.
        let (_, hunks) = parse_hunks(TWO_HUNK_DIFF);
        assert!(hunks[0].lines.contains(&" const x = 1;".to_string()));
        assert!(
            hunks[0].lines.contains(&" ".to_string()),
            "blank context line survives"
        );
    }

    #[test]
    fn last_turn_is_a_filter_over_git_not_a_separate_state() {
        let mut st = parse_status("## main\nM  agent.rs\n M mine.rs\n");
        mark_last_turn(&mut st, &["agent.rs".to_string()]);
        let f = |p: &str| st.files.iter().find(|f| f.path == p).unwrap();
        assert!(f("agent.rs").last_turn);
        assert!(!f("mine.rs").last_turn, "my own edit is not the agent's");
        // Independent facts: the agent wrote it *and* it is staged. Treating
        // these as one state would make either unrepresentable.
        assert!(f("agent.rs").staged && f("agent.rs").last_turn);
    }

    #[test]
    fn a_second_turn_replaces_the_first() {
        let mut st = parse_status("## main\n M a.rs\n M b.rs\n");
        mark_last_turn(&mut st, &["a.rs".to_string()]);
        mark_last_turn(&mut st, &["b.rs".to_string()]);
        let f = |p: &str| st.files.iter().find(|f| f.path == p).unwrap();
        assert!(!f("a.rs").last_turn, "it answers *last* turn, not *ever*");
        assert!(f("b.rs").last_turn);
    }

    #[test]
    fn status_separates_the_index_from_the_working_tree() {
        // The four cases that matter, and the third is the one collapsing X and
        // Y used to lose: a file staged *and* edited again since.
        let s = parse_status(
            "## main\nM  staged.rs\n M unstaged.rs\nMM both.rs\n?? new.rs\nA  added.rs\n",
        );
        let f = |p: &str| s.files.iter().find(|f| f.path == p).unwrap();
        assert!(f("staged.rs").staged && !f("staged.rs").unstaged);
        assert!(!f("unstaged.rs").staged && f("unstaged.rs").unstaged);
        assert!(
            f("both.rs").staged && f("both.rs").unstaged,
            "staged then edited again"
        );
        assert!(
            !f("new.rs").staged && f("new.rs").unstaged,
            "untracked is not staged"
        );
        assert_eq!(f("added.rs").state, "added");
        assert!(f("added.rs").staged);
    }

    #[test]
    fn a_staged_only_change_still_names_its_state() {
        // Y is blank when a change is staged and untouched since, so reading the
        // state from Y alone would report every staged file as "modified".
        let s = parse_status("## main\nD  gone.rs\nR  old.rs -> new.rs\n");
        assert_eq!(s.files[0].state, "deleted");
        assert_eq!(s.files[1].state, "renamed");
        assert_eq!(s.files[1].path, "new.rs");
    }

    #[test]
    fn numstat_sizes_the_changes_and_admits_when_it_cannot() {
        let mut st = GitStatus {
            branch: "main".into(),
            clean: false,
            files: vec![
                GitFile {
                    path: "lib/a.ts".into(),
                    state: "modified".into(),
                    added: None,
                    removed: None,
                    staged: false,
                    unstaged: true,
                    last_turn: false,
                },
                GitFile {
                    path: "img/logo.png".into(),
                    state: "modified".into(),
                    added: None,
                    removed: None,
                    staged: false,
                    unstaged: true,
                    last_turn: false,
                },
                GitFile {
                    path: "new.txt".into(),
                    state: "untracked".into(),
                    added: None,
                    removed: None,
                    staged: false,
                    unstaged: true,
                    last_turn: false,
                },
            ],
        };
        // `-` is git's way of saying a file cannot be counted, not that it is
        // unchanged — the two must not collapse into the same reading.
        apply_numstat(&mut st, "12\t3\tlib/a.ts\n-\t-\timg/logo.png\n");
        assert_eq!(st.files[0].added, Some(12));
        assert_eq!(st.files[0].removed, Some(3));
        assert_eq!(
            st.files[1].added, None,
            "a binary reports no count, not zero"
        );
        assert_eq!(
            st.files[2].added, None,
            "untracked files are absent from numstat"
        );
    }

    #[test]
    fn numstat_follows_a_rename_to_its_new_path() {
        let mut st = GitStatus {
            branch: "main".into(),
            clean: false,
            files: vec![
                GitFile {
                    path: "lib/new.ts".into(),
                    state: "renamed".into(),
                    added: None,
                    removed: None,
                    staged: false,
                    unstaged: true,
                    last_turn: false,
                },
                GitFile {
                    path: "src/new/name.rs".into(),
                    state: "renamed".into(),
                    added: None,
                    removed: None,
                    staged: false,
                    unstaged: true,
                    last_turn: false,
                },
            ],
        };
        apply_numstat(
            &mut st,
            "4\t1\tlib/old.ts => lib/new.ts\n3\t2\tsrc/{old => new}/name.rs\n",
        );
        assert_eq!(st.files[0].added, Some(4));
        assert_eq!(st.files[1].added, Some(3));
        assert_eq!(st.files[1].removed, Some(2));
    }

    #[test]
    fn base_diff_status_includes_changes_already_committed_by_an_attempt() {
        // These reports come from `git diff <attempt-base>`, so `committed.rs`
        // remains visible even when ordinary porcelain status is otherwise clean.
        let status = parse_base_diff_status(
            "axo/attempt-deadbeef-0",
            "M\tcommitted.rs\nA\tsrc/new.rs\nD\tgone.rs\nR100\tsrc/old.rs\tsrc/new_name.rs\nC075\ttemplate.rs\tcopy.rs\nT\tmode.sh\n",
            "1\t2\tcommitted.rs\n4\t0\tsrc/new.rs\n0\t5\tgone.rs\n3\t1\tsrc/{old.rs => new_name.rs}\n8\t0\ttemplate.rs => copy.rs\n-\t-\tmode.sh\n",
        );

        assert_eq!(status.branch, "axo/attempt-deadbeef-0");
        assert!(!status.clean);
        assert_eq!(status.files.len(), 6);
        let file = |path: &str| status.files.iter().find(|file| file.path == path).unwrap();
        assert_eq!(file("committed.rs").state, "modified");
        assert_eq!(file("committed.rs").added, Some(1));
        assert_eq!(file("committed.rs").removed, Some(2));
        assert_eq!(file("src/new.rs").state, "added");
        assert_eq!(file("gone.rs").state, "deleted");
        assert_eq!(file("src/new_name.rs").state, "renamed");
        assert_eq!(file("src/new_name.rs").added, Some(3));
        assert_eq!(file("copy.rs").state, "added");
        assert_eq!(file("copy.rs").added, Some(8));
        assert_eq!(file("mode.sh").state, "modified");
        assert_eq!(file("mode.sh").added, None, "binary count stays unknown");
        assert!(status
            .files
            .iter()
            .all(|file| !file.staged && !file.unstaged));
    }

    #[test]
    fn empty_base_diff_is_clean() {
        let status = parse_base_diff_status("  attempt  ", "", "");
        assert_eq!(status.branch, "attempt");
        assert!(status.clean);
        assert!(status.files.is_empty());
    }

    #[test]
    fn variant_roster_round_trips_through_disk() {
        // The roster is written beside the worktrees and read back by a later
        // process, so what a lane *is* has to survive the trip. Losing it turns
        // a comparison of two agents into "lane 0 vs lane 1".
        let roster = vec![
            Variant {
                index: 0,
                branch: "axo/variant-0".into(),
                worktree: "/w/.axo-variants/0".into(),
                model: None,
                agent: Some("careful".into()),
                provider: Some("ollama".into()),
            },
            Variant {
                index: 1,
                branch: "axo/variant-1".into(),
                worktree: "/w/.axo-variants/1".into(),
                model: Some("qwen3:8b".into()),
                agent: None,
                provider: None,
            },
        ];
        let back: Vec<Variant> =
            serde_json::from_str(&serde_json::to_string(&roster).unwrap()).unwrap();
        assert_eq!(back, roster);
    }

    #[test]
    fn attempt_states_use_stable_snake_case_wire_values() {
        let set_states = [
            (AttemptSetState::Preparing, "preparing"),
            (AttemptSetState::Running, "running"),
            (AttemptSetState::Ready, "ready"),
            (AttemptSetState::Checking, "checking"),
            (AttemptSetState::Verified, "verified"),
            (AttemptSetState::Judged, "judged"),
            (AttemptSetState::Failed, "failed"),
            (AttemptSetState::Discarding, "discarding"),
            (AttemptSetState::Applying, "applying"),
            (AttemptSetState::Applied, "applied"),
            (AttemptSetState::TranscriptRecorded, "transcript_recorded"),
        ];
        for (state, wire) in set_states {
            let json = format!("\"{wire}\"");
            assert_eq!(serde_json::to_string(&state).unwrap(), json);
            assert_eq!(
                serde_json::from_str::<AttemptSetState>(&json).unwrap(),
                state
            );
        }

        let lane_states = [
            (AttemptLaneState::Queued, "queued"),
            (AttemptLaneState::Running, "running"),
            (AttemptLaneState::Completed, "completed"),
            (AttemptLaneState::Failed, "failed"),
            (AttemptLaneState::Cancelled, "cancelled"),
            (AttemptLaneState::Interrupted, "interrupted"),
        ];
        for (state, wire) in lane_states {
            let json = format!("\"{wire}\"");
            assert_eq!(serde_json::to_string(&state).unwrap(), json);
            assert_eq!(
                serde_json::from_str::<AttemptLaneState>(&json).unwrap(),
                state
            );
        }
    }

    #[test]
    fn attempt_set_round_trip_keeps_identity_and_resolved_lane_provider() {
        let attempt = AttemptSet {
            id: "attempt-019".into(),
            session_id: "session-1".into(),
            task: "Make the parser streaming".into(),
            instruction: "Implement the reviewed plan".into(),
            base_sha: "abc123".into(),
            base_tree: "def456".into(),
            state: AttemptSetState::Running,
            kept_index: None,
            created_at: 1_722_222_222,
            lanes: vec![Variant {
                index: 0,
                branch: "axo/attempt-019/0".into(),
                worktree: "/w/.axo-variants/attempt-019/0".into(),
                model: Some("qwen3-coder".into()),
                agent: Some("careful".into()),
                provider: Some("ollama".into()),
            }],
        };

        let json = serde_json::to_string(&attempt).unwrap();
        let back: AttemptSet = serde_json::from_str(&json).unwrap();
        assert_eq!(back, attempt);
        assert!(json.contains(r#""state":"running""#));
        assert_eq!(back.lanes[0].provider.as_deref(), Some("ollama"));
    }

    #[test]
    fn attempt_types_read_pre_attempt_set_results() {
        let old_variant: Variant = serde_json::from_str(
            r#"{"index":0,"branch":"axo/variant-0","worktree":"/w/.axo-variants/0","model":null,"agent":"careful"}"#,
        )
        .expect("a roster without provider still parses");
        assert!(old_variant.provider.is_none());

        let old_results: RunResults = serde_json::from_str(
            r#"{"lanes":[{"index":0,"branch":"axo/variant-0","worktree":"/w/.axo-variants/0","model":null,"agent":null}],"verdicts":[],"usage":[{"index":0,"model":null,"input_tokens":10,"output_tokens":2,"cost_usd":0.0}]}"#,
        )
        .expect("results written before attempt-set identity still parse");
        assert!(old_results.attempt_set.is_none());
        assert!(old_results.lane_states.is_empty());
        assert_eq!(old_results.usage[0].reasoning_tokens, 0);
        assert!(!old_results.usage[0].cost_known);
    }

    fn attempt_lane(index: usize, state: AttemptLaneState) -> AttemptLaneStatus {
        AttemptLaneStatus {
            index,
            state,
            error: None,
            started_at: None,
            finished_at: None,
        }
    }

    fn passing_verdict(index: usize) -> LaneVerdict {
        LaneVerdict {
            index,
            passed: true,
            exit_code: 0,
            output: "ok".into(),
            changed_files: 1,
            touched_tests: vec![],
            patch_sha256: Some("abc".into()),
        }
    }

    fn sample_judgment() -> Judgment {
        Judgment {
            winner: 0,
            candidates: vec![],
            reasoning: "best fit".into(),
            control_usage: ControlUsage::default(),
        }
    }

    #[test]
    fn attempt_set_state_follows_lifecycle_precedence() {
        let verdicts = vec![passing_verdict(0)];
        let judgment = sample_judgment();

        assert_eq!(
            derive_attempt_set_state(
                &[
                    attempt_lane(0, AttemptLaneState::Completed),
                    attempt_lane(1, AttemptLaneState::Running),
                ],
                &verdicts,
                Some(&judgment),
            ),
            AttemptSetState::Running,
            "active work outranks stale fan-in results"
        );
        assert_eq!(
            derive_attempt_set_state(
                &[
                    attempt_lane(0, AttemptLaneState::Failed),
                    attempt_lane(1, AttemptLaneState::Cancelled),
                    attempt_lane(2, AttemptLaneState::Interrupted),
                ],
                &verdicts,
                Some(&judgment),
            ),
            AttemptSetState::Failed,
            "an all-terminal set with no completion failed"
        );
        assert_eq!(
            derive_attempt_set_state(
                &[
                    attempt_lane(0, AttemptLaneState::Completed),
                    attempt_lane(1, AttemptLaneState::Failed),
                ],
                &verdicts,
                Some(&judgment),
            ),
            AttemptSetState::Judged
        );
        assert_eq!(
            derive_attempt_set_state(
                &[attempt_lane(0, AttemptLaneState::Completed)],
                &verdicts,
                None,
            ),
            AttemptSetState::Verified
        );
        assert_eq!(
            derive_attempt_set_state(&[attempt_lane(0, AttemptLaneState::Completed)], &[], None,),
            AttemptSetState::Ready
        );
        assert_eq!(
            derive_attempt_set_state(&[], &[], None),
            AttemptSetState::Ready,
            "an empty persisted status does not vacuously fail"
        );
    }

    #[test]
    fn run_results_round_trip_keeps_every_axis_the_board_compares() {
        // One read has to carry identity, outcome, spend and ranking: the
        // scoreboard renders all four, and a reload rebuilds it from this alone.
        let results = RunResults {
            attempt_set: None,
            lanes: vec![Variant {
                index: 0,
                branch: "axo/variant-0".into(),
                worktree: "/w/.axo-variants/0".into(),
                model: None,
                agent: Some("careful".into()),
                provider: Some("ollama".into()),
            }],
            lane_states: vec![AttemptLaneStatus {
                index: 0,
                state: AttemptLaneState::Completed,
                error: None,
                started_at: Some(1_000),
                finished_at: Some(1_004),
            }],
            verdicts: vec![LaneVerdict {
                index: 0,
                passed: true,
                exit_code: 0,
                output: "ok".into(),
                changed_files: 2,
                touched_tests: vec![],
                patch_sha256: Some("def".into()),
            }],
            usage: vec![LaneUsage {
                index: 0,
                model: None,
                input_tokens: 100,
                output_tokens: 20,
                reasoning_tokens: 4,
                token_usage_known: true,
                cost_usd: 0.0,
                cost_known: true,
                duration_ms: 4_200,
            }],
            outputs: vec![AttemptLaneOutput {
                index: 0,
                content: "Implemented the selected route.".into(),
            }],
            judgment: Some(Judgment {
                winner: 0,
                candidates: vec![],
                reasoning: "only survivor".into(),
                control_usage: ControlUsage::default(),
            }),
        };
        let json = serde_json::to_string(&results).unwrap();
        let back: RunResults = serde_json::from_str(&json).unwrap();
        assert_eq!(back.lanes[0].agent.as_deref(), Some("careful"));
        assert_eq!(back.lanes[0].provider.as_deref(), Some("ollama"));
        assert_eq!(back.lane_states[0].state, AttemptLaneState::Completed);
        assert!(back.verdicts[0].passed);
        assert_eq!(back.usage[0].duration_ms, 4_200);
        assert_eq!(back.usage[0].reasoning_tokens, 4);
        assert!(back.usage[0].cost_known);
        assert_eq!(back.outputs[0].content, "Implemented the selected route.");
        assert_eq!(back.judgment.unwrap().winner, 0);
    }

    #[test]
    fn lane_usage_tolerates_a_record_written_before_durations_existed() {
        // Usage files persist across upgrades; an older one must still price,
        // reporting an unknown duration rather than failing the whole read.
        let old: LaneUsage = serde_json::from_str(
            r#"{"index":0,"model":null,"input_tokens":10,"output_tokens":2,"cost_usd":0.0}"#,
        )
        .expect("a record without duration_ms still parses");
        assert_eq!(old.duration_ms, 0);
        assert_eq!(old.reasoning_tokens, 0);
        assert!(!old.token_usage_known);
        assert!(!old.cost_known);
    }

    #[test]
    fn lane_config_parses_heterogeneous_models() {
        // The shape the API accepts: one plan, executed by several different
        // (typically cheaper) models concurrently — plus a lane that inherits
        // the agent's own model.
        let lanes: Vec<LaneConfig> =
            serde_json::from_str(r#"[{"model":"qwen3-coder"},{"model":"deepseek-v3"},{}]"#)
                .expect("lane configs parse");
        assert_eq!(lanes.len(), 3);
        assert_eq!(lanes[0].model.as_deref(), Some("qwen3-coder"));
        assert_eq!(lanes[1].model.as_deref(), Some("deepseek-v3"));
        assert_eq!(
            lanes[2].model, None,
            "an empty lane inherits the agent's model"
        );
    }

    #[test]
    fn verdict_tail_keeps_the_end_and_stays_utf8() {
        // Short output passes through untouched.
        assert_eq!(verdict_tail("all tests passed"), "all tests passed");

        // Long output keeps the TAIL — a failing suite's useful part is the end.
        let long = format!("{}FAILED: 3 tests", "x".repeat(VERDICT_OUTPUT_MAX));
        let tail = verdict_tail(&long);
        assert!(tail.len() <= VERDICT_OUTPUT_MAX);
        assert!(tail.ends_with("FAILED: 3 tests"));

        // Truncating mid-multibyte-char must not panic or produce invalid UTF-8.
        let multi = "é".repeat(VERDICT_OUTPUT_MAX);
        let tail = verdict_tail(&multi);
        assert!(tail.chars().all(|c| c == 'é'));
    }

    #[test]
    fn judge_patch_truncation_is_bounded_and_utf8_safe() {
        let short = "small patch";
        assert_eq!(truncate_judge_patch(short), (short, false));

        let exact = "x".repeat(JUDGE_PATCH_MAX);
        let (kept, truncated) = truncate_judge_patch(&exact);
        assert_eq!(kept.len(), JUDGE_PATCH_MAX);
        assert!(!truncated);

        // The byte limit lands between the two bytes of the final `é`.
        let crossing = format!("{}é", "x".repeat(JUDGE_PATCH_MAX - 1));
        let (kept, truncated) = truncate_judge_patch(&crossing);
        assert!(truncated);
        assert_eq!(kept.len(), JUDGE_PATCH_MAX - 1);
        assert!(kept.chars().all(|ch| ch == 'x'));

        let long_ascii = "y".repeat(JUDGE_PATCH_MAX + 100);
        let (kept, truncated) = truncate_judge_patch(&long_ascii);
        assert!(truncated);
        assert_eq!(kept.len(), JUDGE_PATCH_MAX);
    }

    #[test]
    fn recognises_the_test_files_a_lane_must_not_grade_itself_with() {
        for p in [
            "lib/orders.test.ts",
            "src/foo_test.go",
            "tests/integration.rs",
            "__tests__/App.tsx",
            "spec/models/order_spec.rb",
            "e2e/checkout.ts",
            "test_math.py",
        ] {
            assert!(looks_like_test(p), "{p} should be recognised as a test");
        }
        for p in [
            "lib/orders.ts",
            "src/latest.rs", // contains "test" but is not one
            "src/protest/main.rs",
            "README.md",
        ] {
            assert!(!looks_like_test(p), "{p} should NOT be flagged");
        }
    }

    #[test]
    fn plan_renders_the_instruction_a_weak_model_needs() {
        let plan = Plan {
            summary: "Replace the switch with a lookup table.".into(),
            steps: vec![PlanStep {
                path: "lib/orders.ts".into(),
                change: "rewrite compareBy using a Record<SortKey, …> map".into(),
            }],
            constraints: vec!["modify any test file".into()],
            acceptance: vec!["npm run check passes".into()],
        };
        let out = plan.render("Refactor compareBy.");
        // The task leads; the plan supplies what a small model would otherwise
        // have to infer — the file, the change, the guardrail, the finish line.
        assert!(out.starts_with("Refactor compareBy."));
        assert!(out.contains("In lib/orders.ts: rewrite compareBy"));
        assert!(out.contains("Do not:\n- modify any test file"));
        assert!(out.contains("Done when:\n- npm run check passes"));
    }

    #[test]
    fn plan_render_omits_empty_sections() {
        let bare = Plan {
            summary: "Do the thing.".into(),
            steps: vec![],
            constraints: vec![],
            acceptance: vec![],
        };
        let out = bare.render("Task.");
        assert!(!out.contains("Do not:"));
        assert!(!out.contains("Done when:"));
        assert!(!out.contains("Make these changes:"));
    }

    #[test]
    fn model_price_computes_per_million_tokens() {
        let p = ModelPrice {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        };
        // 1M in + 1M out = 3 + 15.
        assert!((p.cost(1_000_000, 1_000_000) - 18.0).abs() < 1e-9);
        // Sub-million scales linearly.
        assert!((p.cost(500_000, 0) - 1.5).abs() < 1e-9);
        // An unpriced model — the default — is free, which is what local means.
        assert_eq!(ModelPrice::default().cost(9_999_999, 9_999_999), 0.0);
    }

    #[test]
    fn unfence_json_handles_how_models_actually_reply() {
        let want = r#"{"winner":1}"#;
        assert_eq!(unfence_json(want), want, "bare JSON passes through");
        assert_eq!(unfence_json("```json\n{\"winner\":1}\n```"), want);
        assert_eq!(unfence_json("```\n{\"winner\":1}\n```"), want);
        assert_eq!(unfence_json("  \n{\"winner\":1}\n  "), want);
        // An unterminated fence still yields parseable content rather than
        // failing the whole judgment.
        assert_eq!(unfence_json("```json\n{\"winner\":1}"), want);
    }

    #[test]
    fn judgment_round_trips() {
        let j = Judgment {
            winner: 2,
            candidates: vec![CandidateRationale {
                index: 2,
                rank: 1,
                approach: "Extracted a helper and reused it".into(),
                tradeoffs: "Clearer, but adds an indirection".into(),
            }],
            reasoning: "Candidate 2 fits the existing style".into(),
            control_usage: ControlUsage::default(),
        };
        let parsed: Judgment = serde_json::from_str(&serde_json::to_string(&j).unwrap()).unwrap();
        assert_eq!(parsed, j);
    }

    fn candidate(index: usize, rank: usize) -> CandidateRationale {
        CandidateRationale {
            index,
            rank,
            approach: format!("candidate {index}"),
            tradeoffs: "trade-off".into(),
        }
    }

    #[test]
    fn judgment_validation_requires_the_exact_survivors_and_ranks() {
        let valid = Judgment {
            winner: 2,
            candidates: vec![candidate(5, 2), candidate(2, 1)],
            reasoning: "two is the best fit".into(),
            control_usage: ControlUsage::default(),
        };
        assert_eq!(validate_judgment(&valid, &[2, 5]), Ok(()));

        let mut invalid = valid.clone();
        invalid.candidates.pop();
        assert!(validate_judgment(&invalid, &[2, 5])
            .unwrap_err()
            .contains("missing: [2]"));

        let mut invalid = valid.clone();
        invalid.candidates[1].index = 5;
        assert!(validate_judgment(&invalid, &[2, 5])
            .unwrap_err()
            .contains("more than once"));

        let mut invalid = valid.clone();
        invalid.candidates[0].index = 7;
        let error = validate_judgment(&invalid, &[2, 5]).unwrap_err();
        assert!(error.contains("missing: [5]"));
        assert!(error.contains("unexpected: [7]"));

        let mut invalid = valid.clone();
        invalid.candidates[0].rank = 1;
        assert!(validate_judgment(&invalid, &[2, 5])
            .unwrap_err()
            .contains("unique and cover 1 through 2"));

        let mut invalid = valid.clone();
        invalid.candidates[0].rank = 3;
        assert!(validate_judgment(&invalid, &[2, 5])
            .unwrap_err()
            .contains("unique and cover 1 through 2"));

        let mut invalid = valid.clone();
        invalid.winner = 9;
        assert!(validate_judgment(&invalid, &[2, 5])
            .unwrap_err()
            .contains("not a surviving candidate"));

        let mut invalid = valid;
        invalid.winner = 5;
        assert!(validate_judgment(&invalid, &[2, 5])
            .unwrap_err()
            .contains("ranked 1"));

        assert!(validate_judgment(
            &Judgment {
                winner: 0,
                candidates: vec![],
                reasoning: String::new(),
                control_usage: ControlUsage::default(),
            },
            &[],
        )
        .is_err());
        assert!(validate_judgment(&candidate_judgment_for_test(), &[2, 2]).is_err());
    }

    fn candidate_judgment_for_test() -> Judgment {
        Judgment {
            winner: 2,
            candidates: vec![candidate(2, 1)],
            reasoning: "only survivor".into(),
            control_usage: ControlUsage::default(),
        }
    }

    #[test]
    fn variant_omits_model_when_not_overridden() {
        // Uniform runs stay wire-compatible: no `model` key at all.
        let v = Variant {
            index: 0,
            branch: "axo/variant-0".into(),
            worktree: "/w/.axo-variants/0".into(),
            model: None,
            agent: None,
            provider: None,
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(
            !json.contains("model"),
            "unset model must not serialize: {json}"
        );

        let labelled = Variant {
            model: Some("qwen3-coder".into()),
            ..v
        };
        assert!(serde_json::to_string(&labelled)
            .unwrap()
            .contains("qwen3-coder"));
    }
}
