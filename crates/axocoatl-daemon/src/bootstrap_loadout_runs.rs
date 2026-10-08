//! Loadouts and loadout runs on the live daemon: the registry of built-in
//! and user loadouts, run admission (Workspace, Session with the loadout's
//! sandbox binding, environment approval), the run record, JUnit and the
//! record bundle. Owner: workstream `core`.
//!
//! The registry is read on each request (no cache; the file set is small).
//! A run's record lives in `{data root}/loadout-runs/{run_id}/` and is never
//! evicted with its Session: it is the audit trail.
use super::*;
use crate::loadout::api::{
    LoadoutGraph, LoadoutGraphEdge, LoadoutGraphNode, LoadoutParamView, LoadoutSummary,
    LoadoutView, RunAccepted, RunEventsPage, RunRequest, RunStatusView, ValidateLoadoutRequest,
    ValidateLoadoutResponse,
};
use crate::loadout::host::CheckLabel;
use crate::loadout::{RunContext, RunError, RunOptions};
use crate::SessionTeamEdit;
use axocoatl_config::loadout::{
    builtin_loadouts, load_user_loadouts, parse_loadout, resolve_loadout, AgentRuntime, CheckRun,
    Loadout, LoadoutError, LoadoutFile, LoadoutKind, LoadoutRole, LoadoutSource, ParamKind,
    ParamOr, USER_LOADOUT_DIR,
};
use axocoatl_session::record_bundle::{BundleHeader, RECORD_BUNDLE_SCHEMA};
use axocoatl_session::run_outcome::{
    exit_code, CheckResult, CheckState, GenerationObservation, LoadoutRef, ModelIdentity,
    NetworkSummary, NodeFailure, NodeObservation, NodeState, ReviewOutcome, RunOutcome, RunUsage,
    RunVerdict, RunWarning, TurnObservation, TurnState,
};
use axocoatl_session::run_record::{
    RunEvent, RunManifest, RunRecordError, RunRecordStore, SessionLoadoutBinding,
    RUN_MANIFEST_SCHEMA,
};

/// Longest task a run accepts, in bytes.
pub const MAX_RUN_TASK_BYTES: usize = 64 * 1024;
/// Longest text `POST /api/loadouts/validate` accepts.
pub const MAX_VALIDATE_BYTES: usize = 128 * 1024;
/// Most bytes of a check's stdout or stderr kept in the Outcome.
const CHECK_TAIL_BYTES: usize = 4 * 1024;
/// Most bytes of a review round's findings kept in the Outcome.
const FINDINGS_TEXT_BYTES: usize = 24 * 1024;
/// Network-record events read per page.
const NETWORK_PAGE: usize = 1000;
/// Run events read per bundle page.
const RUN_EVENT_PAGE: usize = 1000;
/// How the session dispatch begins the output it keeps for an activation
/// that failed with an error (`session_dispatch_run.rs`): that output is the
/// failure's reason, unlike the partial answer kept for a stopped one.
const ACTIVATION_FAILED: &str = "Activation failed:";
/// Longest admission waits to observe the context an Ollama model is
/// loaded with (a load without a prompt, as the run's first call does).
const ADMISSION_CONTEXT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// What the daemon keeps about loadout runs while it runs: the record store,
/// admissions by request id, which runs have a driver and which a person
/// asked to stop.
pub(crate) struct LoadoutRuns {
    store: Result<RunRecordStore, String>,
    /// `request_id` → the request's digest and what its admission returned.
    admitted: StdMutex<HashMap<String, (String, RunAccepted, RunContext)>>,
    /// Request ids being admitted now.
    admitting: StdMutex<HashSet<String>>,
    drivers: StdMutex<HashSet<String>>,
    stops: StdMutex<HashSet<String>>,
    /// Runs whose loadout has an e2e check, known before the run's manifest
    /// is written: the Session's first container already mounts
    /// `.e2e/cache` read-only.
    e2e_runs: StdMutex<HashSet<String>>,
}

impl LoadoutRuns {
    pub(crate) fn open(data_root: &SecureDir) -> Self {
        Self {
            store: RunRecordStore::open(data_root).map_err(|error| error.to_string()),
            admitted: StdMutex::new(HashMap::new()),
            admitting: StdMutex::new(HashSet::new()),
            drivers: StdMutex::new(HashSet::new()),
            stops: StdMutex::new(HashSet::new()),
            e2e_runs: StdMutex::new(HashSet::new()),
        }
    }

    fn store(&self) -> Result<&RunRecordStore, DaemonError> {
        self.store.as_ref().map_err(|error| {
            DaemonError::Session(format!("the loadout run record is unavailable: {error}"))
        })
    }
}

fn record_error(error: RunRecordError) -> DaemonError {
    match error {
        RunRecordError::NotFound(run) => DaemonError::NotFound(format!("no run {run}")),
        RunRecordError::Invalid(message) => DaemonError::InvalidRequest(message),
        RunRecordError::NotImplemented(what) => DaemonError::NotImplemented(what),
        other => DaemonError::Session(other.to_string()),
    }
}

fn run_error(error: RunError) -> DaemonError {
    match error {
        RunError::NotImplemented(what) => DaemonError::NotImplemented(what),
        RunError::Usage(message) => DaemonError::InvalidRequest(message),
        RunError::Busy(detail) => DaemonError::WorkspaceBusy(detail),
        other => DaemonError::Session(other.to_string()),
    }
}

fn now_ms() -> u64 {
    crate::loadout::driver::now_ms()
}

/// Why a run is refused on a data directory that still uses the 1.0 Session
/// format (one that holds Sessions, or that a daemon already used), and how
/// to upgrade it. A new data directory, and an existing one no daemon has
/// used yet that holds no Session, start in the native format by themselves.
pub(crate) const LEGACY_ROOT_REFUSAL: &str = "loadout runs need native Session history, and \
     this data directory still uses the 1.0 Session format. Stop Axocoatl, make a cold \
     backup of the data directory, run `axocoatl session upgrade --confirm` with the same \
     configuration and AXOCOATL_DATA_DIR, then start Axocoatl again";

/// One loadout as the registry lists it: usable, or why not.
struct RegistryEntry {
    loadout: Result<Loadout, LoadoutError>,
    path: Option<std::path::PathBuf>,
}

fn param_text<T: std::fmt::Display>(value: &ParamOr<T>) -> String {
    match value {
        ParamOr::Param { param } => format!("param {param}"),
        ParamOr::Value(value) => value.to_string(),
    }
}

fn kind_name(kind: ParamKind) -> &'static str {
    match kind {
        ParamKind::Model => "model",
        ParamKind::Text => "text",
        ParamKind::Url => "url",
    }
}

/// The row of `GET /api/loadouts` for a usable loadout.
pub fn loadout_summary(loadout: &Loadout) -> LoadoutSummary {
    let file = &loadout.file;
    LoadoutSummary {
        id: file.id.clone(),
        version: file.version,
        name: file.name.clone(),
        description: file.description.clone(),
        kind: file.kind.to_string(),
        builtin: loadout.source == LoadoutSource::Builtin,
        opt_in: file.opt_in,
        digest: loadout.digest.clone(),
        params: file
            .params
            .iter()
            .map(|(name, param)| LoadoutParamView {
                name: name.clone(),
                kind: kind_name(param.kind).into(),
                required: param.required,
                default: param.default.clone(),
                description: param.description.clone(),
            })
            .collect(),
        warnings: loadout.warnings.clone(),
        error: None,
        path: match &loadout.source {
            LoadoutSource::Builtin => None,
            LoadoutSource::User { path } => Some(path.display().to_string()),
        },
    }
}

/// The row of a user loadout file that cannot be used.
fn invalid_summary(path: &std::path::Path, error: &LoadoutError) -> LoadoutSummary {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("loadout")
        .to_string();
    LoadoutSummary {
        id: stem.clone(),
        version: 0,
        name: path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("loadout")
            .to_string(),
        description: String::new(),
        kind: String::new(),
        builtin: false,
        opt_in: false,
        digest: String::new(),
        params: Vec::new(),
        warnings: Vec::new(),
        error: Some(error.to_string()),
        path: Some(path.display().to_string()),
    }
}

fn role_name(role: LoadoutRole) -> &'static str {
    match role {
        LoadoutRole::Writer => "writer",
        LoadoutRole::Explorer => "explorer",
        LoadoutRole::Planner => "planner",
        LoadoutRole::Worker => "worker",
        LoadoutRole::Integrator => "integrator",
    }
}

fn check_run_text(run: &CheckRun) -> String {
    if let Some(argv) = &run.argv {
        argv.join(" ")
    } else if let Some(shell) = &run.shell {
        format!("sh -c {}", param_text(shell))
    } else if run.detected {
        "the repository's check command (or --check)".into()
    } else if let Some(e2e) = &run.e2e {
        format!(
            "e2e {} (model {})",
            e2e.args.join(" "),
            param_text(&e2e.model)
        )
    } else {
        "nothing".into()
    }
}

/// The display graph of a loadout: one node per Agent (an audit's worker as
/// its area workers), one per check, one for the reviewer; edges for
/// dependencies and the host's order (Agents → checks → review). Nothing in
/// it is executable.
pub fn loadout_graph(file: &LoadoutFile) -> LoadoutGraph {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for agent in &file.agents {
        let area_workers = file.kind == LoadoutKind::Audit && agent.role == LoadoutRole::Worker;
        let mut detail = vec![param_text(&agent.model)];
        if agent.runtime != AgentRuntime::Native {
            detail.push(format!(
                "runtime {}",
                match agent.runtime {
                    AgentRuntime::ClaudeCode => "claude-code",
                    AgentRuntime::Codex => "codex",
                    AgentRuntime::Native => "native",
                }
            ));
        } else {
            detail.push(format!("tools: {}", agent.tools.join(", ")));
        }
        detail.push(match &agent.writes {
            None => "writes: every path".into(),
            Some(paths) if paths.is_empty() => "read-only".into(),
            Some(paths) => format!("writes: {}", paths.join(", ")),
        });
        if area_workers {
            if let Some(audit) = &file.audit {
                detail.push(format!(
                    "{}-{} areas, one fresh read-only worker each, in parallel",
                    audit.min_areas, audit.max_areas
                ));
            }
        }
        nodes.push(LoadoutGraphNode {
            id: format!("agent:{}", agent.id),
            kind: if area_workers {
                "area_workers"
            } else {
                "agent"
            }
            .into(),
            label: format!("{} ({})", agent.id, role_name(agent.role)),
            detail,
        });
        for parent in &agent.depends_on {
            edges.push(LoadoutGraphEdge {
                from: format!("agent:{parent}"),
                to: format!("agent:{}", agent.id),
            });
        }
    }
    if file.kind == LoadoutKind::Audit {
        let id_of = |role: LoadoutRole| {
            file.agents
                .iter()
                .find(|agent| agent.role == role)
                .map(|agent| format!("agent:{}", agent.id))
        };
        if let (Some(planner), Some(worker), Some(integrator)) = (
            id_of(LoadoutRole::Planner),
            id_of(LoadoutRole::Worker),
            id_of(LoadoutRole::Integrator),
        ) {
            for (from, to) in [(planner, worker.clone()), (worker, integrator)] {
                if !edges.iter().any(|edge| edge.from == from && edge.to == to) {
                    edges.push(LoadoutGraphEdge { from, to });
                }
            }
        }
    }
    // The Agents no other Agent waits for hand their result to the host.
    let terminal: Vec<String> = file
        .agents
        .iter()
        .filter(|agent| {
            !file
                .agents
                .iter()
                .any(|other| other.depends_on.contains(&agent.id))
        })
        .filter(|agent| file.kind != LoadoutKind::Audit || agent.role == LoadoutRole::Integrator)
        .map(|agent| format!("agent:{}", agent.id))
        .collect();
    let mut tails = terminal.clone();
    for check in &file.checks {
        let id = format!("check:{}", check.name);
        nodes.push(LoadoutGraphNode {
            id: id.clone(),
            kind: "check".into(),
            label: check.name.clone(),
            detail: vec![
                check_run_text(&check.run),
                format!("timeout {}", check.timeout.as_deref().unwrap_or("3m")),
            ],
        });
        for from in &terminal {
            edges.push(LoadoutGraphEdge {
                from: from.clone(),
                to: id.clone(),
            });
        }
    }
    if !file.checks.is_empty() {
        tails = file
            .checks
            .iter()
            .map(|check| format!("check:{}", check.name))
            .collect();
    }
    if let Some(review) = &file.review {
        nodes.push(LoadoutGraphNode {
            id: "review".into(),
            kind: "review".into(),
            label: "reviewer".into(),
            detail: vec![
                param_text(&review.model),
                format!(
                    "{} round{}",
                    review.rounds,
                    if review.rounds == 1 { "" } else { "s" }
                ),
                format!("tools: {}", review.tools.join(", ")),
            ],
        });
        for from in tails {
            edges.push(LoadoutGraphEdge {
                from,
                to: "review".into(),
            });
        }
    }
    LoadoutGraph { nodes, edges }
}

/// Whether the loadout's findings make a run need attention.
fn fail_on_findings(file: &LoadoutFile) -> bool {
    file.qa.as_ref().is_some_and(|qa| qa.fail_on_findings)
        || file
            .audit
            .as_ref()
            .is_some_and(|audit| audit.fail_on_findings)
}

fn check_state(view: &axocoatl_session::turn_checks::TurnCheckView) -> CheckState {
    match view.state.as_str() {
        "passed" => CheckState::Passed,
        "failed" | "signalled" => CheckState::Failed,
        "timed_out" => CheckState::TimedOut,
        "unavailable" | "outcome_unknown" | "launch_failed" => CheckState::Unavailable,
        _ => CheckState::NotRun,
    }
}

fn tail(text: &str, bytes: usize) -> String {
    if text.len() <= bytes {
        return text.to_string();
    }
    let mut start = text.len() - bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

fn head(text: &str, bytes: usize) -> String {
    if text.len() <= bytes {
        return text.to_string();
    }
    let mut end = bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn evidence_text(value: &crate::session_control_plane::EvidenceValue<String>) -> Option<String> {
    use crate::session_control_plane::EvidenceValue;
    match value {
        EvidenceValue::Available { value } | EvidenceValue::Truncated { value, .. } => {
            Some(value.clone())
        }
        _ => None,
    }
}

fn usage_of(value: &crate::session_control_plane::EvidenceValue<serde_json::Value>) -> RunUsage {
    use crate::session_control_plane::EvidenceValue;
    let EvidenceValue::Available { value } = value else {
        // Usage not recorded: neither its tokens nor its cost are known.
        return RunUsage {
            cost_known: false,
            ..RunUsage::default()
        };
    };
    let (stats, complete) = match value.get("kind").and_then(|kind| kind.as_str()) {
        Some("measured") => (value.get("usage"), true),
        _ => (value.get("known_subtotal"), false),
    };
    let number = |key: &str| {
        stats
            .and_then(|stats| stats.get(key))
            .and_then(|value| value.as_u64())
            .unwrap_or(0)
    };
    RunUsage {
        input_tokens: number("input_tokens"),
        output_tokens: number("output_tokens"),
        cost_microunits: 0,
        complete,
        // The projection carries no cost: a call whose usage is not known
        // has no known cost either.
        cost_known: complete,
        retries: 0,
    }
}

fn node_state(state: &str) -> NodeState {
    match state {
        "unstarted" => NodeState::NeverStarted,
        "running" => NodeState::Running,
        "accepted" => NodeState::Accepted,
        "failed" => NodeState::Failed,
        "interrupted" => NodeState::Stopped,
        "superseded" => NodeState::Superseded,
        _ => NodeState::Blocked,
    }
}

/// How the run driver sees the state of a turn's control-plane projection.
fn turn_state(state: &str) -> TurnState {
    match state {
        "completed" => TurnState::Completed,
        "needs_attention" => TurnState::NeedsAttention,
        "cancelled" | "finished" => TurnState::Stopped,
        "running" => TurnState::Running,
        _ => TurnState::Failed,
    }
}

/// How the Outcome names the model of a node whose retained definition has
/// `provider` and `model`, as [`crate::loadout::team_plan::model_identity`]
/// names a loadout's Agents. An external writer's retained definition names
/// its runtime as the provider (`claude-code`, `codex`); its identity is the
/// provider of the model API its program calls (`anthropic`, `openai`), the
/// program's model and that runtime.
fn node_identity(provider: String, model: String) -> ModelIdentity {
    match crate::external_agent::runtime_for_provider(&provider)
        .and_then(crate::external_agent::model_provider)
    {
        Some(model_provider) => ModelIdentity {
            provider: model_provider.into(),
            model,
            runtime: provider,
        },
        None => ModelIdentity {
            provider,
            model,
            runtime: "native".into(),
        },
    }
}

/// What the run driver observes of one turn's control-plane projection:
/// nodes and their generations (answers bounded to 64 KiB by the
/// projection), the required checks named by `checks`, the required review
/// with every round, and usage. `measured` is what each activation's
/// provider calls measured, by activation id
/// ([`AxocoatlDaemon::loadout_turn_provider_usage`]): it counts every
/// settled call, those of an activation that then failed included, where the
/// projection has only the usage attached to an accepted answer. An
/// activation it does not list keeps the projection's usage.
pub fn observation_from_control_plane(
    view: &crate::session_control_plane::SessionTurnControlPlane,
    checks: &[CheckLabel],
    reviewer_fallback: Option<&ModelIdentity>,
    measured: &HashMap<String, RunUsage>,
) -> TurnObservation {
    use crate::session_control_plane::{ControlPlaneActivationRef, EvidenceValue};
    let turn_stopped = turn_state(&view.state) == TurnState::Stopped;
    let helpers: HashMap<&str, &str> = view
        .edges
        .iter()
        .filter(|edge| edge.kind == "delegated_by")
        .map(|edge| {
            (
                edge.target.as_str(),
                match &edge.summary {
                    EvidenceValue::Available { value } => value.as_str(),
                    _ => "",
                },
            )
        })
        .collect();
    let mut usage = RunUsage {
        complete: true,
        cost_known: true,
        ..RunUsage::default()
    };
    let mut nodes = Vec::new();
    let mut reviewer_identity = None;
    for node in &view.nodes {
        let (role, provider, model) = match &node.definition {
            EvidenceValue::Available { value } => (
                match &value.role {
                    EvidenceValue::Available { value } => value.clone(),
                    _ => String::new(),
                },
                evidence_text(&value.provider).unwrap_or_default(),
                evidence_text(&value.model).unwrap_or_default(),
            ),
            _ => (String::new(), String::new(), String::new()),
        };
        let identity = node_identity(provider, model);
        let (kind, slot_id, required) = match helpers.get(node.node_id.as_str()) {
            Some(template) => (
                "helper",
                if template.is_empty() {
                    node.label.clone()
                } else {
                    (*template).to_string()
                },
                false,
            ),
            None if role == "Worker" => ("reviewer", node.label.clone(), false),
            None => ("slot", node.label.clone(), true),
        };
        if kind == "reviewer" {
            reviewer_identity = Some(identity.clone());
        }
        let mut generations = Vec::new();
        for (index, activation) in node.activations.iter().enumerate() {
            let generation = match &activation.generation {
                EvidenceValue::Available { value } => *value,
                _ => index as u32 + 1,
            };
            let state = node_state(&activation.state);
            let activation_usage = match &activation.reference {
                ControlPlaneActivationRef::Exact { activation } => {
                    measured.get(activation.activation_id.as_str()).cloned()
                }
                ControlPlaneActivationRef::Legacy { .. } => None,
            }
            .unwrap_or_else(|| usage_of(&activation.usage));
            usage.input_tokens = usage
                .input_tokens
                .saturating_add(activation_usage.input_tokens);
            usage.output_tokens = usage
                .output_tokens
                .saturating_add(activation_usage.output_tokens);
            if !matches!(state, NodeState::NeverStarted | NodeState::Running) {
                usage.complete &= activation_usage.complete;
                usage.cost_known &= activation_usage.cost_known;
            }
            // An empty recorded reason says nothing: it is no reason. Nor is
            // the activation's own partial answer. An activation that ends
            // without an accepted answer fails with its reserved output as
            // the evidence, which the projection shows as the reason: for an
            // error that output is `Activation failed: <error>`
            // (`session_dispatch_run.rs`), a real reason; for a stop it is
            // the answer as far as the model had written it, which says
            // nothing about why it ended.
            let reason = evidence_text(&activation.reason)
                .filter(|reason| !reason.trim().is_empty())
                .filter(|reason| {
                    reason.starts_with(ACTIVATION_FAILED)
                        || !activation
                            .partial_outputs
                            .iter()
                            .any(|output| output.text == *reason)
                });
            let failure = matches!(
                state,
                NodeState::Failed | NodeState::Stopped | NodeState::Blocked
            )
            .then(|| {
                // Without a reason of its own, a node of a turn that was
                // stopped ended because of the stop.
                let stopped = state == NodeState::Stopped || (reason.is_none() && turn_stopped);
                let message = reason
                    .clone()
                    .unwrap_or_else(|| format!("{} ended without a result", node.label));
                NodeFailure {
                    class: crate::loadout::driver::class_of(&message, stopped.then_some("stopped")),
                    message,
                }
            });
            generations.push(GenerationObservation {
                generation,
                state,
                answer: evidence_text(&activation.output).or_else(|| {
                    activation
                        .partial_outputs
                        .last()
                        .map(|output| output.text.clone())
                }),
                failure,
            });
        }
        nodes.push(NodeObservation {
            node_id: node.node_id.clone(),
            slot_id,
            model: identity,
            required,
            kind: kind.into(),
            generations,
        });
    }
    let checks_out: Vec<CheckResult> = view
        .required_checks
        .iter()
        .enumerate()
        .map(|(index, check)| {
            let label = checks
                .iter()
                .find(|label| label.argv == check.argv)
                .or_else(|| checks.get(index));
            CheckResult {
                name: label
                    .map(|label| label.name.clone())
                    .unwrap_or_else(|| format!("check-{}", index + 1)),
                argv: check.argv.clone(),
                state: check_state(check),
                timeout_ms: label.map_or(
                    axocoatl_session::check_options::DEFAULT_CHECK_TIMEOUT_MS,
                    |label| label.timeout_ms,
                ),
                exit_code: check.exit_code,
                stdout_tail: tail(&check.stdout, CHECK_TAIL_BYTES),
                stderr_tail: tail(&check.stderr, CHECK_TAIL_BYTES),
                candidate_sha256: check.candidate_sha256.clone(),
                report: None,
                reason: check.reason.clone(),
            }
        })
        .collect();
    let review = view.required_review.as_ref().map(|review| {
        // Findings are split by id from the whole proof text before the
        // text is bounded, so a long first finding never hides later ids.
        let rounds = view
            .review_rounds
            .iter()
            .map(|proof| {
                let mut round = proof.to_round();
                round.findings_text = head(&proof.findings, FINDINGS_TEXT_BYTES);
                round
            })
            .collect();
        ReviewOutcome {
            reviewer: reviewer_identity
                .clone()
                .or_else(|| reviewer_fallback.cloned())
                .unwrap_or(ModelIdentity {
                    provider: String::new(),
                    model: review.reviewer.clone(),
                    runtime: "native".into(),
                }),
            max_rounds: review.max_rounds,
            rounds,
            passed: review.state == "approved" && review.current,
            state: review.state.clone(),
            reason: review.reason.clone(),
        }
    });
    usage.retries = crate::provider_retry::run_events(view).len() as u32;
    TurnObservation {
        session_id: view.session_id.clone(),
        turn_id: view.turn_id.clone(),
        state: turn_state(&view.state),
        attention_reason: None,
        nodes,
        checks: checks_out,
        review,
        usage,
    }
}

/// `HEAD` and the paths with uncommitted changes of `repo`, from host git
/// with repository configuration that could run commands turned off. A
/// directory that is not a Git repository has neither.
async fn repository_state(repo: &std::path::Path) -> (Option<String>, Vec<String>) {
    fn git(repo: &std::path::Path, args: &[&str]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("git");
        command
            .kill_on_drop(true)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .arg("--no-optional-locks")
            .arg("-C")
            .arg(repo)
            .args(["-c", "core.fsmonitor=false"])
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(["-c", "credential.helper="])
            .args(["-c", "diff.external="])
            .args(args);
        command
    }
    let timeout = std::time::Duration::from_secs(60);
    let head = match tokio::time::timeout(
        timeout,
        git(repo, &["rev-parse", "--verify", "HEAD"]).output(),
    )
    .await
    {
        Ok(Ok(output)) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    };
    let dirty = match tokio::time::timeout(
        timeout,
        git(
            repo,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )
        .output(),
    )
    .await
    {
        Ok(Ok(output)) if output.status.success() => {
            let mut paths = Vec::new();
            let mut entries = output.stdout.split(|byte| *byte == 0).peekable();
            while let Some(entry) = entries.next() {
                if entry.len() < 4 {
                    continue;
                }
                let status = &entry[..2];
                paths.push(String::from_utf8_lossy(&entry[3..]).to_string());
                // A rename or copy carries its source as the next entry.
                if status.contains(&b'R') || status.contains(&b'C') {
                    if let Some(source) = entries.next() {
                        paths.push(String::from_utf8_lossy(source).to_string());
                    }
                }
            }
            paths.sort();
            paths.dedup();
            paths
        }
        _ => Vec::new(),
    };
    (head, dirty)
}

/// The URL a `ParamOr` names, with parameters (defaults applied) resolved.
fn param_url(
    value: &ParamOr<String>,
    params: &axocoatl_config::loadout::ParamValues,
) -> Option<String> {
    match value {
        ParamOr::Value(url) => Some(url.clone()),
        ParamOr::Param { param } => params.get(param).cloned(),
    }
}

/// The Session ports a qa run's browser reaches the build under test (and
/// the reference build) through: the port of each URL on `localhost` or a
/// loopback address. Any other host is left to
/// [`crate::loadout::qa::validate_qa_admission`], which checks it against
/// every kind of `browser.allow` entry (host, cidr, preset) in force now.
fn qa_exposed_ports(
    file: &LoadoutFile,
    params: &axocoatl_config::loadout::ParamValues,
) -> Result<Vec<u16>, DaemonError> {
    let Some(qa) = &file.qa else {
        return Ok(Vec::new());
    };
    let mut urls = vec![("qa.target_url", param_url(&qa.target_url, params))];
    if let Some(reference) = &qa.reference_url {
        urls.push(("qa.reference_url", param_url(reference, params)));
    }
    let mut ports = Vec::new();
    for (field, url) in urls {
        let Some(url) = url else {
            if field == "qa.target_url" {
                return Err(DaemonError::InvalidRequest(
                    "qa.target_url has no value".into(),
                ));
            }
            continue;
        };
        if let Some(port) = crate::loadout::qa::session_port(&url) {
            if !ports.contains(&port) {
                ports.push(port);
            }
        }
    }
    Ok(ports)
}

/// A loadout under `network: none` reaches no host, so none of its Agents
/// may list a tool that reaches one from the host (the web and browser
/// tools), whatever the daemon's own `sandbox.network` allows.
fn refuse_hosts_under_network_none(file: &LoadoutFile) -> Result<(), DaemonError> {
    if file.sandbox.network != "none" {
        return Ok(());
    }
    for agent in &file.agents {
        if let Some(tool) = agent.tools.iter().find(|tool| {
            matches!(
                tool.as_str(),
                "web_search" | "web_fetch" | "browser" | "browser_check"
            )
        }) {
            return Err(DaemonError::InvalidRequest(format!(
                "agents.{}.tools lists {tool}, which reaches hosts, but the loadout runs under \
                 network: none; run it under network: egress",
                agent.id
            )));
        }
    }
    Ok(())
}

fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

/// The Outcome of a run that ended before its driver ran: its environment
/// failed, or the daemon restarted during it.
fn ended_outcome(manifest: &RunManifest, error: String, warnings: Vec<RunWarning>) -> RunOutcome {
    let mut outcome = RunOutcome {
        schema: axocoatl_session::run_outcome::RUN_OUTCOME_SCHEMA.into(),
        run_id: manifest.run_id.clone(),
        session_id: manifest.session_id.clone(),
        workspace_id: manifest.workspace_id.clone(),
        loadout: manifest.loadout.clone(),
        task: manifest.task.clone(),
        started_at_ms: manifest.started_at_ms,
        finished_at_ms: now_ms(),
        verdict: RunVerdict::Error,
        exit_code: exit_code::INFRASTRUCTURE,
        attention: Vec::new(),
        turns: Vec::new(),
        checks: Vec::new(),
        review: None,
        adjudications: Vec::new(),
        findings: Vec::new(),
        not_covered: Vec::new(),
        warnings,
        usage: RunUsage::default(),
        network: NetworkSummary::default(),
        keep: None,
        error: Some(error),
    };
    outcome.decide(Default::default());
    outcome
}

/// Where a bundle write has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleCursor {
    Start,
    Turns { index: usize },
    History,
    Network { after: Option<u64> },
    RunEvents { after: Option<u64> },
    Done,
}

impl AxocoatlDaemon {
    fn user_loadout_dir(&self) -> Option<std::path::PathBuf> {
        let config = self
            .config_path
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()?;
        Some(config.parent()?.join(USER_LOADOUT_DIR))
    }

    fn loadout_registry(&self) -> Vec<RegistryEntry> {
        let mut entries: Vec<RegistryEntry> = builtin_loadouts()
            .into_iter()
            .map(|loadout| RegistryEntry {
                loadout,
                path: None,
            })
            .collect();
        if let Some(dir) = self.user_loadout_dir() {
            match load_user_loadouts(&dir) {
                Ok(users) => entries.extend(users.into_iter().map(|entry| RegistryEntry {
                    loadout: entry.loadout,
                    path: Some(entry.path),
                })),
                Err(error) => entries.push(RegistryEntry {
                    loadout: Err(LoadoutError::Parse {
                        source_name: dir.display().to_string(),
                        reason: format!("the loadout directory cannot be read: {error}"),
                    }),
                    path: Some(dir),
                }),
            }
        }
        entries
    }

    /// The usable loadout `id`, or why it cannot be used.
    fn find_loadout(&self, id: &str) -> Result<Loadout, DaemonError> {
        let mut found = None;
        for entry in self.loadout_registry() {
            match entry.loadout {
                Ok(loadout) if loadout.file.id == id => return Ok(loadout),
                Ok(_) => {}
                Err(error) => {
                    let named = entry
                        .path
                        .as_ref()
                        .and_then(|path| path.file_stem())
                        .and_then(|stem| stem.to_str())
                        == Some(id);
                    if named {
                        found = Some(error);
                    }
                }
            }
        }
        match found {
            Some(error) => Err(DaemonError::InvalidRequest(format!(
                "loadout {id} cannot be used: {error}"
            ))),
            None => Err(DaemonError::NotFound(format!(
                "no loadout named {id:?}; `axocoatl loadouts list` shows them"
            ))),
        }
    }

    /// `GET /api/loadouts`: built-in loadouts, then user loadouts from
    /// `<config dir>/loadouts/` (invalid ones listed with their error).
    pub async fn list_loadouts(&self) -> Result<Vec<LoadoutSummary>, DaemonError> {
        Ok(self
            .loadout_registry()
            .into_iter()
            .map(|entry| match (&entry.loadout, &entry.path) {
                (Ok(loadout), _) => loadout_summary(loadout),
                (Err(error), Some(path)) => invalid_summary(path, error),
                (Err(error), None) => invalid_summary(std::path::Path::new("built-in"), error),
            })
            .collect())
    }

    /// `GET /api/loadouts/{id}`.
    pub async fn loadout_view(&self, id: &str) -> Result<LoadoutView, DaemonError> {
        let loadout = self.find_loadout(id)?;
        Ok(LoadoutView {
            summary: loadout_summary(&loadout),
            graph: loadout_graph(&loadout.file),
            file: loadout.file.clone(),
            text: loadout.text.clone(),
        })
    }

    /// `POST /api/loadouts/validate`.
    pub async fn validate_loadout_text(
        &self,
        request: ValidateLoadoutRequest,
    ) -> Result<ValidateLoadoutResponse, DaemonError> {
        Ok(validate_text(&request.text))
    }

    /// Admission of one run. See [`Self::admit_loadout_run`].
    async fn admit_loadout_run_inner(
        &self,
        request: &RunRequest,
    ) -> Result<(RunAccepted, RunContext), DaemonError> {
        let loadout = self.find_loadout(&request.loadout)?;
        if request.task.trim().is_empty() {
            return Err(DaemonError::InvalidRequest("the task is empty".into()));
        }
        if request.task.len() > MAX_RUN_TASK_BYTES {
            return Err(DaemonError::InvalidRequest(format!(
                "the task is longer than {MAX_RUN_TASK_BYTES} bytes"
            )));
        }
        let repo = std::fs::canonicalize(&request.repo).map_err(|error| {
            DaemonError::InvalidRequest(format!("repository {}: {error}", request.repo))
        })?;
        if !repo.is_dir() {
            return Err(DaemonError::InvalidRequest(format!(
                "repository {} is not a directory",
                repo.display()
            )));
        }
        let repo_text = repo.display().to_string();
        let mut resolved = resolve_loadout(&loadout, &request.params, &request.task, &repo_text)
            .map_err(|error| DaemonError::InvalidRequest(error.to_string()))?;
        crate::loadout::team_plan::fill_detected_checks(
            &mut resolved,
            request.check_command.as_deref(),
            axocoatl_session::detect_check_command(&repo).as_deref(),
        )
        .map_err(run_error)?;
        // A loadout Session runs only where the host can isolate it:
        // local rootless Podman, never a fallback.
        if self.config.sandbox.backend == "e2b" {
            return Err(DaemonError::Session(LOADOUT_NEEDS_LOCAL_PODMAN.into()));
        }
        let rootless = axocoatl_isolation::session_sandbox::podman_rootless()
            .await
            .map_err(|error| {
                DaemonError::Session(format!(
                    "a loadout Session needs rootless Podman, and Podman could not be asked: {error}"
                ))
            })?;
        if !rootless {
            return Err(DaemonError::Session(
                "a loadout Session runs its commands as non-root workload users, which needs \
                 rootless Podman; this Podman runs as root"
                    .into(),
            ));
        }
        let overlay = crate::loadout::egress::loadout_overlay(
            &self.network_policy.current(),
            &resolved,
            self.data_root.path(),
        )
        .map_err(run_error)?;
        let environment = loadout.file.environment.clone().unwrap_or_default();
        let image = match (&environment.image, environment.recipes.is_empty()) {
            (Some(image), _) => Some(image.clone()),
            // A recipe image runs by the exact id `axocoatl recipe build`
            // recorded, which is also what makes it trusted for Sessions.
            (None, false) => Some(self.recipe_image(&environment.recipes)?.image_id),
            (None, true) => None,
        };
        let setup = match (&request.setup_command, &environment.setup) {
            (Some(command), _) => Some(command.clone()),
            (None, Some(ParamOr::Value(command))) => Some(command.clone()),
            (None, Some(ParamOr::Param { param })) => resolved.params.get(param).cloned(),
            (None, None) => None,
        }
        .map(|command| command.trim().to_string())
        .filter(|command| !command.is_empty());
        refuse_hosts_under_network_none(&loadout.file)?;
        let exposed_ports = qa_exposed_ports(&loadout.file, &resolved.params)?;
        if loadout.file.kind == axocoatl_config::loadout::LoadoutKind::Qa {
            // The lists in force now, so `axocoatl network reload` applies
            // to the next run (the browser: block itself needs a restart).
            let policy = self.network_policy.current();
            let browser = policy
                .browser
                .as_ref()
                .map(|(allow, private)| (allow.as_slice(), private.as_slice()));
            let settings =
                crate::loadout::qa::validate_qa_admission(&resolved, &exposed_ports, browser)
                    .map_err(run_error)?;
            crate::loadout::qa::check_repro_dir(&repo, &settings).map_err(run_error)?;
        }
        // An e2e check's agent needs tool calls and image input: refused
        // when OpenRouter's catalog says its model has neither, a warning
        // when the provider's catalog cannot say.
        let e2e_warnings = crate::loadout::e2e::verify_e2e_models(
            &resolved,
            &crate::loadout::e2e::OpenRouterCatalog::default(),
        )
        .await
        .map_err(run_error)?;
        for warning in e2e_warnings {
            if !resolved
                .warnings
                .iter()
                .any(|known| known.code == warning.code && known.message == warning.message)
            {
                resolved
                    .warnings
                    .push(axocoatl_config::loadout::LoadoutWarning {
                        code: warning.code,
                        field: "checks".into(),
                        message: warning.message,
                    });
            }
        }
        // A tokens budget that cannot hold one model call is refused now,
        // as a usage error naming the budget and the minimum, not after the
        // run's Session started.
        let contexts = self.loadout_model_contexts(&resolved).await;
        crate::loadout::team_plan::refuse_budgets_below_one_call(&resolved, |model| {
            contexts.get(model).copied().flatten()
        })
        .map_err(run_error)?;
        let workspace = self.create_workspace(&repo_text, None).await?;
        let run_id = format!("run-{}", uuid::Uuid::new_v4());
        let loadout_ref = LoadoutRef {
            id: loadout.file.id.clone(),
            version: loadout.file.version,
            kind: loadout.file.kind.to_string(),
            digest: loadout.digest.clone(),
            builtin: loadout.source == LoadoutSource::Builtin,
        };
        let binding = SessionLoadoutBinding {
            run_id: run_id.clone(),
            loadout: loadout_ref.clone(),
            network: loadout.file.sandbox.network.clone(),
            workload: "hardened".into(),
        };
        let task_line: String = request
            .task
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();
        let name = format!("{} · {}", loadout.file.id, task_line.trim());
        let started_at_ms = now_ms();
        if crate::loadout::e2e::workspace_mounts(&resolved).read_only_e2e_cache {
            self.loadout_runs
                .e2e_runs
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(run_id.clone());
        }
        let session = self
            .create_loadout_session(
                &workspace.id,
                &name,
                image,
                setup,
                exposed_ports,
                binding,
                overlay,
            )
            .await?;
        let (repo_head, dirty_paths) = repository_state(&repo).await;
        let options = RunOptions {
            task: request.task.clone(),
            repo: repo.clone(),
            params: request.params.clone(),
            keep: request.keep,
            check_command: request.check_command.clone(),
            setup_command: request.setup_command.clone(),
        };
        let manifest = RunManifest {
            schema: RUN_MANIFEST_SCHEMA.into(),
            run_id: run_id.clone(),
            session_id: session.id.clone(),
            workspace_id: workspace.id.clone(),
            loadout: loadout_ref,
            loadout_text: loadout.text.clone(),
            params: resolved.params.clone(),
            task: request.task.clone(),
            repo: repo_text,
            repo_head,
            dirty_paths,
            started_at_ms,
            options: serde_json::json!({
                "request_id": request.request_id,
                "keep": request.keep,
                "check_command": request.check_command,
                "setup_command": request.setup_command,
                "axocoatl_version": env!("CARGO_PKG_VERSION"),
            }),
        };
        let store = self.loadout_runs.store()?;
        store.create(&manifest).map_err(record_error)?;
        let warnings: Vec<RunWarning> = resolved
            .warnings
            .iter()
            .map(|warning| RunWarning {
                code: warning.code.clone(),
                message: warning.message.clone(),
            })
            .collect();
        for warning in &warnings {
            store
                .append(
                    &run_id,
                    &RunEvent::Warning {
                        at_ms: now_ms(),
                        warning: warning.clone(),
                    },
                )
                .map_err(record_error)?;
        }
        let ready = session.environment.state == SessionEnvironmentState::Ready;
        store
            .append(
                &run_id,
                &RunEvent::Phase {
                    at_ms: now_ms(),
                    phase: "preparing".into(),
                    detail: if ready {
                        "the Session's environment is ready".into()
                    } else {
                        format!(
                            "the Session's environment is {:?}",
                            session.environment.state
                        )
                    },
                },
            )
            .map_err(record_error)?;
        if !ready {
            // The environment failed (or still needs a person): the run ends
            // here with what the setup printed.
            let mut detail = session
                .environment
                .error
                .clone()
                .unwrap_or_else(|| "the Session's environment is not ready".into());
            for result in &session.environment.setup_results {
                detail.push_str(&format!(
                    "\n$ {} (exit {})\n{}{}",
                    result.command, result.exit_code, result.stdout, result.stderr
                ));
            }
            let outcome = ended_outcome(&manifest, head(&detail, 16 * 1024), warnings.clone());
            let _ = store.append(
                &run_id,
                &RunEvent::Ended {
                    at_ms: now_ms(),
                    outcome: Box::new(outcome.clone()),
                },
            );
            store.finish(&run_id, &outcome).map_err(record_error)?;
        }
        let wall_clock = crate::loadout::team_plan::wall_clock_ms(&resolved).map_err(run_error)?;
        let context = RunContext {
            run_id: run_id.clone(),
            session_id: session.id.clone(),
            workspace_id: workspace.id.clone(),
            resolved,
            options,
            deadline: std::time::Instant::now() + std::time::Duration::from_millis(wall_clock),
        };
        Ok((
            RunAccepted {
                run_id,
                session_id: session.id,
                workspace_id: workspace.id,
                warnings,
            },
            context,
        ))
    }

    /// `POST /api/runs`: resolve the loadout, authorize the repository as a
    /// Workspace, create the Session bound to the loadout, approve exactly
    /// the loadout's environment, write the run manifest. The server then
    /// starts the driver task. A repeat of the same `request_id` returns the
    /// same run; the same id with another request is refused.
    pub async fn admit_loadout_run(
        &self,
        request: RunRequest,
    ) -> Result<(RunAccepted, crate::loadout::RunContext), DaemonError> {
        if !valid_request_id(&request.request_id) {
            return Err(DaemonError::InvalidRequest(
                "request_id is 1-128 letters, digits, '-', '_', '.' or ':'".into(),
            ));
        }
        let digest = {
            use sha2::{Digest, Sha256};
            format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&request).unwrap_or_default())
            )
        };
        {
            let admitted = self
                .loadout_runs
                .admitted
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if let Some((known, accepted, context)) = admitted.get(&request.request_id) {
                if *known != digest {
                    return Err(DaemonError::SessionConflict(format!(
                        "request {} already started run {} with another request",
                        request.request_id, accepted.run_id
                    )));
                }
                return Ok((accepted.clone(), context.clone()));
            }
            let mut admitting = self
                .loadout_runs
                .admitting
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if !admitting.insert(request.request_id.clone()) {
                return Err(DaemonError::SessionConflict(format!(
                    "request {} is being admitted",
                    request.request_id
                )));
            }
        }
        let result = self.admit_loadout_run_inner(&request).await;
        self.loadout_runs
            .admitting
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&request.request_id);
        let (accepted, context) = result?;
        self.loadout_runs
            .admitted
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(
                request.request_id.clone(),
                (digest, accepted.clone(), context.clone()),
            );
        Ok((accepted, context))
    }

    /// The context each native Ollama model of the run is loaded with,
    /// observed as its first call observes it (a load without a prompt, no
    /// inference), within [`ADMISSION_CONTEXT_TIMEOUT`]. A model that cannot
    /// be observed now maps to `None`; its run reports the provider's own
    /// error. Other providers' models are not observed before the run.
    async fn loadout_model_contexts(
        &self,
        resolved: &axocoatl_config::loadout::ResolvedLoadout,
    ) -> HashMap<axocoatl_config::loadout::ModelSpec, Option<u64>> {
        let mut contexts = HashMap::new();
        let Some(base_url) = self
            .config
            .providers
            .ollama
            .as_ref()
            .map(|provider| provider.base_url.clone())
        else {
            return contexts;
        };
        for (_, model) in crate::loadout::team_plan::resolved_call_budgets(resolved) {
            let Some(model) = model.filter(|model| model.provider == "ollama") else {
                continue;
            };
            if contexts.contains_key(&model) {
                continue;
            }
            let observed = tokio::time::timeout(
                ADMISSION_CONTEXT_TIMEOUT,
                axocoatl_llm_ollama::observe_native_ollama_context(&base_url, &model.model),
            )
            .await;
            let context = match observed {
                Ok(Ok(observation)) => Some(observation.context_tokens as u64),
                Ok(Err(error)) => {
                    tracing::debug!(model = %model, %error, "admission could not observe the model's context");
                    None
                }
                Err(_) => None,
            };
            contexts.insert(model, context);
        }
        contexts
    }

    /// Create the native Session of a loadout run in `workspace_id`, bind it
    /// to the run before its environment is prepared, and prepare that
    /// environment with exactly `setup` approved (no detected command is
    /// ever approved by a run). Returns the Session as prepared: Ready, or
    /// Failed with the setup's output.
    #[allow(clippy::too_many_arguments)]
    async fn create_loadout_session(
        &self,
        workspace_id: &str,
        name: &str,
        image: Option<String>,
        setup: Option<String>,
        exposed_ports: Vec<u16>,
        binding: SessionLoadoutBinding,
        overlay: crate::loadout::egress::LoadoutEgressOverlay,
    ) -> Result<Session, DaemonError> {
        let axocoatl_session::execution_ownership::DataRootFormatOwnership::Upgraded(ownership) =
            &self._data_dir_lease.ownership
        else {
            return Err(DaemonError::Session(LEGACY_ROOT_REFUSAL.into()));
        };
        let workspace = self
            .get_workspace(workspace_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("workspace '{workspace_id}' not found")))?;
        // Never wait for the Workspace: its owner can be a turn that needs a
        // person, which holds it until someone continues, stops or closes
        // it. A run refuses at once and names who holds it.
        let operation = self.attempt_operation_for_workspace(workspace_id).await;
        let _operation = match operation.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                return Err(self
                    .workspace_in_use(workspace_id, &workspace.canonical_path)
                    .await)
            }
        };
        // Named while it holds the Workspace (its Session's environment is
        // prepared under it), for other requests' busy refusals.
        let _named = self.workspace_operation_labels.name(
            &super::workspace_attempt_operation_key(workspace_id),
            format!("the admission of loadout run {}", binding.run_id),
        );
        if let Some((owner, set_id)) = self
            .unresolved_attempt_owner_for_workspace_id(workspace_id)
            .await?
        {
            return Err(DaemonError::AttemptConflict(format!(
                "attempt set '{set_id}' in session '{owner}' owns this Workspace; keep or discard it before a loadout run"
            )));
        }
        // No configured Agent runs in a loadout Session: the run applies the
        // loadout's own team before the first turn.
        let mode = SessionMode::Custom { agents: Vec::new() };
        let setup_approved = setup.is_some();
        let session = {
            let mut sessions = self.session_store.lock().await;
            let (session, receipt) = sessions
                .create_native_with_environment(
                    ownership,
                    name,
                    workspace_id,
                    &workspace.canonical_path,
                    mode,
                    Vec::new(),
                    exposed_ports,
                    image,
                    setup,
                    setup_approved,
                    true,
                )
                .map_err(|error| DaemonError::Session(error.to_string()))?;
            self.session_dispatch_lifecycles
                .retain_native_session(ownership.clone(), receipt)?;
            sessions
                .bind_loadout(&session.id, binding)
                .map_err(|error| DaemonError::Session(error.to_string()))?
        };
        self.egress_points.set_loadout_overlay(&session.id, overlay);
        match self.prepare_new_session_environment(session.clone()).await {
            Ok(session) => Ok(session),
            Err(error) => {
                // A failed preparation is recorded on the Session; return it
                // as it stands so the run reports the setup's output.
                let current = self.get_session(&session.id).await;
                match current {
                    Some(current)
                        if current.environment.state == SessionEnvironmentState::Failed =>
                    {
                        Ok(current)
                    }
                    _ => Err(error),
                }
            }
        }
    }

    /// The refusal a run gets when another operation holds its Workspace
    /// (exit code 7): the open Sessions of the Workspace whose turn is
    /// running or needs a person (each such turn holds the Workspace until
    /// it ends), or, when none has one, every open Session of the Workspace.
    async fn workspace_in_use(&self, workspace_id: &str, path: &std::path::Path) -> DaemonError {
        let holders = self.workspace_holders(workspace_id, None).await;
        let label = self
            .workspace_operation_labels
            .get(&super::workspace_attempt_operation_key(workspace_id));
        let held_by = match label {
            Some(label) if holders.turns.is_empty() => {
                format!("is held by another operation: {label}")
            }
            _ => holders.held_by(),
        };
        DaemonError::WorkspaceBusy(format!(
            "the Workspace {} {held_by}. A loadout run needs the Workspace to itself: let that \
             turn finish, or stop it or close its Session, then run again",
            path.display()
        ))
    }

    /// Whether the loadout that created `session` has an e2e check, so its
    /// container mounts `.e2e/cache` read-only (workstream e2e passes this to
    /// the Session container's policy).
    pub fn session_has_e2e_check(&self, session: &Session) -> bool {
        let Some(binding) = &session.loadout else {
            return false;
        };
        if self
            .loadout_runs
            .e2e_runs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains(&binding.run_id)
        {
            return true;
        }
        self.loadout_runs
            .store()
            .ok()
            .and_then(|store| store.manifest(&binding.run_id).ok())
            .and_then(|manifest| parse_loadout(&manifest.loadout_text, LoadoutSource::Builtin).ok())
            .is_some_and(|loadout| {
                loadout
                    .file
                    .checks
                    .iter()
                    .any(|check| check.run.e2e.is_some())
            })
    }

    /// Whether this call is the first to start a driver for `run_id`. A run
    /// that has ended (its environment failed at admission) has none.
    pub fn claim_loadout_run_driver(&self, run_id: &str) -> bool {
        let finished = self
            .loadout_runs
            .store()
            .and_then(|store| store.is_finished(run_id).map_err(record_error))
            .unwrap_or(true);
        !finished
            && self
                .loadout_runs
                .drivers
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(run_id.to_string())
    }

    /// Record that a person asked to stop `run_id`, and stop its Session's
    /// active turn.
    pub async fn request_loadout_run_stop(&self, run_id: &str) -> Result<(), DaemonError> {
        let store = self.loadout_runs.store()?;
        let manifest = store.manifest(run_id).map_err(record_error)?;
        if store.is_finished(run_id).map_err(record_error)? {
            return Ok(());
        }
        self.loadout_runs
            .stops
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(run_id.to_string());
        let _ = store.append(
            run_id,
            &RunEvent::Phase {
                at_ms: now_ms(),
                phase: "stopping".into(),
                detail: "a person asked to stop the run".into(),
            },
        );
        if let Some(active) = self.active_session_turn(&manifest.session_id).await? {
            self.stop_session_turn(&manifest.session_id, &active.turn_id)
                .await?;
        }
        Ok(())
    }

    /// Whether a person asked to stop `run_id`.
    pub fn loadout_run_stop_requested(&self, run_id: &str) -> bool {
        self.loadout_runs
            .stops
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains(run_id)
    }

    /// Append one event to `run_id`'s record.
    pub fn record_loadout_run_event(
        &self,
        run_id: &str,
        event: &RunEvent,
    ) -> Result<u64, DaemonError> {
        self.loadout_runs
            .store()?
            .append(run_id, event)
            .map_err(record_error)
    }

    /// Every event recorded for `run_id`.
    pub fn loadout_run_recorded_events(&self, run_id: &str) -> Result<Vec<RunEvent>, DaemonError> {
        Ok(self
            .loadout_runs
            .store()?
            .events(run_id, None, axocoatl_session::run_record::MAX_RUN_EVENTS)
            .map_err(record_error)?
            .into_iter()
            .map(|(_, event)| event)
            .collect())
    }

    /// Write `run_id`'s Outcome once.
    pub fn finish_loadout_run(
        &self,
        run_id: &str,
        outcome: &RunOutcome,
    ) -> Result<(), DaemonError> {
        self.loadout_runs
            .store()?
            .finish(run_id, outcome)
            .map_err(record_error)
    }

    fn run_status(&self, run_id: &str) -> Result<RunStatusView, DaemonError> {
        let store = self.loadout_runs.store()?;
        let manifest = store.manifest(run_id).map_err(record_error)?;
        let outcome = store.outcome(run_id).map_err(record_error)?;
        let events = store
            .events(run_id, None, axocoatl_session::run_record::MAX_RUN_EVENTS)
            .map_err(record_error)?;
        let phase = events
            .iter()
            .rev()
            .find_map(|(_, event)| match event {
                RunEvent::Phase { phase, .. } if phase == crate::keep_pr::KEEP_PHASE => None,
                RunEvent::Phase { phase, detail, .. } => Some(if detail.is_empty() {
                    phase.clone()
                } else {
                    format!("{phase}: {detail}")
                }),
                _ => None,
            })
            .unwrap_or_default();
        let started = events
            .iter()
            .any(|(_, event)| matches!(event, RunEvent::TurnStarted { .. }));
        let state = match &outcome {
            Some(outcome) if outcome.verdict == RunVerdict::Error => "failed",
            Some(_) => "finished",
            None if started => "running",
            None => "preparing",
        };
        Ok(RunStatusView {
            run_id: manifest.run_id,
            session_id: manifest.session_id,
            loadout: format!("{}@{}", manifest.loadout.id, manifest.loadout.version),
            state: state.into(),
            phase,
            started_at_ms: manifest.started_at_ms,
            outcome,
            keep: events
                .iter()
                .rev()
                .find_map(|(_, event)| crate::keep_pr::keep_result_of(event)),
        })
    }

    /// `GET /api/runs`.
    pub async fn list_loadout_runs(&self) -> Result<Vec<RunStatusView>, DaemonError> {
        let store = self.loadout_runs.store()?;
        let mut out = Vec::new();
        for run_id in store.list().map_err(record_error)? {
            match self.run_status(&run_id) {
                Ok(status) => out.push(status),
                Err(error) => {
                    tracing::warn!(run = %run_id, %error, "a loadout run record cannot be read")
                }
            }
        }
        Ok(out)
    }

    /// `GET /api/runs/{run_id}`.
    pub async fn loadout_run(&self, run_id: &str) -> Result<RunStatusView, DaemonError> {
        self.run_status(run_id)
    }

    /// `GET /api/runs/{run_id}/events`.
    pub async fn loadout_run_events(
        &self,
        run_id: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Result<RunEventsPage, DaemonError> {
        let limit = limit.clamp(1, 1000);
        let store = self.loadout_runs.store()?;
        // Read whether the run has ended before its events: the Outcome is
        // written after the last event, so a page that says finished holds
        // every event up to the end (or the next page does, when `limit`
        // cut it).
        let ended = store.is_finished(run_id).map_err(record_error)?;
        let events = store.events(run_id, after, limit).map_err(record_error)?;
        let next_after = events.last().map(|(seq, _)| *seq).or(after);
        let finished = ended && events.len() < limit;
        Ok(RunEventsPage {
            events,
            next_after,
            finished,
        })
    }

    /// `GET /api/runs/{run_id}/junit`.
    pub async fn loadout_run_junit(&self, run_id: &str) -> Result<String, DaemonError> {
        let store = self.loadout_runs.store()?;
        let manifest = store.manifest(run_id).map_err(record_error)?;
        let outcome = store
            .outcome(run_id)
            .map_err(record_error)?
            .ok_or_else(|| {
                DaemonError::SessionConflict(format!("run {run_id} has not finished"))
            })?;
        let fails = parse_loadout(&manifest.loadout_text, LoadoutSource::Builtin)
            .map(|loadout| fail_on_findings(&loadout.file))
            .unwrap_or(false);
        axocoatl_session::run_junit::render_junit_with(&outcome, fails)
            .map_err(|error| DaemonError::Session(error.to_string()))
    }

    /// The header of `run_id`'s record bundle. Its `created_at_ms` is when
    /// the run finished (the Outcome's `finished_at_ms`), never the time of
    /// the download, so every download of a finished run whose record and
    /// Session did not change in between is the same file, byte for byte:
    /// `axocoatl run --record`, `GET /api/runs/{run_id}/record` and the Run
    /// outcome panel's download. A run that has not finished has no such
    /// time yet; its bundle, a snapshot of a record still growing, carries
    /// the time it was read.
    pub fn record_bundle_header(&self, run_id: &str) -> Result<BundleHeader, DaemonError> {
        let store = self.loadout_runs.store()?;
        let manifest = store.manifest(run_id).map_err(record_error)?;
        let finished_at_ms = store
            .outcome(run_id)
            .map_err(record_error)?
            .map(|outcome| outcome.finished_at_ms);
        Ok(BundleHeader {
            schema: RECORD_BUNDLE_SCHEMA.into(),
            run_id: manifest.run_id,
            session_id: manifest.session_id,
            created_at_ms: finished_at_ms.unwrap_or_else(now_ms),
            axocoatl_version: env!("CARGO_PKG_VERSION").into(),
        })
    }

    /// The turns `run_id` started, in order.
    fn run_turn_ids(&self, run_id: &str) -> Result<Vec<String>, DaemonError> {
        Ok(self
            .loadout_run_recorded_events(run_id)?
            .into_iter()
            .filter_map(|event| match event {
                RunEvent::TurnStarted { turn_id, .. } => Some(turn_id),
                _ => None,
            })
            .collect())
    }

    /// The next sections of `run_id`'s record bundle after `cursor`, in
    /// `BUNDLE_SECTIONS` order, and where to continue. Each step reads one
    /// bounded part, so a caller can release the daemon between steps.
    pub async fn record_bundle_step(
        &self,
        run_id: &str,
        cursor: BundleCursor,
    ) -> Result<(Vec<(&'static str, serde_json::Value)>, BundleCursor), DaemonError> {
        let store = self.loadout_runs.store()?;
        let manifest = store.manifest(run_id).map_err(record_error)?;
        let value = |result: Result<serde_json::Value, serde_json::Error>| {
            result.map_err(|error| DaemonError::Session(format!("record bundle: {error}")))
        };
        match cursor {
            BundleCursor::Start => {
                let outcome = store.outcome(run_id).map_err(record_error)?;
                let session = self.get_session(&manifest.session_id).await;
                // The team as applied: each slot's reset_history, inline
                // definition and tools, not the choices the team view offers.
                let team = match self.session_team_record(&manifest.session_id).await {
                    Ok(team) => team,
                    Err(error) => serde_json::json!({ "unavailable": error.to_string() }),
                };
                Ok((
                    vec![
                        ("manifest", value(serde_json::to_value(&manifest))?),
                        (
                            "loadout",
                            serde_json::json!({
                                "text": manifest.loadout_text,
                                "digest": manifest.loadout.digest,
                            }),
                        ),
                        ("outcome", value(serde_json::to_value(&outcome))?),
                        ("session", value(serde_json::to_value(&session))?),
                        ("team", team),
                    ],
                    BundleCursor::Turns { index: 0 },
                ))
            }
            BundleCursor::Turns { index } => {
                let turns = self.run_turn_ids(run_id)?;
                let Some(turn_id) = turns.get(index) else {
                    return Ok((Vec::new(), BundleCursor::History));
                };
                let projection = match self
                    .session_turn_control_plane(&manifest.session_id, turn_id)
                    .await
                {
                    Ok(view) => value(serde_json::to_value(view))?,
                    Err(error) => serde_json::json!({
                        "turn_id": turn_id,
                        "unavailable": error.to_string(),
                    }),
                };
                Ok((
                    vec![("turn", projection)],
                    BundleCursor::Turns { index: index + 1 },
                ))
            }
            BundleCursor::History => {
                let history = match self.loadout_session_export(&manifest.session_id).await {
                    Ok(text) => {
                        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
                    }
                    Err(error) => serde_json::json!({ "unavailable": error.to_string() }),
                };
                Ok((
                    vec![("history", history)],
                    BundleCursor::Network { after: None },
                ))
            }
            BundleCursor::Network { after } => {
                let page = self
                    .session_network_records
                    .read_after(&manifest.session_id, after, NETWORK_PAGE)
                    .await;
                let page = match page {
                    Ok(page) => page,
                    // A Session without a network record has no events.
                    Err(_) => return Ok((Vec::new(), BundleCursor::RunEvents { after: None })),
                };
                if page.events.is_empty() {
                    return Ok((Vec::new(), BundleCursor::RunEvents { after: None }));
                }
                let next = page.events.last().map(|line| line.seq);
                let sections = page
                    .events
                    .into_iter()
                    .map(|line| serde_json::to_value(line).map(|line| ("network", line)))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| DaemonError::Session(format!("record bundle: {error}")))?;
                Ok((sections, BundleCursor::Network { after: next }))
            }
            BundleCursor::RunEvents { after } => {
                let events = store
                    .events(run_id, after, RUN_EVENT_PAGE)
                    .map_err(record_error)?;
                if events.is_empty() {
                    return Ok((Vec::new(), BundleCursor::Done));
                }
                let next = events.last().map(|(seq, _)| *seq);
                let sections = events
                    .into_iter()
                    .map(|(seq, event)| {
                        (
                            "run_event",
                            serde_json::json!({ "seq": seq, "event": event }),
                        )
                    })
                    .collect();
                Ok((sections, BundleCursor::RunEvents { after: next }))
            }
            BundleCursor::Done => Ok((Vec::new(), BundleCursor::Done)),
        }
    }

    /// The Session export of a record bundle's `history` section: the
    /// versioned export when the Session's History holds native execution
    /// (every loadout Session's turns are native, which the legacy rows
    /// cannot represent), the legacy export otherwise.
    async fn loadout_session_export(&self, session_id: &str) -> Result<String, DaemonError> {
        if self.session_history_is_versioned(session_id).await {
            self.export_versioned_session_json(session_id).await
        } else {
            self.export_session_json(session_id).await
        }
    }

    /// `GET /api/runs/{run_id}/record`: the record bundle, written to `out`.
    pub async fn write_record_bundle(
        &self,
        run_id: &str,
        out: &mut (dyn std::io::Write + Send),
    ) -> Result<(), DaemonError> {
        let header = self.record_bundle_header(run_id)?;
        let bundle_error = |error: axocoatl_session::record_bundle::BundleError| {
            DaemonError::Session(error.to_string())
        };
        let mut writer = axocoatl_session::record_bundle::BundleWriter::new(out, &header)
            .map_err(bundle_error)?;
        let mut cursor = BundleCursor::Start;
        while cursor != BundleCursor::Done {
            let (sections, next) = self.record_bundle_step(run_id, cursor).await?;
            for (section, data) in sections {
                writer.section(section, &data).map_err(bundle_error)?;
            }
            cursor = next;
        }
        writer.finish().map_err(bundle_error)?;
        Ok(())
    }

    /// What `session_id`'s network record holds: events, allowed and refused
    /// connections, and route requests per host.
    pub async fn loadout_network_summary(&self, session_id: &str) -> NetworkSummary {
        use axocoatl_session::network_record::{Decision, NetworkEvent};
        let mut summary = NetworkSummary::default();
        let mut routes: std::collections::BTreeMap<String, u64> = Default::default();
        let mut after = None;
        loop {
            let Ok(page) = self
                .session_network_records
                .read_after(session_id, after, NETWORK_PAGE)
                .await
            else {
                break;
            };
            if page.events.is_empty() {
                break;
            }
            after = page.events.last().map(|line| line.seq);
            for line in page.events {
                summary.events += 1;
                match line.event {
                    NetworkEvent::Open { decision, .. } => match decision {
                        Decision::Allow => summary.allowed_connections += 1,
                        Decision::Deny => summary.refused_connections += 1,
                    },
                    NetworkEvent::Request {
                        host,
                        decision: Decision::Allow,
                        ..
                    } => {
                        summary.route_requests += 1;
                        *routes.entry(host).or_default() += 1;
                    }
                    _ => {}
                }
            }
        }
        summary.routes = routes.into_iter().collect();
        summary
    }

    /// One turn of a loadout run as the driver observes it, or `None` when
    /// the turn has no projection yet.
    pub async fn loadout_turn_observation(
        &self,
        session_id: &str,
        turn_id: &str,
        checks: &[CheckLabel],
        reviewer: Option<&ModelIdentity>,
    ) -> Result<Option<TurnObservation>, DaemonError> {
        Ok(self
            .loadout_turn_observation_with_retries(session_id, turn_id, checks, reviewer)
            .await?
            .map(|(observation, _)| observation))
    }

    /// [`Self::loadout_turn_observation`] with the turn's provider retries
    /// as run events (`provider_retry::run_events`), for the run record.
    pub async fn loadout_turn_observation_with_retries(
        &self,
        session_id: &str,
        turn_id: &str,
        checks: &[CheckLabel],
        reviewer: Option<&ModelIdentity>,
    ) -> Result<Option<(TurnObservation, Vec<RunEvent>)>, DaemonError> {
        let Some(view) = self.session_turn_control_plane(session_id, turn_id).await? else {
            return Ok(None);
        };
        let measured = self.loadout_turn_provider_usage(session_id, turn_id);
        let mut observation = observation_from_control_plane(&view, checks, reviewer, &measured);
        // Cost is charged to grants, one per call: the turn's cost is what
        // its grants were charged (settled calls, and the reservations of
        // calls still running or whose cost is not known, such as a Codex
        // writer's, which `cost_known` then says). Without the grants the
        // cost is not known, and the usage is a known subtotal.
        match self.session_control_grants(session_id, turn_id).await {
            Ok(grants) => {
                observation.usage.cost_microunits = grants
                    .grants
                    .iter()
                    .map(|grant| grant.usage.cost_microunits)
                    .fold(0u64, u64::saturating_add);
            }
            Err(_) => {
                observation.usage.complete = false;
                observation.usage.cost_known = false;
            }
        }
        Ok(Some((
            observation,
            crate::provider_retry::run_events(&view),
        )))
    }

    /// What each started activation of `turn_id` measured over its provider
    /// calls, by activation id, from the turn's control authority: the held
    /// authority of the Session's current turn, or a closed turn's retained
    /// one. Every settled call counts, so a call that succeeded before its
    /// activation failed is in the usage, and so does an external writer's
    /// one admitted call, whose program reported no cost when it is Codex
    /// (`cost_known: false`). An activation the authority does not account,
    /// or a turn whose authority cannot be read, is left out; the
    /// observation then keeps the projection's usage for it.
    pub(crate) fn loadout_turn_provider_usage(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> HashMap<String, RunUsage> {
        use axocoatl_session::control_authority::ControlAuthority;
        use axocoatl_session::execution_namespace::ExecutionComponent;
        use axocoatl_session::turn_contract::{ActivationState, LogicalTurnId};
        let Ok(turn) = LogicalTurnId::new(turn_id) else {
            return HashMap::new();
        };
        let Ok(token) = self
            .session_dispatch_lifecycles
            .session_team_token(session_id)
        else {
            return HashMap::new();
        };
        self.session_dispatch_lifecycles
            .with_session_team_grant_stores(&token, |canonical, _, held| {
                let snapshot = canonical
                    .snapshot(&turn)
                    .map_err(|error| DaemonError::Session(error.to_string()))?;
                let held = held
                    .filter(|(current, _)| **current == turn)
                    .map(|(_, authority)| authority);
                let mut measured = HashMap::new();
                for item in snapshot.contract().activations() {
                    if item.state == ActivationState::Unstarted {
                        continue;
                    }
                    let usage = match held {
                        Some(authority) => authority.provider_usage(&item.activation).ok(),
                        None => canonical
                            .existing_component_namespace(
                                ExecutionComponent::ControlAuthority {
                                    turn_id: turn.clone(),
                                },
                                std::path::Path::new("control-authority.v1.json"),
                            )
                            .ok()
                            .and_then(|namespace| {
                                ControlAuthority::read_provider_usage_owned(
                                    namespace,
                                    std::slice::from_ref(&item.activation),
                                )
                                .ok()
                            }),
                    };
                    let Some(usage) = usage else {
                        continue;
                    };
                    measured.insert(
                        item.activation.activation_id.as_str().to_string(),
                        RunUsage {
                            input_tokens: usage.tokens.usage.input_tokens as u64,
                            output_tokens: usage.tokens.usage.output_tokens as u64,
                            cost_microunits: usage.cost_microunits,
                            complete: usage.tokens.complete,
                            cost_known: usage.cost_known,
                            retries: 0,
                        },
                    );
                }
                Ok(measured)
            })
            .unwrap_or_default()
    }

    /// Preview and apply `edit` on the Session's current configuration
    /// revision.
    pub async fn apply_loadout_team(
        &self,
        session_id: &str,
        mut edit: SessionTeamEdit,
    ) -> Result<(), DaemonError> {
        edit.expected_configuration_revision =
            self.session_team(session_id).await?.configuration_revision;
        let preview = self.preview_session_team(session_id, edit.clone()).await?;
        self.apply_session_team(
            session_id,
            session_team::SessionTeamApply {
                edit,
                review_digest: preview.review_digest,
            },
        )
        .await?;
        Ok(())
    }

    /// Read a file from the Session's Ready container as the writer user,
    /// at most `max_bytes`; `None` when it does not exist.
    pub async fn loadout_read_sandbox_file(
        &self,
        session_id: &str,
        path: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, DaemonError> {
        if !path.starts_with('/') || path.contains('\0') || path.split('/').any(|part| part == "..")
        {
            return Err(DaemonError::InvalidRequest(format!(
                "{path:?} is not an absolute container path"
            )));
        }
        let sandbox = self
            .session_sandboxes
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| {
                DaemonError::Session(format!("Session {session_id} has no running container"))
            })?;
        let limit = max_bytes.saturating_add(1).to_string();
        let script = "if [ ! -e \"$1\" ]; then exit 44; fi; if [ -L \"$1\" ] || [ ! -f \"$1\" ]; then exit 45; fi; exec head -c \"$2\" -- \"$1\"";
        let result = sandbox
            .exec_observed(
                &["sh", "-c", script, "sh", path, &limit],
                std::time::Duration::from_secs(60),
                max_bytes.saturating_add(1),
                4096,
            )
            .await
            .map_err(|error| DaemonError::Session(format!("reading {path}: {error}")))?;
        match result.exit_code {
            Some(0) => {
                if result.stdout.retained.len() > max_bytes || !result.stdout.complete {
                    return Err(DaemonError::Session(format!(
                        "{path} is larger than {max_bytes} bytes"
                    )));
                }
                Ok(Some(result.stdout.retained))
            }
            Some(44) => Ok(None),
            Some(45) => Err(DaemonError::Session(format!(
                "{path} is not a regular file"
            ))),
            other => Err(DaemonError::Session(format!(
                "reading {path} failed ({other:?}): {}",
                String::from_utf8_lossy(&result.stderr.retained)
            ))),
        }
    }

    /// At startup: a run left without an Outcome was cut off by a restart;
    /// it ends failed, never resumed silently. Loadout Sessions get their
    /// loadout's network additions back.
    pub(crate) async fn recover_loadout_runs(&self) {
        let Ok(store) = self.loadout_runs.store() else {
            return;
        };
        match store.unfinished() {
            Ok(runs) => {
                for run_id in runs {
                    let Ok(manifest) = store.manifest(&run_id) else {
                        continue;
                    };
                    let outcome = ended_outcome(
                        &manifest,
                        "the daemon restarted during the run; it was not resumed".into(),
                        Vec::new(),
                    );
                    let _ = store.append(
                        &run_id,
                        &RunEvent::Ended {
                            at_ms: now_ms(),
                            outcome: Box::new(outcome.clone()),
                        },
                    );
                    if let Err(error) = store.finish(&run_id, &outcome) {
                        tracing::warn!(run = %run_id, %error, "ending an interrupted loadout run failed");
                    }
                }
            }
            Err(error) => tracing::warn!(%error, "listing loadout runs failed"),
        }
        let base = self.network_policy.current();
        for session in self.list_sessions().await {
            let Some(binding) = &session.loadout else {
                continue;
            };
            let overlay = store
                .manifest(&binding.run_id)
                .map_err(|error| error.to_string())
                .and_then(|manifest| {
                    let loadout = parse_loadout(&manifest.loadout_text, LoadoutSource::Builtin)
                        .map_err(|error| error.to_string())?;
                    let resolved =
                        resolve_loadout(&loadout, &manifest.params, &manifest.task, &manifest.repo)
                            .map_err(|error| error.to_string())?;
                    crate::loadout::egress::loadout_overlay(&base, &resolved, self.data_root.path())
                        .map_err(|error| error.to_string())
                });
            match overlay {
                Ok(overlay) => self.egress_points.set_loadout_overlay(&session.id, overlay),
                Err(error) => tracing::warn!(
                    session = %session.id,
                    %error,
                    "a loadout Session's network additions cannot be restored; it keeps only the configured lists"
                ),
            }
        }
    }
}

/// Validate one loadout text without the daemon (also `axocoatl loadouts
/// validate`).
pub fn validate_text(text: &str) -> ValidateLoadoutResponse {
    if text.len() > MAX_VALIDATE_BYTES {
        return ValidateLoadoutResponse {
            valid: false,
            error: Some(format!(
                "the text is larger than {MAX_VALIDATE_BYTES} bytes"
            )),
            warnings: Vec::new(),
            summary: None,
        };
    }
    match parse_loadout(
        text,
        LoadoutSource::User {
            path: "validate".into(),
        },
    ) {
        Ok(loadout) => {
            let builtin_id =
                axocoatl_config::loadout::BUILTIN_LOADOUT_IDS.contains(&loadout.file.id.as_str());
            let mut summary = loadout_summary(&loadout);
            summary.path = None;
            ValidateLoadoutResponse {
                valid: !builtin_id,
                error: builtin_id.then(|| {
                    format!(
                        "{:?} is a built-in loadout; a user loadout takes another id",
                        loadout.file.id
                    )
                }),
                warnings: loadout.warnings.clone(),
                summary: Some(summary),
            }
        }
        Err(error) => ValidateLoadoutResponse {
            valid: false,
            error: Some(error.to_string()),
            warnings: Vec::new(),
            summary: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin(id: &str) -> Loadout {
        builtin_loadouts()
            .into_iter()
            .map(Result::unwrap)
            .find(|loadout| loadout.file.id == id)
            .unwrap()
    }

    #[test]
    fn the_fix_graph_runs_agents_then_checks_then_review() {
        let graph = loadout_graph(&builtin("fix").file);
        let ids: Vec<&str> = graph.nodes.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids, ["agent:writer", "check:tests", "review"]);
        assert_eq!(graph.nodes[0].detail[0], "param writer_model");
        let edges: Vec<(&str, &str)> = graph
            .edges
            .iter()
            .map(|edge| (edge.from.as_str(), edge.to.as_str()))
            .collect();
        assert_eq!(
            edges,
            [("agent:writer", "check:tests"), ("check:tests", "review")]
        );
    }

    #[test]
    fn the_audit_graph_shows_area_workers_between_plan_and_integration() {
        let graph = loadout_graph(&builtin("audit").file);
        let worker = graph
            .nodes
            .iter()
            .find(|node| node.id == "agent:worker")
            .unwrap();
        assert_eq!(worker.kind, "area_workers");
        assert!(graph
            .edges
            .iter()
            .any(|edge| edge.from == "agent:planner" && edge.to == "agent:worker"));
        assert!(graph
            .edges
            .iter()
            .any(|edge| edge.from == "agent:worker" && edge.to == "agent:integrator"));
    }

    #[test]
    fn validation_reports_errors_warnings_and_builtin_ids() {
        let fix = builtin("fix");
        let response = validate_text(&fix.text);
        assert!(!response.valid, "a user loadout cannot reuse the fix id");
        let mine = fix.text.replace("id: fix", "id: my-fix");
        let response = validate_text(&mine);
        assert!(response.valid, "{:?}", response.error);
        assert_eq!(response.summary.unwrap().id, "my-fix");
        let response = validate_text("schema: nope\n");
        assert!(!response.valid);
        assert!(response.error.is_some());
        let same = mine.replace(
            "model: { param: reviewer_model }",
            "model: { param: writer_model }",
        );
        let response = validate_text(&same);
        assert!(response
            .warnings
            .iter()
            .any(|warning| warning.code == "same_model_reviewer"));
    }

    #[test]
    fn summaries_carry_params_and_invalid_files_their_path() {
        let summary = loadout_summary(&builtin("qa"));
        assert!(summary.builtin);
        assert!(summary
            .params
            .iter()
            .any(|param| param.name == "explorer_model" && param.required));
        let error = LoadoutError::Invalid {
            field: "id".into(),
            reason: "bad".into(),
        };
        let row = invalid_summary(std::path::Path::new("/c/loadouts/broken.yaml"), &error);
        assert_eq!(row.id, "broken");
        assert_eq!(row.path.as_deref(), Some("/c/loadouts/broken.yaml"));
        assert!(row.error.unwrap().contains("bad"));
    }

    /// A qa URL on localhost or a loopback address is a Session port; any
    /// other host exposes nothing here and is left to the qa admission
    /// check, which reads every kind of browser.allow entry.
    #[test]
    fn qa_targets_on_loopback_are_session_ports() {
        let qa = builtin("qa");
        let mut params = axocoatl_config::loadout::ParamValues::new();
        params.insert("target_url".into(), "http://localhost:3000".into());
        params.insert("reference_url".into(), "http://127.0.0.1:3001/app".into());
        assert_eq!(
            qa_exposed_ports(&qa.file, &params).unwrap(),
            vec![3000, 3001]
        );
        params.insert("reference_url".into(), "https://staging.example.com".into());
        assert_eq!(qa_exposed_ports(&qa.file, &params).unwrap(), vec![3000]);
        params.insert("target_url".into(), "http://192.168.1.5:8766".into());
        assert!(qa_exposed_ports(&qa.file, &params).unwrap().is_empty());
        assert!(qa_exposed_ports(&builtin("fix").file, &params)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn network_none_refuses_tools_that_reach_hosts() {
        let mut file = builtin("qa").file.clone();
        assert!(refuse_hosts_under_network_none(&file).is_ok());
        file.sandbox.network = "none".into();
        assert!(matches!(
            refuse_hosts_under_network_none(&file),
            Err(DaemonError::InvalidRequest(_))
        ));
        let mut fix = builtin("fix").file.clone();
        fix.sandbox.network = "none".into();
        assert!(refuse_hosts_under_network_none(&fix).is_ok());
    }

    /// A native turn's projection with one writer node whose single
    /// activation ended in `state`, with `reason` and `usage` as recorded.
    fn projection(
        turn_state: &str,
        state: &str,
        reason: crate::session_control_plane::EvidenceValue<String>,
        usage: crate::session_control_plane::EvidenceValue<serde_json::Value>,
    ) -> crate::session_control_plane::SessionTurnControlPlane {
        use crate::session_control_plane::*;
        use axocoatl_session::turn_contract::{
            ActivationId, ActivationRef, ExecutionEpochId, LogicalTurnId, SessionId, TurnNodeId,
        };
        let unavailable = || ControlPlaneCapability {
            enabled: false,
            requires_revalidation: false,
            reason: "test".into(),
        };
        let activation = ControlPlaneActivation {
            reference: ControlPlaneActivationRef::Exact {
                activation: ActivationRef {
                    session_id: SessionId::new("ses-1").unwrap(),
                    turn_id: LogicalTurnId::new("turn-1").unwrap(),
                    execution_epoch_id: ExecutionEpochId::new("epoch-1").unwrap(),
                    node_id: TurnNodeId::new("node-0").unwrap(),
                    generation: 1,
                    activation_id: ActivationId::new("activation-1").unwrap(),
                },
            },
            generation: EvidenceValue::Available { value: 1 },
            state: state.into(),
            reason,
            started_at: EvidenceValue::NotRecorded,
            completed_at: EvidenceValue::NotRecorded,
            input: EvidenceValue::NotRecorded,
            output: EvidenceValue::NotRecorded,
            partial_outputs: Vec::new(),
            usage,
            capabilities: ControlPlaneCapabilities {
                inspect: true,
                human_responses: Vec::new(),
                stop: unavailable(),
                retry: unavailable(),
                guide: unavailable(),
                revise: unavailable(),
                revise_invalidates: Vec::new(),
            },
            evidence: Vec::new(),
        };
        SessionTurnControlPlane {
            schema_version: 1,
            history_version: "execution_v2".into(),
            superseded_conversation: false,
            session_id: "ses-1".into(),
            turn_id: "turn-1".into(),
            state: turn_state.into(),
            stop_requested: None,
            turn_revision: EvidenceValue::NotRecorded,
            graph_revision: EvidenceValue::NotRecorded,
            request: EvidenceValue::NotRecorded,
            nodes: vec![ControlPlaneNode {
                node_id: "node-0".into(),
                definition_id: "writer".into(),
                label: "writer".into(),
                definition: EvidenceValue::NotRecorded,
                dependencies: Vec::new(),
                activations: vec![activation],
            }],
            edges: Vec::new(),
            epochs: EvidenceValue::NotRecorded,
            accepted_inputs: EvidenceValue::NotRecorded,
            invocations: EvidenceValue::NotRecorded,
            conditions: EvidenceValue::NotRecorded,
            commands: EvidenceValue::NotRecorded,
            turn_controls: None,
            required_checks: Vec::new(),
            required_check_readiness: None,
            required_review: None,
            review_rounds: Vec::new(),
            decisions: EvidenceValue::NotRecorded,
            warnings: Vec::new(),
        }
    }

    /// An activation that failed with an empty recorded reason ends with
    /// "<node> ended without a result", never an empty reason: as stopped
    /// when its turn was stopped, as other otherwise.
    #[test]
    fn an_empty_reason_falls_back_to_the_node_ending_without_a_result() {
        use crate::session_control_plane::EvidenceValue;
        use axocoatl_session::run_outcome::FailureClass;
        let empty = || EvidenceValue::Available {
            value: String::new(),
        };
        let unknown = || EvidenceValue::Unknown {
            reason: "No complete usage record is attached to the accepted output.".into(),
        };
        let failure = |view| {
            observation_from_control_plane(&view, &[], None, &HashMap::new()).nodes[0].generations
                [0]
            .failure
            .clone()
            .unwrap()
        };
        let stopped = failure(projection("cancelled", "failed", empty(), unknown()));
        assert_eq!(stopped.message, "writer ended without a result");
        assert_eq!(stopped.class, FailureClass::Stopped);
        let failed = failure(projection("needs_attention", "failed", empty(), unknown()));
        assert_eq!(failed.message, "writer ended without a result");
        assert_eq!(failed.class, FailureClass::Other);
        // A recorded reason is kept, and a stopped turn does not reclassify
        // a failure that has one.
        let real = failure(projection(
            "cancelled",
            "failed",
            EvidenceValue::Available {
                value: "the provider's safety classifier stopped the stream".into(),
            },
            unknown(),
        ));
        assert_eq!(
            real.message,
            "the provider's safety classifier stopped the stream"
        );
        assert_ne!(real.class, FailureClass::Stopped);
    }

    /// The fix re-smoke's interrupted run: the writer streamed part of an
    /// answer, the person pressed Ctrl-C, and the activation failed with
    /// its reserved output as evidence, so the projection's reason was the
    /// half-written answer and the run reported "writer: other: I can see
    /// the issue now. ...". That text is no reason: the writer ended
    /// without a result, because of the stop.
    #[test]
    fn a_partial_answer_is_not_the_reason_a_node_ended() {
        use crate::session_control_plane::{ControlPlaneOutput, EvidenceValue};
        use axocoatl_session::run_outcome::FailureClass;
        let partial = "I can see the issue now. The `pageCount` function is using \
                       `Math.floor(total / pageSize)` which truncates the result.\n\nLet me \
                       also look at the tests to better";
        let streamed = |turn_state: &str, reason: &str| {
            let mut view = projection(
                turn_state,
                "failed",
                EvidenceValue::Available {
                    value: reason.into(),
                },
                EvidenceValue::Unknown {
                    reason: "No complete usage record is attached to the accepted output.".into(),
                },
            );
            view.nodes[0].activations[0]
                .partial_outputs
                .push(ControlPlaneOutput {
                    text: partial.into(),
                    truncated: false,
                    original_byte_len: EvidenceValue::Available {
                        value: partial.len() as u64,
                    },
                    reference: EvidenceValue::Available {
                        value: "content-3534dcb05e85".into(),
                    },
                });
            observation_from_control_plane(&view, &[], None, &HashMap::new()).nodes[0].generations
                [0]
            .clone()
        };
        let stopped = streamed("cancelled", partial);
        let failure = stopped.failure.unwrap();
        assert_eq!(failure.message, "writer ended without a result");
        assert_eq!(failure.class, FailureClass::Stopped);
        // The partial answer is still the generation's answer.
        assert_eq!(stopped.answer.as_deref(), Some(partial));
        // Without a stop the node still ended without a result, never with
        // its half-written answer as the reason.
        let failure = streamed("needs_attention", partial).failure.unwrap();
        assert_eq!(failure.message, "writer ended without a result");
        assert_eq!(failure.class, FailureClass::Other);
        // A reason of its own is kept beside a partial answer.
        let failure = streamed("needs_attention", "LLM provider stream ended early")
            .failure
            .unwrap();
        assert_eq!(failure.message, "LLM provider stream ended early");
        // An activation that failed with an error keeps the error as its
        // output, which the projection shows as its reason and as its
        // partial output: that is a reason.
        let error = "Activation failed: LLM provider error: error parsing tool call: raw='{\"c'";
        let mut view = projection(
            "needs_attention",
            "failed",
            EvidenceValue::Available {
                value: error.into(),
            },
            EvidenceValue::NotRecorded,
        );
        view.nodes[0].activations[0]
            .partial_outputs
            .push(ControlPlaneOutput {
                text: error.into(),
                truncated: false,
                original_byte_len: EvidenceValue::Available {
                    value: error.len() as u64,
                },
                reference: EvidenceValue::Available {
                    value: "content-1".into(),
                },
            });
        let failure = observation_from_control_plane(&view, &[], None, &HashMap::new()).nodes[0]
            .generations[0]
            .failure
            .clone()
            .unwrap();
        assert_eq!(failure.message, error);
    }

    /// A failed activation has no usage attached to an accepted answer, but
    /// its provider calls were measured: the observation counts them (the
    /// first call that succeeded before the provider failed included) and
    /// the usage is complete, not a zero "known subtotal".
    #[test]
    fn a_failed_activation_counts_the_calls_it_measured() {
        use crate::session_control_plane::EvidenceValue;
        let view = projection(
            "needs_attention",
            "failed",
            EvidenceValue::Available {
                value: "Activation failed: LLM provider stream ended early".into(),
            },
            EvidenceValue::Unknown {
                reason: "No complete usage record is attached to the accepted output.".into(),
            },
        );
        let without = observation_from_control_plane(&view, &[], None, &HashMap::new());
        assert_eq!(without.usage.input_tokens, 0);
        assert!(!without.usage.complete);
        let measured: HashMap<String, RunUsage> = [(
            "activation-1".to_string(),
            RunUsage {
                input_tokens: 1200,
                output_tokens: 80,
                cost_microunits: 0,
                complete: true,
                cost_known: true,
                retries: 0,
            },
        )]
        .into_iter()
        .collect();
        let with = observation_from_control_plane(&view, &[], None, &measured);
        assert_eq!(with.usage.input_tokens, 1200);
        assert_eq!(with.usage.output_tokens, 80);
        assert!(with.usage.complete);
        assert!(with.usage.cost_known);
    }

    /// `view` with its node's retained definition naming `provider` and
    /// `model`.
    fn defined(
        mut view: crate::session_control_plane::SessionTurnControlPlane,
        provider: &str,
        model: &str,
    ) -> crate::session_control_plane::SessionTurnControlPlane {
        use crate::session_control_plane::{ControlPlaneDefinition, EvidenceValue};
        let text = |value: &str| EvidenceValue::Available {
            value: value.to_string(),
        };
        view.nodes[0].definition = EvidenceValue::Available {
            value: ControlPlaneDefinition {
                name: text("writer"),
                role: text("Autonomous"),
                provider: text(provider),
                model: text(model),
                instructions: EvidenceValue::NotRecorded,
                tools: EvidenceValue::Available {
                    value: vec!["bash".into()],
                },
                configuration_revision: EvidenceValue::NotRecorded,
                snapshot: EvidenceValue::NotRecorded,
            },
        };
        view
    }

    /// An external writer's retained definition names its runtime as the
    /// provider. The Outcome names it as the loadout does
    /// (`team_plan::model_identity`): the model API's provider, the
    /// program's model and the runtime, never `{provider: codex, runtime:
    /// native}`.
    #[test]
    fn an_external_writer_is_named_as_the_loadout_names_it() {
        use crate::session_control_plane::EvidenceValue;
        use axocoatl_config::loadout::ModelSpec;
        let accepted = || {
            projection(
                "completed",
                "accepted",
                EvidenceValue::NotRecorded,
                EvidenceValue::NotRecorded,
            )
        };
        for (runtime, provider, model_provider, model) in [
            (
                AgentRuntime::ClaudeCode,
                "claude-code",
                "anthropic",
                "claude-sonnet-5.5",
            ),
            (AgentRuntime::Codex, "codex", "openai", "gpt-5.6-codex"),
        ] {
            let view = defined(accepted(), provider, model);
            let observed = observation_from_control_plane(&view, &[], None, &HashMap::new());
            let expected = crate::loadout::team_plan::model_identity(
                &ModelSpec {
                    provider: model_provider.into(),
                    model: model.into(),
                },
                runtime,
            );
            assert_eq!(observed.nodes[0].model, expected);
            assert_eq!(observed.nodes[0].model.runtime, provider);
        }
        let native = defined(accepted(), "ollama", "qwen3-coder:30b");
        let observed = observation_from_control_plane(&native, &[], None, &HashMap::new());
        assert_eq!(
            observed.nodes[0].model,
            ModelIdentity {
                provider: "ollama".into(),
                model: "qwen3-coder:30b".into(),
                runtime: "native".into(),
            }
        );
    }

    /// A Codex activation's calls are measured with complete token usage
    /// but no cost: the turn's usage says its cost is not known, while a
    /// native call that settled with its cost keeps it known.
    #[test]
    fn a_call_without_a_cost_makes_the_turns_cost_unknown() {
        use crate::session_control_plane::EvidenceValue;
        let view = projection(
            "completed",
            "accepted",
            EvidenceValue::NotRecorded,
            EvidenceValue::NotRecorded,
        );
        let measured = |cost_known| -> HashMap<String, RunUsage> {
            [(
                "activation-1".to_string(),
                RunUsage {
                    input_tokens: 600,
                    output_tokens: 18,
                    cost_microunits: 0,
                    complete: true,
                    cost_known,
                    retries: 0,
                },
            )]
            .into_iter()
            .collect()
        };
        let codex = observation_from_control_plane(&view, &[], None, &measured(false));
        assert!(codex.usage.complete);
        assert!(!codex.usage.cost_known);
        let native = observation_from_control_plane(&view, &[], None, &measured(true));
        assert!(native.usage.cost_known);
        // Without the authority's measure, the projection carries no cost: a
        // call whose usage is not known has no known cost either.
        let unknown = observation_from_control_plane(&view, &[], None, &HashMap::new());
        assert!(!unknown.usage.complete && !unknown.usage.cost_known);
    }

    #[test]
    fn request_ids_are_bounded() {
        assert!(valid_request_id("req-1:2.3_x"));
        assert!(!valid_request_id(""));
        assert!(!valid_request_id("a b"));
        assert!(!valid_request_id(&"x".repeat(129)));
    }
}

#[cfg(all(test, unix))]
#[path = "bootstrap_loadout_runs_tests.rs"]
mod daemon_tests;
