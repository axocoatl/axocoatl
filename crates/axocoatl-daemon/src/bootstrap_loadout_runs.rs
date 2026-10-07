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
    NetworkSummary, NodeFailure, NodeObservation, NodeState, ReviewOutcome, ReviewRound,
    ReviewVerdictKind, RunOutcome, RunUsage, RunVerdict, RunWarning, TurnObservation, TurnState,
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
}

impl LoadoutRuns {
    pub(crate) fn open(data_root: &SecureDir) -> Self {
        Self {
            store: RunRecordStore::open(data_root).map_err(|error| error.to_string()),
            admitted: StdMutex::new(HashMap::new()),
            admitting: StdMutex::new(HashSet::new()),
            drivers: StdMutex::new(HashSet::new()),
            stops: StdMutex::new(HashSet::new()),
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
        other => DaemonError::Session(other.to_string()),
    }
}

fn now_ms() -> u64 {
    crate::loadout::driver::now_ms()
}

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
        return RunUsage::default();
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

/// What the run driver observes of one turn's control-plane projection:
/// nodes and their generations (answers bounded to 64 KiB by the
/// projection), the required checks named by `checks`, the required review
/// with every round, and usage.
pub fn observation_from_control_plane(
    view: &crate::session_control_plane::SessionTurnControlPlane,
    checks: &[CheckLabel],
    reviewer_fallback: Option<&ModelIdentity>,
) -> TurnObservation {
    use crate::session_control_plane::EvidenceValue;
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
        let identity = ModelIdentity {
            provider,
            model,
            runtime: "native".into(),
        };
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
            let activation_usage = usage_of(&activation.usage);
            usage.input_tokens += activation_usage.input_tokens;
            usage.output_tokens += activation_usage.output_tokens;
            if !matches!(state, NodeState::NeverStarted | NodeState::Running) {
                usage.complete &= activation_usage.complete;
            }
            let reason = evidence_text(&activation.reason);
            let failure = matches!(
                state,
                NodeState::Failed | NodeState::Stopped | NodeState::Blocked
            )
            .then(|| {
                let message = reason
                    .clone()
                    .unwrap_or_else(|| format!("{} ended without a result", node.label));
                NodeFailure {
                    class: crate::loadout::driver::class_of(
                        &message,
                        (state == NodeState::Stopped).then_some("stopped"),
                    ),
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
        let rounds = view
            .review_rounds
            .iter()
            .map(|proof| ReviewRound {
                round: proof.round,
                verdict: match proof.verdict {
                    axocoatl_session::turn_review::ReviewVerdict::Approve => {
                        ReviewVerdictKind::Approve
                    }
                    axocoatl_session::turn_review::ReviewVerdict::Changes => {
                        ReviewVerdictKind::Changes
                    }
                    axocoatl_session::turn_review::ReviewVerdict::Unreadable => {
                        ReviewVerdictKind::Unreadable
                    }
                },
                passed: proof.passed,
                findings_text: head(&proof.findings, FINDINGS_TEXT_BYTES),
                findings: Vec::new(),
                continued: proof.continued,
                candidate_sha256: proof.candidate_sha256.clone(),
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

/// Whether `host` is one of the browser's declared hosts.
fn browser_allows(config: &AxocoatlConfig, host: &str) -> bool {
    let Some(browser) = &config.browser else {
        return false;
    };
    browser.allow.iter().any(|entry| match entry {
        axocoatl_config::EgressAllowYaml::Host(allowed) => {
            let allowed = allowed.host.to_ascii_lowercase();
            match allowed.strip_prefix("*.") {
                Some(suffix) => host.ends_with(&format!(".{suffix}")),
                None => allowed == host,
            }
        }
        _ => false,
    })
}

/// The Session ports a qa run's browser reaches the build under test (and
/// the reference build) through. A URL must be a port of the Session
/// (`http://localhost:<port>`) or a host under `browser.allow`; anything else
/// is a usage error, never a run whose browser cannot reach its target.
fn qa_exposed_ports(
    config: &AxocoatlConfig,
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
        let parsed = reqwest::Url::parse(&url)
            .map_err(|error| DaemonError::InvalidRequest(format!("{field} {url:?}: {error}")))?;
        let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
        let local = matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1");
        if local {
            let port = parsed.port_or_known_default().unwrap_or(80);
            if !ports.contains(&port) {
                ports.push(port);
            }
        } else if !browser_allows(config, &host) {
            return Err(DaemonError::InvalidRequest(format!(
                "{field} {url} is neither a port of the Session (http://localhost:<port>) nor a \
                 host under browser.allow, so the browser cannot reach it"
            )));
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
            (None, false) => Some(
                axocoatl_isolation::recipes::image_name(&environment.recipes).map_err(|error| {
                    match error {
                        axocoatl_isolation::recipes::RecipeError::NotImplemented(what) => {
                            DaemonError::NotImplemented(what)
                        }
                        other => DaemonError::InvalidRequest(other.to_string()),
                    }
                })?,
            ),
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
        let exposed_ports = qa_exposed_ports(&self.config, &loadout.file, &resolved.params)?;
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
            return Err(DaemonError::Session(
                "loadout runs need native Session history; this data directory has not been \
                 upgraded"
                    .into(),
            ));
        };
        let workspace = self
            .get_workspace(workspace_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("workspace '{workspace_id}' not found")))?;
        let operation = self.attempt_operation_for_workspace(workspace_id).await;
        let _operation = operation.lock().await;
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

    /// Whether the loadout that created `session` has an e2e check, so its
    /// container mounts `.e2e/cache` read-only (workstream e2e passes this to
    /// the Session container's policy).
    pub fn session_has_e2e_check(&self, session: &Session) -> bool {
        let Some(binding) = &session.loadout else {
            return false;
        };
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

    /// The header of `run_id`'s record bundle.
    pub fn record_bundle_header(&self, run_id: &str) -> Result<BundleHeader, DaemonError> {
        let manifest = self
            .loadout_runs
            .store()?
            .manifest(run_id)
            .map_err(record_error)?;
        Ok(BundleHeader {
            schema: RECORD_BUNDLE_SCHEMA.into(),
            run_id: manifest.run_id,
            session_id: manifest.session_id,
            created_at_ms: now_ms(),
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
                let team = match self.session_team(&manifest.session_id).await {
                    Ok(team) => value(serde_json::to_value(team))?,
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
                let history = match self.export_session_json(&manifest.session_id).await {
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
            .session_turn_control_plane(session_id, turn_id)
            .await?
            .map(|view| observation_from_control_plane(&view, checks, reviewer)))
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

    #[test]
    fn qa_targets_are_session_ports_or_declared_browser_hosts() {
        let qa = builtin("qa");
        let mut params = axocoatl_config::loadout::ParamValues::new();
        params.insert("target_url".into(), "http://localhost:3000".into());
        params.insert("reference_url".into(), "http://127.0.0.1:3001/app".into());
        let config = AxocoatlConfig::default();
        assert_eq!(
            qa_exposed_ports(&config, &qa.file, &params).unwrap(),
            vec![3000, 3001]
        );
        params.insert("reference_url".into(), "https://staging.example.com".into());
        assert!(matches!(
            qa_exposed_ports(&config, &qa.file, &params),
            Err(DaemonError::InvalidRequest(_))
        ));
        let config = AxocoatlConfig {
            browser: Some(serde_yaml::from_str("allow:\n  - host: \"*.example.com\"\n").unwrap()),
            ..Default::default()
        };
        assert_eq!(
            qa_exposed_ports(&config, &qa.file, &params).unwrap(),
            vec![3000]
        );
        assert!(qa_exposed_ports(&config, &builtin("fix").file, &params)
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
