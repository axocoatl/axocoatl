//! The audit driver against a scripted `RunHost` and a recording edit
//! builder: the turns it starts, the teams it applies, the requests it
//! sends, the coverage it checks from the recorded tool calls and the report
//! it hands to the Outcome.
use super::*;
use crate::bootstrap::session_team::SessionTeamSlotEdit;
use crate::loadout::{KeepMode, RunOptions};
use axocoatl_config::loadout::{builtin_loadouts, resolve_loadout, ModelSpec, ParamValues};
use axocoatl_session::run_outcome::{
    exit_code, FindingSource, GenerationObservation, LoadoutRef, ModelIdentity, NetworkSummary,
    NodeFailure, RunOutcome, RunUsage, RunVerdict, VerdictInputs, RUN_OUTCOME_SCHEMA,
};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

const PLANNER_MODEL: &str = "ollama:qwen3:32b";
const WORKER_MODEL: &str = "openrouter:qwen/qwen3-coder";
const INTEGRATOR_MODEL: &str = "openrouter:openai/gpt-oss-120b";

/// A repository holding `files`.
fn repository(files: &[(&str, &str)]) -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let path = repo.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    repo
}

/// One file in each area of [`three_areas`].
const THREE_FILES: [(&str, &str); 3] = [
    ("src/auth/mod.rs", "pub fn login() {}\n"),
    ("src/db/mod.rs", "pub fn query() {}\n"),
    ("src/api/mod.rs", "pub fn route() {}\n"),
];

fn context_in(deadline: Instant, repo: &Path) -> RunContext {
    let audit = builtin_loadouts()
        .into_iter()
        .map(|loadout| loadout.expect("built-in loadouts parse"))
        .find(|loadout| loadout.file.id == "audit")
        .unwrap();
    let mut params = ParamValues::new();
    params.insert("planner_model".into(), PLANNER_MODEL.into());
    params.insert("worker_model".into(), WORKER_MODEL.into());
    params.insert("integrator_model".into(), INTEGRATOR_MODEL.into());
    let resolved = resolve_loadout(&audit, &params, "find the defects", "/repo").unwrap();
    RunContext {
        run_id: "run-1".into(),
        session_id: "ses-1".into(),
        workspace_id: "wsp-1".into(),
        resolved,
        options: RunOptions {
            task: "find the defects".into(),
            repo: repo.to_path_buf(),
            params,
            keep: KeepMode::None,
            check_command: None,
            setup_command: None,
        },
        deadline,
        agent_contexts: Default::default(),
    }
}

/// The run of most tests, over a repository with [`THREE_FILES`]; the
/// directory lives as long as the returned guard.
fn context(deadline: Instant) -> (RunContext, tempfile::TempDir) {
    let repo = repository(&THREE_FILES);
    (context_in(deadline, repo.path()), repo)
}

fn later() -> Instant {
    Instant::now() + Duration::from_secs(600)
}

#[derive(Clone)]
enum Node {
    Answer(String),
    Fail(FailureClass, &'static str),
    Running,
    /// Stopped by a person's stop of the run.
    Stopped,
}

#[derive(Clone)]
struct Scripted {
    state: TurnState,
    nodes: Vec<(String, Node)>,
    /// Wait until the deadline and report the turn still running.
    until_deadline: bool,
    /// The turn's usage as observed; the default (unknown) without one.
    usage: Option<RunUsage>,
}

fn turn(state: TurnState, nodes: Vec<(&str, Node)>) -> Scripted {
    Scripted {
        state,
        nodes: nodes
            .into_iter()
            .map(|(slot, node)| (slot.to_owned(), node))
            .collect(),
        until_deadline: false,
        usage: None,
    }
}

/// One tool call a scripted worker made: tool, arguments, succeeded.
type Call = (&'static str, serde_json::Value, bool);
/// One tool call as the Session recorded it: tool, arguments, succeeded,
/// and the value it returned (`null` when not kept).
type Recorded = (String, serde_json::Value, bool, serde_json::Value);

/// Succeeded `read_file` calls of `paths`.
fn reads(paths: &[&str]) -> Vec<Call> {
    paths
        .iter()
        .map(|path| ("read_file", serde_json::json!({ "path": path }), true))
        .collect()
}

/// What the host's record of tool calls holds.
#[derive(Default)]
enum Record {
    /// Each worker that answered read `src/<area>/mod.rs`, unless
    /// [`FakeHost::calls`] scripts that activation's calls.
    #[default]
    Reads,
    /// The host keeps no record of tool calls.
    None,
    /// Reading the record fails.
    Fails,
}

#[derive(Default)]
struct FakeHost {
    script: Mutex<VecDeque<Scripted>>,
    turns: Mutex<HashMap<String, Scripted>>,
    stopped: Mutex<Vec<String>>,
    applied: Mutex<Vec<SessionTeamEdit>>,
    sent: Mutex<Vec<String>>,
    log: Mutex<Vec<String>>,
    events: Mutex<Vec<RunEvent>>,
    /// Each worker slot's scripted activations, in order: the calls the
    /// Session recorded for each turn the slot answers in.
    calls: Mutex<HashMap<String, VecDeque<Vec<Recorded>>>>,
    /// The scripted calls each turn's slots took when it was sent.
    turn_calls: Mutex<HashMap<String, HashMap<String, Vec<Recorded>>>>,
    record: Record,
    /// A person asked to stop the run.
    stop: std::sync::atomic::AtomicBool,
    /// A person asks to stop the run while its first turn runs.
    stop_on_wait: std::sync::atomic::AtomicBool,
    /// A person asks to stop the run while this turn runs.
    stop_on_turn: Mutex<Option<String>>,
    /// The run's repository: a succeeded `read_file` scripted without a
    /// result returns what the tool would return of the file there.
    repo: Mutex<Option<std::path::PathBuf>>,
}

/// A scripted `read_file` result the Session did not keep whole.
const NOT_KEPT: &str = "__result_not_kept__";

/// What `read_file` returns for `arguments` from the file under `repo`, as
/// the tool returns it with its largest default window; `null` when there
/// is no such file.
fn simulated_read(repo: &Path, arguments: &serde_json::Value) -> serde_json::Value {
    let number = |key: &str| match arguments.get(key) {
        Some(serde_json::Value::Number(number)) => number.as_u64(),
        Some(serde_json::Value::String(text)) => text.trim().parse().ok(),
        _ => None,
    };
    let path = arguments["path"].as_str().unwrap_or_default();
    let Some(relative) = files::repo_relative(path.trim(), repo) else {
        return serde_json::Value::Null;
    };
    let Ok(bytes) = std::fs::read(repo.join(relative)) else {
        return serde_json::Value::Null;
    };
    let offset = number("offset").unwrap_or(0);
    let limit = number("limit")
        .unwrap_or(files::READ_WINDOW_BYTES)
        .min(files::READ_WINDOW_BYTES);
    let start = (offset as usize).min(bytes.len());
    let end = (start + limit as usize).min(bytes.len());
    let truncated = end < bytes.len();
    let mut result = serde_json::json!({
        "content": String::from_utf8_lossy(&bytes[start..end]),
        "truncated": truncated,
        "returned_bytes": end - start,
        "output_limit_bytes": limit,
    });
    if offset > 0 {
        result["offset"] = offset.into();
    }
    if truncated {
        result["next_offset"] = (end as u64).into();
    }
    result
}

impl FakeHost {
    fn new(script: Vec<Scripted>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            ..Self::default()
        }
    }

    fn with_record(mut self, record: Record) -> Self {
        self.record = record;
        self
    }

    /// Script the tool calls of `slot`'s next activation; each succeeded
    /// `read_file` returns what the tool would ([`simulated_read`]).
    fn calls(self, slot: &str, calls: Vec<Call>) -> Self {
        let calls = calls
            .into_iter()
            .map(|(tool, arguments, succeeded)| {
                (
                    tool.to_owned(),
                    arguments,
                    succeeded,
                    serde_json::Value::Null,
                )
            })
            .collect();
        self.calls
            .lock()
            .unwrap()
            .entry(slot.into())
            .or_default()
            .push_back(calls);
        self
    }

    /// The tool calls each worker slot made in one turn, as a run recorded
    /// them: one JSON object per line with `slot`, `tool`, `arguments` and
    /// `result`, `{"Ok": value}` or `{"Err": message}`, as the Session
    /// stored it.
    fn recorded_calls(self, lines: &str) -> Self {
        let mut by_slot: Vec<(String, Vec<Recorded>)> = Vec::new();
        for line in lines.lines().filter(|line| !line.trim().is_empty()) {
            let call: serde_json::Value = serde_json::from_str(line).unwrap();
            let returned = call["result"].get("Ok").cloned();
            let slot = call["slot"].as_str().unwrap().to_owned();
            let recorded = (
                call["tool"].as_str().unwrap().to_owned(),
                call["arguments"].clone(),
                returned.is_some(),
                returned.unwrap_or_default(),
            );
            match by_slot.iter_mut().find(|(name, _)| *name == slot) {
                Some((_, calls)) => calls.push(recorded),
                None => by_slot.push((slot, vec![recorded])),
            }
        }
        {
            let mut calls = self.calls.lock().unwrap();
            for (slot, recorded) in by_slot {
                calls.entry(slot).or_default().push_back(recorded);
            }
        }
        self
    }

    fn phases(&self, name: &str) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                RunEvent::Phase { phase, detail, .. } if phase == name => Some(detail.clone()),
                _ => None,
            })
            .collect()
    }

    fn observe(&self, turn_id: &str, scripted: &Scripted, state: TurnState) -> TurnObservation {
        let stopped = self.stopped.lock().unwrap().iter().any(|id| id == turn_id);
        let nodes = scripted
            .nodes
            .iter()
            .enumerate()
            .map(|(index, (slot, node))| {
                let generation = match node {
                    Node::Answer(answer) => GenerationObservation {
                        generation: 1,
                        state: NodeState::Accepted,
                        answer: Some(answer.clone()),
                        failure: None,
                    },
                    Node::Fail(class, message) => GenerationObservation {
                        generation: 1,
                        state: NodeState::Failed,
                        answer: None,
                        failure: Some(NodeFailure {
                            class: *class,
                            message: (*message).into(),
                        }),
                    },
                    Node::Stopped => GenerationObservation {
                        generation: 1,
                        state: NodeState::Stopped,
                        answer: None,
                        failure: None,
                    },
                    Node::Running => GenerationObservation {
                        generation: 1,
                        state: if stopped {
                            NodeState::Stopped
                        } else {
                            NodeState::Running
                        },
                        answer: None,
                        failure: None,
                    },
                };
                NodeObservation {
                    node_id: format!("{turn_id}-node-{index}"),
                    slot_id: slot.clone(),
                    model: ModelIdentity {
                        provider: "ollama".into(),
                        model: "m".into(),
                        runtime: "native".into(),
                    },
                    required: true,
                    kind: "slot".into(),
                    generations: vec![generation],
                }
            })
            .collect();
        TurnObservation {
            session_id: "ses-1".into(),
            turn_id: turn_id.into(),
            state,
            attention_reason: None,
            nodes,
            checks: Vec::new(),
            review: None,
            usage: scripted.usage.clone().unwrap_or_default(),
        }
    }

    fn sent(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

#[async_trait]
impl RunHost for FakeHost {
    async fn apply_team(&self, session_id: &str, edit: SessionTeamEdit) -> Result<(), RunError> {
        assert_eq!(session_id, "ses-1");
        self.log.lock().unwrap().push("apply".into());
        self.applied.lock().unwrap().push(edit);
        Ok(())
    }

    async fn send_turn(&self, session_id: &str, request: &str) -> Result<String, RunError> {
        assert_eq!(session_id, "ses-1");
        let mut sent = self.sent.lock().unwrap();
        sent.push(request.into());
        let turn_id = format!("turn-{}", sent.len());
        let scripted = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .expect("a scripted turn for every request");
        let mut taken = HashMap::new();
        {
            let mut calls = self.calls.lock().unwrap();
            for (slot, _) in &scripted.nodes {
                if let Some(next) = calls.get_mut(slot).and_then(VecDeque::pop_front) {
                    taken.insert(slot.clone(), next);
                }
            }
        }
        self.turn_calls
            .lock()
            .unwrap()
            .insert(turn_id.clone(), taken);
        self.turns.lock().unwrap().insert(turn_id.clone(), scripted);
        self.log.lock().unwrap().push(format!("send:{turn_id}"));
        Ok(turn_id)
    }

    async fn wait_turn(
        &self,
        _session_id: &str,
        turn_id: &str,
        deadline: Instant,
    ) -> Result<TurnObservation, RunError> {
        let scripted = self.turns.lock().unwrap()[turn_id].clone();
        if self.stop_on_wait.load(std::sync::atomic::Ordering::SeqCst)
            || self.stop_on_turn.lock().unwrap().as_deref() == Some(turn_id)
        {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if self.stopped.lock().unwrap().iter().any(|id| id == turn_id) {
            return Ok(self.observe(turn_id, &scripted, TurnState::Stopped));
        }
        if scripted.until_deadline {
            tokio::time::sleep_until(deadline.into()).await;
            return Ok(self.observe(turn_id, &scripted, TurnState::Running));
        }
        Ok(self.observe(turn_id, &scripted, scripted.state))
    }

    async fn stop_turn(&self, _session_id: &str, turn_id: &str) -> Result<(), RunError> {
        self.log.lock().unwrap().push(format!("stop:{turn_id}"));
        self.stopped.lock().unwrap().push(turn_id.into());
        Ok(())
    }

    async fn run_repro(
        &self,
        _session_id: &str,
        _request: &crate::loadout::host::ReproRequest,
    ) -> Result<axocoatl_session::run_outcome::ReproRun, RunError> {
        unreachable!("an audit runs no reproductions")
    }

    async fn read_sandbox_file(
        &self,
        _session_id: &str,
        _path: &str,
        _max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, RunError> {
        unreachable!("an audit reads no sandbox files")
    }

    async fn record(&self, run_id: &str, event: RunEvent) -> Result<(), RunError> {
        assert_eq!(run_id, "run-1");
        self.events.lock().unwrap().push(event);
        Ok(())
    }

    async fn recorded_events(&self, run_id: &str) -> Result<Vec<RunEvent>, RunError> {
        assert_eq!(run_id, "run-1");
        Ok(self.events.lock().unwrap().clone())
    }

    async fn tool_calls(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<Vec<ToolCallRecord>>, RunError> {
        assert_eq!(session_id, "ses-1");
        let read: fn(&str) -> Vec<String> = match self.record {
            Record::None => return Ok(None),
            Record::Fails => {
                return Err(RunError::Infrastructure(
                    "the invocation audit could not be read".into(),
                ))
            }
            Record::Reads => |area| vec![format!("src/{area}/mod.rs")],
        };
        let scripted = self.turns.lock().unwrap()[turn_id].clone();
        let taken = self.turn_calls.lock().unwrap()[turn_id].clone();
        let mut records = Vec::new();
        for (index, (slot, node)) in scripted.nodes.iter().enumerate() {
            let Some(area) = slot.strip_prefix(WORKER_SLOT_PREFIX) else {
                continue;
            };
            let calls = match (taken.get(slot), node) {
                (Some(calls), _) => calls.clone(),
                (None, Node::Answer(_)) => read(area)
                    .into_iter()
                    .map(|path| {
                        (
                            "read_file".to_owned(),
                            serde_json::json!({ "path": path }),
                            true,
                            serde_json::Value::Null,
                        )
                    })
                    .collect(),
                (None, _) => Vec::new(),
            };
            let repo = self.repo.lock().unwrap().clone();
            records.extend(
                calls
                    .into_iter()
                    .map(|(tool, arguments, succeeded, result)| {
                        let result = match (&result, &repo) {
                            (serde_json::Value::String(text), _) if text == NOT_KEPT => {
                                serde_json::Value::Null
                            }
                            (serde_json::Value::Null, Some(repo))
                                if succeeded && tool == "read_file" =>
                            {
                                simulated_read(repo, &arguments)
                            }
                            _ => result,
                        };
                        ToolCallRecord {
                            node_id: format!("{turn_id}-node-{index}"),
                            generation: 1,
                            tool,
                            arguments,
                            succeeded,
                            result,
                        }
                    }),
            );
        }
        Ok(Some(records))
    }

    async fn stop_requested(&self, _run_id: &str) -> bool {
        self.stop.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Every `team_edit` call, and an edit that is deliberately loose (writes
/// anything, keeps history, optional, with the plans' dependencies and a
/// check) so the tests see the driver narrow it.
#[derive(Default)]
struct Builds {
    calls: Mutex<Vec<(Vec<SlotPlan>, bool, u64)>>,
}

impl Builds {
    fn build(
        &self,
        _resolved: &ResolvedLoadout,
        slots: &[SlotPlan],
        with_checks_and_review: bool,
        revision: u64,
    ) -> Result<SessionTeamEdit, RunError> {
        self.calls
            .lock()
            .unwrap()
            .push((slots.to_vec(), with_checks_and_review, revision));
        Ok(SessionTeamEdit {
            command_id: format!("apply-{revision}"),
            expected_configuration_revision: revision,
            slots: slots
                .iter()
                .map(|slot| SessionTeamSlotEdit {
                    slot_id: slot.slot_id.clone(),
                    template_id: None,
                    source_slot_id: None,
                    role: Default::default(),
                    delegation: None,
                    name: slot.slot_id.clone(),
                    provider: slot.model.provider.clone(),
                    model: slot.model.model.clone(),
                    instructions: slot.instructions.clone(),
                    max_output_tokens: None,
                    writes: None,
                    required: false,
                    reset_history: false,
                    definition: None,
                    limits: None,
                    expires_at_ms: None,
                })
                .collect(),
            dependencies: slots
                .iter()
                .flat_map(|slot| {
                    slot.depends_on.iter().map(|parent| {
                        crate::bootstrap::session_team::SessionTeamConnection {
                            parent: parent.clone(),
                            child: slot.slot_id.clone(),
                        }
                    })
                })
                .collect(),
            layout: Vec::new(),
            required_checks: vec![vec!["true".into()]],
            required_review: None,
            check_options: Vec::new(),
        })
    }
}

async fn drive(host: &FakeHost, run: &RunContext) -> (KindReport, Vec<(Vec<SlotPlan>, bool, u64)>) {
    *host.repo.lock().unwrap() = Some(run.options.repo.clone());
    let builds = Arc::new(Builds::default());
    let recorder = builds.clone();
    let build =
        move |resolved: &ResolvedLoadout, slots: &[SlotPlan], checks: bool, revision: u64| {
            recorder.build(resolved, slots, checks, revision)
        };
    let report = drive_audit(host, run, &build).await.unwrap();
    let calls = builds.calls.lock().unwrap().clone();
    (report, calls)
}

/// The run's Outcome, as the run driver builds it, on `host`.
async fn outcome_with(host: &FakeHost, run: &RunContext) -> Result<RunOutcome, RunError> {
    *host.repo.lock().unwrap() = Some(run.options.repo.clone());
    crate::loadout::driver::run_to_outcome_with(host, run, &AuditDriver).await
}

fn plan_answer(areas: &[(&str, &str, &[&str])]) -> String {
    let areas: Vec<serde_json::Value> = areas
        .iter()
        .map(|(name, scope, paths)| serde_json::json!({"name": name, "scope": scope, "paths": paths}))
        .collect();
    format!(
        "I listed the tree.\nAREAS\n```json\n{}\n```",
        serde_json::json!({ "areas": areas })
    )
}

fn three_areas() -> String {
    plan_answer(&[
        ("auth", "login and tokens", &["src/auth/**"]),
        ("db", "queries and migrations", &["src/db/**"]),
        ("api", "HTTP handlers", &["src/api/**"]),
    ])
}

fn worker_answer(findings: &[(&str, &str)], not_reached: &[&str]) -> String {
    let findings: Vec<serde_json::Value> = findings
        .iter()
        .enumerate()
        .map(|(index, (title, location))| {
            serde_json::json!({"id": format!("F{}", index + 1), "title": title, "detail": format!("{title}, with evidence"), "severity": "high", "location": location})
        })
        .collect();
    format!(
        "Done.\nFINDINGS\n```json\n{}\n```\nNOT_REACHED\n```json\n{}\n```",
        serde_json::to_string(&findings).unwrap(),
        serde_json::to_string(not_reached).unwrap()
    )
}

fn integrated_answer(findings: &[(&str, &str)]) -> String {
    let findings: Vec<serde_json::Value> = findings
        .iter()
        .map(|(title, area)| serde_json::json!({"title": title, "severity": "high", "area": area}))
        .collect();
    format!(
        "Merged.\nFINDINGS\n```json\n{}\n```",
        serde_json::to_string(&findings).unwrap()
    )
}

fn planned() -> Scripted {
    turn(
        TurnState::Completed,
        vec![(PLANNER_SLOT, Node::Answer(three_areas()))],
    )
}

fn integrated(findings: &[(&str, &str)]) -> Scripted {
    turn(
        TurnState::Completed,
        vec![(INTEGRATOR_SLOT, Node::Answer(integrated_answer(findings)))],
    )
}

fn purposes(report: &KindReport) -> Vec<&str> {
    report
        .turn_refs
        .iter()
        .map(|turn| turn.purpose.as_str())
        .collect()
}

fn entries(not_covered: &[NotCovered]) -> Vec<(&str, FailureClass, &str)> {
    not_covered
        .iter()
        .map(|entry| (entry.area.as_str(), entry.class, entry.detail.as_str()))
        .collect()
}

fn outcome_of(report: &KindReport) -> RunOutcome {
    let mut outcome = RunOutcome {
        schema: RUN_OUTCOME_SCHEMA.into(),
        run_id: "run-1".into(),
        session_id: "ses-1".into(),
        workspace_id: "wsp-1".into(),
        loadout: LoadoutRef {
            id: "audit".into(),
            version: 1,
            kind: "audit".into(),
            digest: "0".repeat(64),
            builtin: true,
        },
        task: "find the defects".into(),
        started_at_ms: 1,
        finished_at_ms: 2,
        verdict: RunVerdict::Pass,
        exit_code: 0,
        attention: Vec::new(),
        turns: report.turn_refs.clone(),
        checks: Vec::new(),
        review: None,
        adjudications: Vec::new(),
        findings: report.findings.clone(),
        not_covered: report.not_covered.clone(),
        unreadable_findings: report.unreadable_findings.clone(),
        notes: report.notes.clone(),
        warnings: Vec::new(),
        usage: RunUsage::default(),
        network: NetworkSummary::default(),
        keep: None,
        error: None,
    };
    outcome.decide(VerdictInputs {
        fail_on_findings: report.fail_on_findings,
        turn_needs_attention: false,
        budget_exhausted: report.budget_exhausted,
        interrupted: false,
    });
    outcome
}

#[tokio::test]
async fn a_plan_runs_one_fresh_read_only_worker_per_area_then_integrates() {
    let host = FakeHost::new(vec![
        planned(),
        turn(
            TurnState::Completed,
            vec![
                (
                    "worker-auth",
                    Node::Answer(worker_answer(
                        &[("token compared with ==", "src/auth.rs:42")],
                        &[],
                    )),
                ),
                (
                    "worker-db",
                    Node::Answer(worker_answer(
                        &[("SQL built by format!", "src/db.rs:7")],
                        &[],
                    )),
                ),
                ("worker-api", Node::Answer(worker_answer(&[], &[]))),
            ],
        ),
        integrated(&[
            ("token compared with ==", "auth"),
            ("SQL built by format!", "db"),
        ]),
    ]);
    let (run, _repo) = context(later());
    let (report, calls) = drive(&host, &run).await;

    // Three Applies, none with checks or review, at consecutive revisions.
    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|(_, checks, _)| !checks));
    assert_eq!(
        calls
            .iter()
            .map(|(_, _, revision)| *revision)
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    let slot_ids = |index: usize| {
        calls[index]
            .0
            .iter()
            .map(|slot| slot.slot_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(slot_ids(0), [PLANNER_SLOT]);
    assert_eq!(slot_ids(1), ["worker-auth", "worker-db", "worker-api"]);
    assert_eq!(slot_ids(2), [INTEGRATOR_SLOT]);
    for (slots, _, _) in &calls {
        for slot in slots {
            assert_eq!(
                slot.agent.writes.as_deref(),
                Some(&[][..]),
                "{}",
                slot.slot_id
            );
            assert!(slot.depends_on.is_empty() && slot.agent.depends_on.is_empty());
            assert!(slot.required);
        }
    }
    for slot in &calls[1].0 {
        assert_eq!(slot.agent.role, LoadoutRole::Worker);
        assert_eq!(Some(&slot.model), ModelSpec::parse(WORKER_MODEL).as_ref());
        let instructions = slot.instructions.as_deref().unwrap();
        assert!(
            instructions.contains("Audit only your area"),
            "{instructions}"
        );
        assert!(
            instructions.contains("at the same time, each with a fresh context: stay inside yours")
                && instructions.contains("The host checks your read_file calls, not your answer"),
            "{instructions}"
        );
        assert!(instructions.contains("FINDINGS") && instructions.contains("NOT_REACHED"));
    }
    // Each worker is told the files the host gave its area.
    let auth = calls[1].0[0].instructions.as_deref().unwrap();
    assert!(auth.contains("Your area: auth") && auth.contains("login and tokens"));
    assert!(
        auth.contains("- src/auth/**")
            && auth.contains(
                "Your files: the host listed the repository and gave your area 1 file to read. \
                 Read every one of them:\n- src/auth/mod.rs\n"
            )
            && auth.contains("(db, api)"),
        "{auth}"
    );
    assert!(!auth.contains("src/db/mod.rs"));
    assert_eq!(
        calls[0].0[0].model,
        ModelSpec::parse(PLANNER_MODEL).unwrap()
    );
    assert_eq!(
        calls[2].0[0].model,
        ModelSpec::parse(INTEGRATOR_MODEL).unwrap()
    );

    // What reached the daemon: read-only, fresh, required, unconnected.
    for edit in host.applied.lock().unwrap().iter() {
        assert!(edit.dependencies.is_empty());
        assert!(edit.required_checks.is_empty() && edit.check_options.is_empty());
        assert!(edit.required_review.is_none());
        for slot in &edit.slots {
            assert_eq!(slot.writes, Some(Some(Vec::new())));
            assert!(slot.reset_history && slot.required);
        }
    }

    let sent = host.sent();
    assert_eq!(sent.len(), 3);
    assert!(sent[0].starts_with("Audit this repository: find the defects"));
    assert!(
        sent[0].contains("one AREAS block of 2-8 areas")
            && sent[0].contains("each worker must read every file it is given"),
        "{}",
        sent[0]
    );
    assert!(sent[1].contains("3 areas of the plan as executed in parallel: auth, db, api"));
    assert!(sent[2].contains("REPORT of area auth") && sent[2].contains("token compared with =="));
    assert!(sent[2].contains("REPORT of area db") && sent[2].contains("SQL built by format!"));
    assert!(sent[2].contains("REPORT of area api"));
    assert!(!sent[2].contains("Not covered") && !sent[2].contains("did not read"));

    // The plan as executed, the assignment and each area's coverage are
    // in the record.
    assert_eq!(
        host.phases("assigned"),
        [
            "3 files listed by a walk of the directory (not a Git work tree) in 3 areas: auth \
             1, db 1, api 1",
            "auth (paths src/auth/**): src/auth/mod.rs",
            "db (paths src/db/**): src/db/mod.rs",
            "api (paths src/api/**): src/api/mod.rs",
        ]
    );
    assert_eq!(
        host.phases("coverage"),
        [
            "auth: 1 of 1 files examined: 1 read",
            "db: 1 of 1 files examined: 1 read",
            "api: 1 of 1 files examined: 1 read",
        ]
    );
    assert!(report.notes.is_empty(), "{:?}", report.notes);

    assert_eq!(report.findings.len(), 2);
    assert!(report
        .findings
        .iter()
        .all(|finding| finding.source == FindingSource::Integrator));
    assert_eq!(report.findings[1].area.as_deref(), Some("db"));
    assert!(report.not_covered.is_empty());
    assert!(!report.fail_on_findings && !report.budget_exhausted);
    assert_eq!(
        purposes(&report),
        [PLAN_PURPOSE, AREAS_PURPOSE, INTEGRATE_PURPOSE]
    );
    assert_eq!(report.turns.len(), 3);
    assert!(host.stopped.lock().unwrap().is_empty());
    let events = host.events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RunEvent::TurnStarted { .. }))
            .count(),
        3
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RunEvent::Finding { .. }))
            .count(),
        2
    );

    // fail_on_findings is false: findings are reported, the exit is 0.
    let outcome = outcome_of(&report);
    assert_eq!(
        outcome.exit_code,
        exit_code::PASS,
        "{:?}",
        outcome.attention
    );
}

/// The heart of host-verified coverage: a worker that skips a file of its
/// area gets a follow-up activation naming exactly that file, and its read
/// there covers it; its findings join the area's report.
#[tokio::test]
async fn a_skipped_file_is_named_in_a_follow_up_whose_read_covers_it() {
    let repo = repository(&[
        ("src/auth/mod.rs", "pub fn login() {}\n"),
        ("src/auth/session.rs", "pub fn renew() {}\n"),
        ("src/db/mod.rs", "pub fn query() {}\n"),
        ("src/api/mod.rs", "pub fn route() {}\n"),
    ]);
    let run = context_in(later(), repo.path());
    let host = FakeHost::new(vec![
        planned(),
        turn(
            TurnState::Completed,
            vec![
                (
                    "worker-auth",
                    Node::Answer(worker_answer(
                        &[("token compared with ==", "src/auth/mod.rs:1")],
                        &[],
                    )),
                ),
                ("worker-db", Node::Answer(worker_answer(&[], &[]))),
                ("worker-api", Node::Answer(worker_answer(&[], &[]))),
            ],
        ),
        turn(
            TurnState::Completed,
            vec![(
                "worker-auth",
                Node::Answer(worker_answer(
                    &[("session never expires", "src/auth/session.rs:1")],
                    &[],
                )),
            )],
        ),
        integrated(&[
            ("token compared with ==", "auth"),
            ("session never expires", "auth"),
        ]),
    ])
    .calls("worker-auth", reads(&["src/auth/mod.rs"]))
    .calls("worker-auth", reads(&["src/auth/session.rs"]));
    let (report, calls) = drive(&host, &run).await;

    assert_eq!(
        purposes(&report),
        [
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            FOLLOW_UP_PURPOSE,
            INTEGRATE_PURPOSE
        ]
    );
    // The follow-up is a fresh read-only activation of the auth worker
    // alone, told exactly the file it did not read.
    let follow_up: Vec<&str> = calls[2]
        .0
        .iter()
        .map(|slot| slot.slot_id.as_str())
        .collect();
    assert_eq!(follow_up, ["worker-auth"]);
    let instructions = calls[2].0[0].instructions.as_deref().unwrap();
    assert!(
        instructions.contains(
            "Follow-up 1: the host checked the read_file calls of your area's worker, and these \
             files of your area were not read to their end. Read these files and report \
             additional findings in the same format:\n- src/auth/session.rs\nYou are read-only"
        ),
        "{instructions}"
    );
    assert!(!instructions.contains("src/auth/mod.rs"), "{instructions}");
    let edit = &host.applied.lock().unwrap()[2];
    assert!(edit
        .slots
        .iter()
        .all(|slot| slot.reset_history && slot.required && slot.writes == Some(Some(Vec::new()))));
    let sent = host.sent();
    assert!(
        sent[2].contains(
            "Follow-up 1 of the audit's areas (auth): the host found files of these areas that \
             their workers did not read"
        ),
        "{}",
        sent[2]
    );
    assert_eq!(
        host.phases("applying_team")[2],
        "audit follow-up 1: 1 read-only worker (auth (1 unread))"
    );
    // The integrator gets the area's findings of both activations, the
    // follow-up's under their own ids.
    let request = &sent[3];
    assert!(
        request.contains("\"id\":\"auth-F1\"") && request.contains("\"id\":\"auth-followup1-F1\""),
        "{request}"
    );
    assert!(!request.contains("did not read"), "{request}");
    assert_eq!(
        host.phases("coverage")[0],
        "auth: 2 of 2 files examined: 2 read"
    );
    assert!(report.not_covered.is_empty(), "{:?}", report.not_covered);
    assert_eq!(outcome_of(&report).exit_code, exit_code::PASS);
}

/// A worker that reads nothing of its area in its turn gets a follow-up;
/// one that reads something new gets another, naming only the files still
/// unread; once a follow-up reads nothing new there is no other, and each
/// file still unread is not covered, listed by file.
#[tokio::test]
async fn files_unread_when_a_follow_up_reads_nothing_new_are_not_covered_by_file() {
    let repo = repository(&[
        ("src/auth/mod.rs", "pub fn login() {}\n"),
        ("src/auth/session.rs", "pub fn renew() {}\n"),
        ("src/db/mod.rs", "pub fn query() {}\n"),
        ("src/api/mod.rs", "pub fn route() {}\n"),
    ]);
    let run = context_in(later(), repo.path());
    let auth_only = |answer: String| {
        turn(
            TurnState::Completed,
            vec![("worker-auth", Node::Answer(answer))],
        )
    };
    let host = FakeHost::new(vec![
        planned(),
        turn(
            TurnState::Completed,
            vec![
                (
                    "worker-auth",
                    Node::Answer(worker_answer(&[], &["src/auth"])),
                ),
                ("worker-db", Node::Answer(worker_answer(&[], &[]))),
                ("worker-api", Node::Answer(worker_answer(&[], &[]))),
            ],
        ),
        auth_only(worker_answer(&[], &[])),
        auth_only(worker_answer(&[], &[])),
        integrated(&[]),
    ])
    .calls(
        "worker-auth",
        vec![
            ("list_dir", serde_json::json!({"path": "src/auth"}), true),
            (
                "grep",
                serde_json::json!({"pattern": "fn", "path": "src/auth"}),
                true,
            ),
        ],
    )
    .calls("worker-auth", reads(&["src/auth/mod.rs"]))
    .calls(
        "worker-auth",
        vec![(
            "read_file",
            serde_json::json!({"path": "src/auth/session.rs"}),
            false,
        )],
    );
    let (report, calls) = drive(&host, &run).await;
    assert_eq!(
        purposes(&report),
        [
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            FOLLOW_UP_PURPOSE,
            FOLLOW_UP_PURPOSE,
            INTEGRATE_PURPOSE
        ]
    );
    let first = calls[2].0[0].instructions.as_deref().unwrap();
    assert!(
        first.contains("Follow-up 1:")
            && first.contains("format:\n- src/auth/mod.rs\n- src/auth/session.rs\n"),
        "{first}"
    );
    let second = calls[3].0[0].instructions.as_deref().unwrap();
    assert!(
        second.contains("Follow-up 2:")
            && second.contains("format:\n- src/auth/session.rs\nYou are read-only"),
        "{second}"
    );
    assert_eq!(
        entries(&report.not_covered),
        [(
            "auth",
            FailureClass::NotReached,
            "src/auth/session.rs: not read (the area worker did not read it to its end in its \
             turn or its 2 follow-ups, the last of which read nothing new)"
        )]
    );
    // The worker's own account is a note, not the coverage.
    assert_eq!(
        report.notes,
        [
            "worker-auth listed as not reached: src/auth; a note: the host decides coverage from \
          the files its workers read"
        ]
    );
    let request = &host.sent()[4];
    assert!(
        request.contains(
            "Its worker did not read 1 of this area's 2 files to read (they are listed as not \
             covered)"
        ) && request.contains("- auth (not_reached): src/auth/session.rs: not read"),
        "{request}"
    );
    assert_eq!(
        host.phases("coverage")[0],
        "auth: 1 of 2 files examined: 1 read; not read, not covered: src/auth/session.rs"
    );
    let outcome = outcome_of(&report);
    assert_eq!(outcome.exit_code, exit_code::NEEDS_ATTENTION);
    assert_eq!(outcome.attention, ["1 area was not covered"]);
}

/// A worker without a result gets a follow-up too, unless the wall clock,
/// a person or its provider's refusal of the request itself ended it; an
/// unreadable report is not covered, and what a worker lists as not
/// reached is a note.
#[tokio::test]
async fn failed_workers_get_follow_ups_unless_the_failure_would_repeat() {
    let host = FakeHost::new(vec![
        planned(),
        turn(
            TurnState::NeedsAttention,
            vec![
                (
                    "worker-auth",
                    Node::Answer(worker_answer(
                        &[("token compared with ==", "src/auth.rs:42")],
                        &["src/auth/oauth.rs"],
                    )),
                ),
                (
                    "worker-db",
                    Node::Fail(FailureClass::ProviderFailure, "connection reset"),
                ),
                ("worker-api", Node::Answer("I looked around.".into())),
            ],
        ),
        turn(
            TurnState::Completed,
            vec![(
                "worker-db",
                Node::Answer(worker_answer(
                    &[("SQL built by format!", "src/db.rs:7")],
                    &[],
                )),
            )],
        ),
        // The api worker's two re-asks answer in prose too.
        turn(
            TurnState::Completed,
            vec![("worker-api", Node::Answer("Nothing to add.".into()))],
        ),
        turn(
            TurnState::Completed,
            vec![("worker-api", Node::Answer("Still nothing.".into()))],
        ),
        integrated(&[("token compared with ==", "auth")]),
    ]);
    let (run, _repo) = context(later());
    let (report, calls) = drive(&host, &run).await;

    // The paused areas turn is stopped before the follow-up's Apply.
    assert_eq!(
        host.log(),
        [
            "apply",
            "send:turn-1",
            "apply",
            "send:turn-2",
            "stop:turn-2",
            "apply",
            "send:turn-3",
            "apply",
            "send:turn-4",
            "apply",
            "send:turn-5",
            "apply",
            "send:turn-6"
        ]
    );
    assert_eq!(
        purposes(&report),
        [
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            FOLLOW_UP_PURPOSE,
            REASK_PURPOSE,
            REASK_PURPOSE,
            INTEGRATE_PURPOSE
        ]
    );
    let follow_up: Vec<&str> = calls[2]
        .0
        .iter()
        .map(|slot| slot.slot_id.as_str())
        .collect();
    assert_eq!(follow_up, ["worker-db"]);
    assert!(calls[2].0[0]
        .instructions
        .as_deref()
        .unwrap()
        .contains("format:\n- src/db/mod.rs\n"));
    assert_eq!(report.turn_refs[1].state, TurnState::Stopped);
    // The failed activation is accounted for by the audit: the Outcome's
    // node scan does not list it again.
    assert_eq!(
        report.accounted,
        [("turn-2".to_string(), "turn-2-node-1".to_string())]
    );
    // The api worker read its file: its findings are unreadable, but the
    // area is covered.
    assert!(report.not_covered.is_empty(), "{:?}", report.not_covered);
    let no_block = "the answer has no FINDINGS block: write a line FINDINGS, then a fenced \
                    JSON array of findings ([] when there are none)";
    assert_eq!(
        report.unreadable_findings,
        [UnreadableFindings {
            area: "api".into(),
            detail: format!(
                "the area worker's FINDINGS block could not be read ({no_block}), nor after 2 \
                 re-asks (the last: {no_block}); its files' coverage stands, and the integrator \
                 received its answer as text"
            ),
            answers: vec![
                "I looked around.".into(),
                "Nothing to add.".into(),
                "Still nothing.".into()
            ],
            node_id: Some("turn-5-node-0".into()),
            turn_id: Some("turn-5".into()),
        }]
    );
    assert_eq!(
        host.phases("coverage")[2],
        "api: 1 of 1 files examined: 1 read"
    );
    assert_eq!(
        outcome_of(&report).attention,
        ["Findings unreadable for api"]
    );
    assert_eq!(
        report.notes,
        [
            "worker-auth listed as not reached: src/auth/oauth.rs; a note: the host decides \
          coverage from the files its workers read"
        ]
    );
    let request = &host.sent()[5];
    assert!(request.contains("REPORT of area db") && request.contains("db-followup1-F1"));
    assert!(request.contains("REPORT of area api") && request.contains("I looked around."));
    assert!(!request.contains("Not covered"), "{request}");
    let outcome = outcome_with(
        &FakeHost::new(vec![
            planned(),
            turn(
                TurnState::NeedsAttention,
                vec![
                    ("worker-auth", Node::Answer(worker_answer(&[], &[]))),
                    (
                        "worker-db",
                        Node::Fail(FailureClass::ProviderFailure, "connection reset"),
                    ),
                    ("worker-api", Node::Answer(worker_answer(&[], &[]))),
                ],
            ),
            turn(
                TurnState::Completed,
                vec![("worker-db", Node::Answer(worker_answer(&[], &[])))],
            ),
            integrated(&[]),
        ]),
        &run,
    )
    .await
    .unwrap();
    // A failure the follow-up made good leaves nothing not covered.
    assert_eq!(
        outcome.exit_code,
        exit_code::PASS,
        "{:?} {:?}",
        outcome.attention,
        outcome.not_covered
    );

    // A provider that refuses the request itself would refuse a follow-up
    // too: none runs, and the worker's files are listed.
    let host = FakeHost::new(vec![
        planned(),
        turn(
            TurnState::NeedsAttention,
            vec![
                ("worker-auth", Node::Answer(worker_answer(&[], &[]))),
                (
                    "worker-db",
                    Node::Fail(FailureClass::ProviderRejected, "HTTP 401"),
                ),
                ("worker-api", Node::Answer(worker_answer(&[], &[]))),
            ],
        ),
        integrated(&[]),
    ]);
    let (report, _) = drive(&host, &run).await;
    assert_eq!(
        purposes(&report),
        [PLAN_PURPOSE, AREAS_PURPOSE, INTEGRATE_PURPOSE]
    );
    assert_eq!(
        entries(&report.not_covered),
        [
            (
                "db",
                FailureClass::ProviderRejected,
                "the area worker has no result: HTTP 401"
            ),
            (
                "db",
                FailureClass::ProviderRejected,
                "src/db/mod.rs: not read (the area worker has no result)"
            ),
        ]
    );
    assert!(report
        .not_covered
        .iter()
        .all(|entry| entry.turn_id.as_deref() == Some("turn-2")
            && entry.node_id.as_deref() == Some("turn-2-node-1")));
    let outcome = outcome_of(&report);
    assert_eq!(outcome.attention, ["1 area was not covered"]);
}

/// Empty and binary files need no read and count as examined; a file over
/// the size limit is a note, not a gap; a file longer than one read is
/// read across calls, and the windows of every activation count.
#[tokio::test]
async fn empty_binary_too_large_and_long_files() {
    let window = files::READ_WINDOW_BYTES as usize;
    let long = "-- row\n".repeat(200 * 1024 / 7);
    let huge = "x".repeat(files::MAX_AUDITED_FILE_BYTES as usize + 1);
    let repo = repository(&[
        ("src/auth/mod.rs", "pub fn login() {}\n"),
        ("src/auth/__init__.py", ""),
        ("src/auth/logo.png", "\u{89}PNG\r\n\u{1a}\n\0\0\0\rIHDR"),
        ("src/auth/fixtures.json", &huge),
        ("src/db/mod.rs", "pub fn query() {}\n"),
        ("src/db/schema.sql", &long),
        ("src/api/mod.rs", "pub fn route() {}\n"),
    ]);
    let run = context_in(later(), repo.path());
    let read_at = |offsets: &[usize]| -> Vec<Call> {
        offsets
            .iter()
            .map(|offset| {
                (
                    "read_file",
                    serde_json::json!({"path": "src/db/schema.sql", "offset": offset}),
                    true,
                )
            })
            .collect()
    };
    let mut first = reads(&["src/db/mod.rs"]);
    first.extend(read_at(&[0, window]));
    let host = FakeHost::new(vec![
        planned(),
        turn(
            TurnState::Completed,
            vec![
                ("worker-auth", Node::Answer(worker_answer(&[], &[]))),
                ("worker-db", Node::Answer(worker_answer(&[], &[]))),
                ("worker-api", Node::Answer(worker_answer(&[], &[]))),
            ],
        ),
        turn(
            TurnState::Completed,
            vec![("worker-db", Node::Answer(worker_answer(&[], &[])))],
        ),
        integrated(&[]),
    ])
    .calls("worker-db", first)
    .calls("worker-db", read_at(&[2 * window, 3 * window]));
    let (report, calls) = drive(&host, &run).await;

    // The auth worker is told only the file it must read.
    let auth = calls[1].0[0].instructions.as_deref().unwrap();
    assert!(
        auth.contains(
            "gave your area 1 file to read. Read every one of them:\n- src/auth/mod.rs\n(3 more \
             files of your area are empty, binary or over 256 KiB and need no read.)"
        ),
        "{auth}"
    );
    let db = calls[1].0[1].instructions.as_deref().unwrap();
    assert!(db.contains(
        "when the result says truncated, read on with more calls at each result's next_offset"
    ));
    // The long file was half read in the db worker's turn; its follow-up
    // read the rest.
    assert_eq!(
        purposes(&report),
        [
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            FOLLOW_UP_PURPOSE,
            INTEGRATE_PURPOSE
        ]
    );
    assert!(calls[2].0[0]
        .instructions
        .as_deref()
        .unwrap()
        .contains("format:\n- src/db/schema.sql\n"));
    assert!(report.not_covered.is_empty(), "{:?}", report.not_covered);
    assert_eq!(
        host.phases("assigned")[1],
        "auth (paths src/auth/**): src/auth/__init__.py (empty), src/auth/fixtures.json (too \
         large), src/auth/logo.png (binary), src/auth/mod.rs"
    );
    assert_eq!(
        host.phases("coverage")[..2],
        [
            "auth: 3 of 4 files examined: 1 read; not read: empty: src/auth/__init__.py; not \
             read: binary: src/auth/logo.png; not read: too large: src/auth/fixtures.json",
            "db: 2 of 2 files examined: 2 read",
        ]
    );
    assert_eq!(
        report.notes,
        [format!(
            "src/auth/fixtures.json ({} bytes) of area auth was not read: too large (over 256 \
             KiB, the most a worker is asked to read); a note, not a gap",
            huge.len()
        )]
    );
    assert_eq!(outcome_of(&report).exit_code, exit_code::PASS);
}

/// The plan as executed: files no area's paths match go to the area that
/// shares their directories, or to the host-made area rest, which gets its
/// own worker; a planned area left without files is not run. All of it is
/// recorded.
#[tokio::test]
async fn the_plan_as_executed_adds_rest_and_drops_areas_without_files() {
    let repo = repository(&[
        ("src/auth/mod.rs", "pub fn login() {}\n"),
        ("src/db/mod.rs", "pub fn query() {}\n"),
        ("src/main.rs", "fn main() {}\n"),
        ("README.md", "# fixture\n"),
        ("scripts/deploy.sh", "#!/bin/sh\n"),
    ]);
    let run = context_in(later(), repo.path());
    let host = FakeHost::new(vec![
        planned(),
        turn(
            TurnState::Completed,
            vec![
                ("worker-auth", Node::Answer(worker_answer(&[], &[]))),
                ("worker-db", Node::Answer(worker_answer(&[], &[]))),
                (
                    "worker-rest",
                    Node::Answer(worker_answer(
                        &[("deploy script uses curl | sh", "scripts/deploy.sh:2")],
                        &[],
                    )),
                ),
            ],
        ),
        integrated(&[("deploy script uses curl | sh", "rest")]),
    ])
    .calls("worker-auth", reads(&["src/auth/mod.rs", "src/main.rs"]))
    .calls("worker-rest", reads(&["README.md", "./scripts/deploy.sh"]));
    let (report, calls) = drive(&host, &run).await;

    let slots: Vec<&str> = calls[1]
        .0
        .iter()
        .map(|slot| slot.slot_id.as_str())
        .collect();
    assert_eq!(slots, ["worker-auth", "worker-db", "worker-rest"]);
    let rest = calls[1].0[2].instructions.as_deref().unwrap();
    assert!(
        rest.contains("Your area: rest")
            && rest.contains("The host made this area for the files no planned area's paths name.")
            && rest.contains("Read every one of them:\n- README.md\n- scripts/deploy.sh\n")
            && rest.contains("(auth, db)"),
        "{rest}"
    );
    let auth = calls[1].0[0].instructions.as_deref().unwrap();
    assert!(
        auth.contains("- src/auth/mod.rs\n- src/main.rs\n"),
        "{auth}"
    );
    assert_eq!(
        host.phases("assigned"),
        [
            "5 files listed by a walk of the directory (not a Git work tree) in 3 areas: auth \
             2, db 1, rest 2 (host-made)",
            "auth (paths src/auth/**): src/auth/mod.rs, src/main.rs",
            "db (paths src/db/**): src/db/mod.rs",
            "rest (host-made for the files no planned area's paths name): README.md, \
             scripts/deploy.sh",
        ]
    );
    assert_eq!(
        report.notes,
        [
            "1 file no area's paths match was assigned to auth, whose paths share its \
             directories: src/main.rs",
            "the host made area rest for 2 files no planned area's paths name and that share \
             no directory with them: README.md, scripts/deploy.sh",
            "planned area api got no file (no file its paths name is left to it), so it was \
             not run",
        ]
    );
    assert!(host.sent()[1].contains("3 areas of the plan as executed in parallel: auth, db, rest"));
    assert!(host.sent()[2].contains("Areas audited: auth, db, rest."));
    assert!(report.not_covered.is_empty(), "{:?}", report.not_covered);
    assert_eq!(report.findings[0].area.as_deref(), Some("rest"));
    assert_eq!(outcome_of(&report).exit_code, exit_code::PASS);
}

/// The 1.3.0 re-smoke's fixture repository (`repo1`): four modules, an
/// empty `__init__.py` in three, and two files no module holds
/// (`README.md`, `tests/test_billing.py`). Not a Git work tree here, so
/// the host walks it, skipping `.git`.
fn smoke_repository() -> tempfile::TempDir {
    let repo = repository(&[
        ("README.md", "# fixture\n"),
        ("auth/__init__.py", ""),
        ("auth/tokens.py", "def is_valid(token): ...\n"),
        ("billing/__init__.py", ""),
        ("billing/pagination.py", "def get_page(items, page): ...\n"),
        ("ingest/feed.go", "package ingest\n"),
        ("ingest/go.mod", "module ingest\n"),
        ("notify/__init__.py", ""),
        ("notify/webhook.py", "TOKEN = 'x'\n"),
        ("tests/test_billing.py", "def test_page(): ...\n"),
    ]);
    std::fs::create_dir_all(repo.path().join(".git/objects")).unwrap();
    std::fs::write(repo.path().join(".git/objects/lib.rs"), "").unwrap();
    repo
}

/// A recorded answer or tool-call log of the 1.3.0 re-smokes.
fn fixture(name: &str) -> String {
    let path = format!(
        "{}/../axocoatl-session/tests/fixtures/answers/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// A recorded run's areas turn: each worker slot answering as recorded.
fn recorded_areas(run: &str, slots: &[&str]) -> Scripted {
    let answers: Vec<(String, Node)> = slots
        .iter()
        .map(|slot| {
            (
                (*slot).to_owned(),
                Node::Answer(fixture(&format!("{run}-{slot}.txt"))),
            )
        })
        .collect();
    Scripted {
        state: TurnState::Completed,
        nodes: answers,
        until_deadline: false,
        usage: None,
    }
}

fn answered(slot: &str, answer: String) -> (String, Node) {
    (slot.to_owned(), Node::Answer(answer))
}

fn turn_of(state: TurnState, nodes: Vec<(String, Node)>) -> Scripted {
    Scripted {
        state,
        nodes,
        until_deadline: false,
        usage: None,
    }
}

/// The 1.3.0 rc5 re-smoke's run 1 (`resmoke5-audit/out1`), with every
/// answer and every worker's tool calls as recorded. It needed attention
/// for two "areas": the auth worker listed the empty `auth/__init__.py` as
/// not reached, and the notify worker listed `tests`. Now the host checks
/// coverage itself: the empty files need no read, what the workers listed
/// are notes, and `README.md` and `tests/test_billing.py`, which no
/// planned area holds, go to the host-made area rest, whose worker reads
/// them (the billing worker read them too, outside its area: that does not
/// cover rest's files). The run passes.
#[tokio::test]
async fn resmoke5_run_1_passes_with_empty_files_covered_and_the_unassigned_files_in_rest() {
    let repo = smoke_repository();
    let run = context_in(later(), repo.path());
    let workers = [
        "worker-auth",
        "worker-billing",
        "worker-ingest",
        "worker-notify",
    ];
    let mut areas = recorded_areas("audit-resmoke5-out1", &workers);
    areas
        .nodes
        .push(answered("worker-rest", worker_answer(&[], &[])));
    let host = FakeHost::new(vec![
        turn(
            TurnState::Completed,
            vec![(
                PLANNER_SLOT,
                Node::Answer(fixture("audit-resmoke5-out1-planner.txt")),
            )],
        ),
        areas,
        turn(
            TurnState::Completed,
            vec![(
                INTEGRATOR_SLOT,
                Node::Answer(fixture("audit-resmoke5-out1-integrator.txt")),
            )],
        ),
    ])
    .recorded_calls(&fixture("audit-resmoke5-out1-worker-calls.jsonl"))
    .calls(
        "worker-rest",
        reads(&["README.md", "tests/test_billing.py"]),
    );
    {
        let calls = host.calls.lock().unwrap();
        assert_eq!(
            calls.values().map(|queue| queue[0].len()).sum::<usize>(),
            45
        );
    }
    let (report, calls) = drive(&host, &run).await;

    let slots: Vec<&str> = calls[1]
        .0
        .iter()
        .map(|slot| slot.slot_id.as_str())
        .collect();
    assert_eq!(
        slots,
        [
            "worker-auth",
            "worker-billing",
            "worker-ingest",
            "worker-notify",
            "worker-rest"
        ]
    );
    assert_eq!(
        host.phases("assigned"),
        [
            "10 files listed by a walk of the directory (not a Git work tree) in 5 areas: auth \
             2, billing 2, ingest 2, notify 2, rest 2 (host-made)",
            "auth (paths auth/**): auth/__init__.py (empty), auth/tokens.py",
            "billing (paths billing/**): billing/__init__.py (empty), billing/pagination.py",
            "ingest (paths ingest/**): ingest/feed.go, ingest/go.mod",
            "notify (paths notify/**): notify/__init__.py (empty), notify/webhook.py",
            "rest (host-made for the files no planned area's paths name): README.md, \
             tests/test_billing.py",
        ]
    );
    assert_eq!(
        host.phases("coverage"),
        [
            "auth: 2 of 2 files examined: 1 read; not read: empty: auth/__init__.py",
            "billing: 2 of 2 files examined: 1 read; not read: empty: billing/__init__.py",
            "ingest: 2 of 2 files examined: 2 read",
            "notify: 2 of 2 files examined: 1 read; not read: empty: notify/__init__.py",
            "rest: 2 of 2 files examined: 2 read",
        ]
    );
    let note = |slot: &str, items: &str| {
        format!(
            "{slot} listed as not reached: {items}; a note: the host decides coverage from the \
             files its workers read"
        )
    };
    assert_eq!(
        report.notes,
        [
            "the host made area rest for 2 files no planned area's paths name and that share no \
             directory with them: README.md, tests/test_billing.py"
                .to_owned(),
            note("worker-auth", "auth/__init__.py"),
            note("worker-billing", "auth; ingest; notify"),
            note("worker-notify", "auth; billing; ingest; tests"),
        ]
    );
    assert!(report.not_covered.is_empty(), "{:?}", report.not_covered);
    assert_eq!(
        purposes(&report),
        [PLAN_PURPOSE, AREAS_PURPOSE, INTEGRATE_PURPOSE]
    );
    assert_eq!(report.findings.len(), 7);
    let outcome = outcome_of(&report);
    assert_eq!(
        outcome.exit_code,
        exit_code::PASS,
        "{:?}",
        outcome.attention
    );
}

/// The 1.3.0 rc4 re-smoke's run 1 (`resmoke4-audit/out1`), with every
/// answer and every worker's tool calls as recorded. Its planner named one
/// file per area, so the empty `__init__.py` files and `ingest/go.mod`
/// matched no area's paths: the host assigns each to the area sharing its
/// directory. The ingest worker never read `go.mod`, so its follow-up names
/// exactly that file and reads it; the billing worker's not-reached
/// `billing/__init__.py` (empty) is a note. The run needed attention; now
/// it passes.
#[tokio::test]
async fn resmoke4_run_1_assigns_unmatched_files_by_directory_and_follows_up_go_mod() {
    let repo = smoke_repository();
    let run = context_in(later(), repo.path());
    let workers = [
        "worker-auth-tokens",
        "worker-billing-pagination",
        "worker-ingest-feed",
        "worker-notify-webhook",
    ];
    let mut areas = recorded_areas("audit-resmoke4-out1", &workers);
    areas
        .nodes
        .push(answered("worker-rest", worker_answer(&[], &[])));
    let host = FakeHost::new(vec![
        turn(
            TurnState::Completed,
            vec![(
                PLANNER_SLOT,
                Node::Answer(fixture("audit-resmoke4-out1-planner.txt")),
            )],
        ),
        areas,
        turn_of(
            TurnState::Completed,
            vec![answered(
                "worker-ingest-feed",
                "FINDINGS\n```json\n[]\n```\nNOT_REACHED\n```json\n[]\n```".into(),
            )],
        ),
        turn(
            TurnState::Completed,
            vec![(
                INTEGRATOR_SLOT,
                Node::Answer(fixture("audit-resmoke4-out1-integrator.txt")),
            )],
        ),
    ])
    .recorded_calls(&fixture("audit-resmoke4-out1-worker-calls.jsonl"))
    .calls(
        "worker-rest",
        reads(&["README.md", "tests/test_billing.py"]),
    )
    .calls("worker-ingest-feed", reads(&["ingest/go.mod"]));
    let (report, calls) = drive(&host, &run).await;

    assert_eq!(
        host.phases("assigned")[1..5],
        [
            "auth-tokens (paths auth/tokens.py): auth/__init__.py (empty), auth/tokens.py",
            "billing-pagination (paths billing/pagination.py): billing/__init__.py (empty), \
             billing/pagination.py",
            "ingest-feed (paths ingest/feed.go): ingest/feed.go, ingest/go.mod",
            "notify-webhook (paths notify/webhook.py): notify/__init__.py (empty), \
             notify/webhook.py",
        ]
    );
    assert_eq!(
        report.notes[..5],
        [
            "1 file no area's paths match was assigned to auth-tokens, whose paths share its \
             directories: auth/__init__.py",
            "1 file no area's paths match was assigned to billing-pagination, whose paths \
             share its directories: billing/__init__.py",
            "1 file no area's paths match was assigned to ingest-feed, whose paths share its \
             directories: ingest/go.mod",
            "1 file no area's paths match was assigned to notify-webhook, whose paths share \
             its directories: notify/__init__.py",
            "the host made area rest for 2 files no planned area's paths name and that share \
             no directory with them: README.md, tests/test_billing.py",
        ]
    );
    assert_eq!(
        report.notes[5..],
        ["worker-billing-pagination listed as not reached: billing/__init__.py; a note: the host \
          decides coverage from the files its workers read"]
    );
    // One follow-up, of the ingest worker alone, naming exactly go.mod.
    assert_eq!(
        purposes(&report),
        [
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            FOLLOW_UP_PURPOSE,
            INTEGRATE_PURPOSE
        ]
    );
    let follow_up: Vec<&str> = calls[2]
        .0
        .iter()
        .map(|slot| slot.slot_id.as_str())
        .collect();
    assert_eq!(follow_up, ["worker-ingest-feed"]);
    assert!(calls[2].0[0]
        .instructions
        .as_deref()
        .unwrap()
        .contains("report additional findings in the same format:\n- ingest/go.mod\nYou are"));
    assert!(report.not_covered.is_empty(), "{:?}", report.not_covered);
    let outcome = outcome_of(&report);
    assert_eq!(
        outcome.exit_code,
        exit_code::PASS,
        "{:?}",
        outcome.attention
    );
}

/// The 1.3.0 rc5 re-smoke's run 6 (`resmoke5-audit/out6`), with every
/// answer and every worker's tool calls as recorded: the auth worker read
/// its file; the billing worker only grepped one file; the ingest worker
/// listed, globbed and grepped without a match; the notify worker's one
/// read failed. rc5 counted billing as examined from its grep. Now only
/// `read_file` counts: billing, ingest and notify each get a follow-up
/// naming their unread files, and once they read them the run passes,
/// with the planted `ingest/feed.go:29` defect found in the follow-up.
#[tokio::test]
async fn resmoke5_run_6_greps_and_listings_read_nothing_until_a_follow_up_reads() {
    let repo = smoke_repository();
    let run = context_in(later(), repo.path());
    let workers = [
        "worker-auth",
        "worker-billing",
        "worker-ingest",
        "worker-notify",
    ];
    let mut areas = recorded_areas("audit-resmoke5-out6", &workers);
    areas
        .nodes
        .push(answered("worker-rest", worker_answer(&[], &[])));
    let host = FakeHost::new(vec![
        turn(
            TurnState::Completed,
            vec![(
                PLANNER_SLOT,
                Node::Answer(fixture("audit-resmoke5-out6-planner.txt")),
            )],
        ),
        areas,
        turn_of(
            TurnState::Completed,
            vec![
                answered("worker-billing", worker_answer(&[], &[])),
                answered(
                    "worker-ingest",
                    worker_answer(
                        &[("json.Unmarshal error ignored", "ingest/feed.go:29")],
                        &[],
                    ),
                ),
                answered("worker-notify", worker_answer(&[], &[])),
            ],
        ),
        integrated(&[("json.Unmarshal error ignored", "ingest")]),
    ])
    .recorded_calls(&fixture("audit-resmoke5-out6-worker-calls.jsonl"))
    .calls(
        "worker-rest",
        reads(&["README.md", "tests/test_billing.py"]),
    )
    .calls("worker-billing", reads(&["billing/pagination.py"]))
    .calls("worker-ingest", reads(&["ingest/feed.go", "ingest/go.mod"]))
    .calls("worker-notify", reads(&["notify/webhook.py"]));
    let (report, calls) = drive(&host, &run).await;

    assert_eq!(
        purposes(&report),
        [
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            FOLLOW_UP_PURPOSE,
            INTEGRATE_PURPOSE
        ]
    );
    let named: Vec<(&str, &str)> = calls[2]
        .0
        .iter()
        .map(|slot| {
            let instructions = slot.instructions.as_deref().unwrap();
            let list = &instructions[instructions.find("format:\n").unwrap() + 8
                ..instructions.find("You are read-only").unwrap()];
            (slot.slot_id.as_str(), list)
        })
        .collect();
    assert_eq!(
        named,
        [
            ("worker-billing", "- billing/pagination.py\n"),
            ("worker-ingest", "- ingest/feed.go\n- ingest/go.mod\n"),
            ("worker-notify", "- notify/webhook.py\n"),
        ]
    );
    assert!(host.sent()[2].contains("Follow-up 1 of the audit's areas (billing, ingest, notify)"));
    assert!(report.not_covered.is_empty(), "{:?}", report.not_covered);
    let request = &host.sent()[3];
    assert!(request.contains("ingest-followup1-F1"), "{request}");
    assert_eq!(outcome_of(&report).exit_code, exit_code::PASS);
}

/// The 1.3.0 rc4 re-smoke's run 5 (`resmoke4-audit/out5`), with every
/// answer and every worker's tool calls as recorded: the ingest worker's
/// five reads all failed (it guessed paths), its listing succeeded and its
/// greps matched nothing; rc4 counted ingest as covered and missed the
/// defect at `ingest/feed.go:29`. Now its follow-ups name its two files:
/// the first follow-up reads `feed.go` and reports the defect, so it gets
/// another; the second reads nothing new, so `go.mod` is not covered, by
/// file, and no third follows.
#[tokio::test]
async fn resmoke4_run_5_follows_up_a_worker_whose_reads_all_failed() {
    let repo = smoke_repository();
    let run = context_in(later(), repo.path());
    let workers = [
        "worker-auth",
        "worker-billing",
        "worker-ingest",
        "worker-notify",
    ];
    let mut areas = recorded_areas("audit-resmoke4-out5", &workers);
    areas
        .nodes
        .push(answered("worker-rest", worker_answer(&[], &[])));
    let ingest = |answer: String| {
        turn(
            TurnState::Completed,
            vec![("worker-ingest", Node::Answer(answer))],
        )
    };
    let host = FakeHost::new(vec![
        turn(
            TurnState::Completed,
            vec![(
                PLANNER_SLOT,
                Node::Answer(fixture("audit-resmoke4-out5-planner.txt")),
            )],
        ),
        areas,
        ingest(worker_answer(
            &[("Unchecked Unmarshal", "ingest/feed.go:29")],
            &["ingest/go.mod"],
        )),
        ingest(worker_answer(&[], &["ingest/go.mod (a module file)"])),
        turn(
            TurnState::Completed,
            vec![(
                INTEGRATOR_SLOT,
                Node::Answer(fixture("audit-resmoke4-out5-integrator.txt")),
            )],
        ),
    ])
    .recorded_calls(&fixture("audit-resmoke4-out5-worker-calls.jsonl"))
    .calls(
        "worker-rest",
        reads(&["README.md", "tests/test_billing.py"]),
    )
    .calls("worker-ingest", reads(&["ingest/feed.go"]))
    .calls(
        "worker-ingest",
        vec![(
            "grep",
            serde_json::json!({"pattern": "module", "path": "ingest"}),
            true,
        )],
    );
    {
        let calls = host.calls.lock().unwrap();
        let ingest = &calls["worker-ingest"][0];
        assert_eq!(ingest.len(), 21);
        // Its listing of ingest succeeded and every read failed.
        assert!(ingest
            .iter()
            .any(|(tool, arguments, succeeded, _)| tool == "list_dir"
                && arguments["path"] == "ingest"
                && *succeeded));
        assert!(ingest
            .iter()
            .filter(|(tool, ..)| tool == "read_file")
            .all(|(_, _, succeeded, _)| !succeeded));
    }
    let (report, calls) = drive(&host, &run).await;

    assert_eq!(
        purposes(&report),
        [
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            FOLLOW_UP_PURPOSE,
            FOLLOW_UP_PURPOSE,
            INTEGRATE_PURPOSE
        ]
    );
    assert!(calls[2].0[0]
        .instructions
        .as_deref()
        .unwrap()
        .contains("format:\n- ingest/feed.go\n- ingest/go.mod\n"));
    assert!(calls[3].0[0]
        .instructions
        .as_deref()
        .unwrap()
        .contains("format:\n- ingest/go.mod\n"));
    assert_eq!(
        entries(&report.not_covered),
        [(
            "ingest",
            FailureClass::NotReached,
            "ingest/go.mod: not read (the area worker did not read it to its end in its turn or \
             its 2 follow-ups, the last of which read nothing new)"
        )]
    );
    assert_eq!(
        host.phases("coverage"),
        [
            "auth: 2 of 2 files examined: 1 read; not read: empty: auth/__init__.py",
            "billing: 2 of 2 files examined: 1 read; not read: empty: billing/__init__.py",
            "ingest: 1 of 2 files examined: 1 read; not read, not covered: ingest/go.mod",
            "notify: 2 of 2 files examined: 1 read; not read: empty: notify/__init__.py",
            "rest: 2 of 2 files examined: 2 read",
        ]
    );
    // Every worker's own not-reached list, invented paths included, is a
    // note; none is a gap.
    let notes: Vec<&String> = report
        .notes
        .iter()
        .filter(|note| note.contains("listed as not reached"))
        .collect();
    assert_eq!(notes.len(), 6, "{notes:#?}");
    assert!(notes[2].starts_with(
        "worker-ingest listed as not reached: auth; billing; notify; ingest/utils.py; \
         ingest/models.py; ingest/legacy/old_handler.py;"
    ));
    let request = &host.sent()[4];
    assert!(
        request.contains("ingest-followup1-F1") && request.contains("Unchecked Unmarshal"),
        "{request}"
    );
    let outcome = outcome_of(&report);
    assert_eq!(outcome.exit_code, exit_code::NEEDS_ATTENTION);
    assert_eq!(outcome.attention, ["1 area was not covered"]);
}

/// The 1.3.0 re-smoke's run 4 (`resmoke3`), with every answer as recorded:
/// the billing worker made no tool call and listed six paths that do not
/// exist, which rc3 took as notes and passed with billing never read. Now
/// its follow-up names billing's file; a follow-up that reads nothing new
/// gets no other and leaves it not covered, and the invented paths are only
/// its note.
#[tokio::test]
async fn a_worker_that_makes_no_tool_call_is_followed_up() {
    let answer = |name: &str| fixture(&format!("audit-resmoke3-out4-{name}.txt"));
    let repo = smoke_repository();
    let run = context_in(later(), repo.path());
    let billing = || {
        turn(
            TurnState::Completed,
            vec![(
                "worker-billing-module",
                Node::Answer(answer("worker-billing-module")),
            )],
        )
    };
    let host = FakeHost::new(vec![
        turn(
            TurnState::Completed,
            vec![(PLANNER_SLOT, Node::Answer(answer("planner")))],
        ),
        turn_of(
            TurnState::Completed,
            vec![
                answered("worker-auth-module", answer("worker-auth-module")),
                answered("worker-billing-module", answer("worker-billing-module")),
                answered("worker-ingest-module", answer("worker-ingest-module")),
                answered("worker-notify-module", answer("worker-notify-module")),
                answered("worker-rest", worker_answer(&[], &[])),
            ],
        ),
        billing(),
        turn(
            TurnState::Completed,
            vec![(INTEGRATOR_SLOT, Node::Answer(answer("integrator")))],
        ),
    ])
    .calls("worker-auth-module", reads(&["auth/tokens.py"]))
    .calls("worker-billing-module", Vec::new())
    .calls("worker-billing-module", Vec::new())
    .calls(
        "worker-ingest-module",
        reads(&["ingest/feed.go", "ingest/go.mod"]),
    )
    .calls("worker-notify-module", reads(&["notify/webhook.py"]))
    .calls(
        "worker-rest",
        reads(&["README.md", "tests/test_billing.py"]),
    );
    let (report, _) = drive(&host, &run).await;
    assert_eq!(
        entries(&report.not_covered),
        [(
            "billing-module",
            FailureClass::NotReached,
            "billing/pagination.py: not read (the area worker did not read it to its end in its \
             turn or its follow-up, which read nothing new)"
        )]
    );
    assert_eq!(
        report
            .notes
            .iter()
            .filter(|note| note.starts_with(
                "worker-billing-module listed as not reached: \
                                            billing-module/legacy/old_handler.py; "
            ))
            .count(),
        2,
        "{:#?}",
        report.notes
    );
    let outcome = outcome_of(&report);
    assert_eq!(outcome.attention, ["1 area was not covered"]);
}

/// The 1.3.0 re-smoke's run 7, whose workers could only list and glob: a
/// worker without `read_file` reads nothing, so no follow-up can help; each
/// of its files is not covered, saying why.
#[tokio::test]
async fn a_worker_without_read_file_reads_nothing() {
    let answer = |name: &str| fixture(&format!("audit-resmoke3-out7-{name}.txt"));
    let repo = smoke_repository();
    let mut run = context_in(later(), repo.path());
    // The loadout's workers have only list_dir and glob.
    for agent in &mut run.resolved.loadout.file.agents {
        if agent.role == LoadoutRole::Worker {
            agent.tools = vec!["list_dir".into(), "glob".into()];
        }
    }
    let host = FakeHost::new(vec![
        turn(
            TurnState::Completed,
            vec![(PLANNER_SLOT, Node::Answer(answer("planner")))],
        ),
        turn_of(
            TurnState::NeedsAttention,
            vec![
                answered("worker-auth", answer("worker-auth")),
                (
                    "worker-billing".into(),
                    Node::Fail(FailureClass::ProviderFailure, TOO_MANY_TOOL_CALLS),
                ),
                (
                    "worker-ingest".into(),
                    Node::Fail(FailureClass::ProviderFailure, TOO_MANY_TOOL_CALLS),
                ),
                answered("worker-notify", answer("worker-notify")),
                answered("worker-rest", worker_answer(&[], &[])),
            ],
        ),
        integrated(&[("Potential hardcoded webhook URL", "notify")]),
    ])
    .calls(
        "worker-auth",
        vec![
            ("list_dir", serde_json::json!({"path": "auth"}), true),
            ("glob", serde_json::json!({"pattern": "auth/**/*"}), true),
        ],
    )
    .calls(
        "worker-notify",
        vec![
            ("glob", serde_json::json!({"pattern": "notify/**/*"}), true),
            ("list_dir", serde_json::json!({"path": "notify"}), true),
        ],
    )
    .calls("worker-rest", Vec::new());
    let (report, _) = drive(&host, &run).await;
    assert_eq!(
        purposes(&report),
        [PLAN_PURPOSE, AREAS_PURPOSE, INTEGRATE_PURPOSE],
        "no follow-up can read without read_file"
    );
    let no_reader = "(the worker Agent has no read_file tool)";
    let shown: Vec<(&str, String)> = report
        .not_covered
        .iter()
        .map(|entry| (entry.area.as_str(), entry.detail.clone()))
        .collect();
    assert_eq!(
        shown,
        [
            ("auth", format!("auth/tokens.py: not read {no_reader}")),
            (
                "billing",
                format!("the area worker has no result: {TOO_MANY_TOOL_CALLS}")
            ),
            (
                "billing",
                "billing/pagination.py: not read (the area worker has no result)".into()
            ),
            (
                "ingest",
                format!("the area worker has no result: {TOO_MANY_TOOL_CALLS}")
            ),
            (
                "ingest",
                "ingest/feed.go: not read (the area worker has no result)".into()
            ),
            (
                "ingest",
                "ingest/go.mod: not read (the area worker has no result)".into()
            ),
            ("notify", format!("notify/webhook.py: not read {no_reader}")),
            ("rest", format!("README.md: not read {no_reader}")),
            (
                "rest",
                format!("tests/test_billing.py: not read {no_reader}")
            ),
        ]
    );
    let outcome = outcome_of(&report);
    assert_eq!(outcome.exit_code, exit_code::NEEDS_ATTENTION);
    assert_eq!(outcome.attention, ["5 areas were not covered"]);
}

/// Without a readable record of tool calls, whether a worker read its
/// files is not known, nothing is taken as covered on a guess, and no
/// follow-up runs.
#[tokio::test]
async fn an_unreadable_record_of_tool_calls_covers_nothing() {
    for (record, reason) in [
        (
            Record::None,
            "the Session's record of tool calls could not be read",
        ),
        (
            Record::Fails,
            "the Session's record of tool calls could not be read: the invocation audit could \
             not be read",
        ),
    ] {
        let host =
            FakeHost::new(vec![planned(), workers_answered(), integrated(&[])]).with_record(record);
        let (run, _repo) = context(later());
        let (report, _) = drive(&host, &run).await;
        assert_eq!(
            purposes(&report),
            [PLAN_PURPOSE, AREAS_PURPOSE, INTEGRATE_PURPOSE]
        );
        let detail = format!("whether the area worker read its files is not known: {reason}");
        assert_eq!(
            entries(&report.not_covered),
            [
                ("auth", FailureClass::Other, detail.as_str()),
                ("db", FailureClass::Other, detail.as_str()),
                ("api", FailureClass::Other, detail.as_str()),
            ]
        );
        assert!(host.sent()[2].contains(
            "Whether its worker read this area's files is not known, so the area is not covered"
        ));
    }
}

/// A repository the host cannot list leaves the whole scope not covered:
/// no worker's coverage could be checked, so none runs.
#[tokio::test]
async fn a_repository_the_host_cannot_list_is_not_covered() {
    let gone = tempfile::tempdir().unwrap();
    let run = context_in(later(), &gone.path().join("missing"));
    let host = FakeHost::new(vec![planned()]);
    let (report, calls) = drive(&host, &run).await;
    assert_eq!(calls.len(), 1, "no worker Apply");
    assert_eq!(report.not_covered.len(), 1);
    assert_eq!(report.not_covered[0].area, WHOLE_SCOPE);
    assert!(report.not_covered[0]
        .detail
        .starts_with("the host could not list the repository's files"));
    assert_eq!(
        outcome_of(&report).attention,
        ["The whole scope was not covered"]
    );
}

#[tokio::test]
async fn the_integrate_request_bounds_each_report() {
    let many: Vec<String> = (0..150)
        .map(|n| format!("defect number {n} with a long description of what goes wrong"))
        .collect();
    let many: Vec<(&str, &str)> = many
        .iter()
        .map(|title| (title.as_str(), "src/x.rs:1"))
        .collect();
    let host = FakeHost::new(vec![
        planned(),
        turn(
            TurnState::Completed,
            vec![
                ("worker-auth", Node::Answer(worker_answer(&many, &[]))),
                ("worker-db", Node::Answer(worker_answer(&many[..2], &[]))),
                ("worker-api", Node::Answer(worker_answer(&[], &[]))),
            ],
        ),
        integrated(&[]),
    ]);
    let (run, _repo) = context(later());
    drive(&host, &run).await;
    let request = &host.sent()[2];
    let auth = &request
        [request.find("REPORT of area auth").unwrap()..request.find("REPORT of area db").unwrap()];
    let block = &auth[auth.find("```json\n").unwrap() + 8..auth.rfind("```").unwrap()];
    assert!(block.len() <= MAX_REPORT_BYTES, "{}", block.len());
    assert!(
        auth.contains("(truncated: ") && auth.contains("of 150 findings shown"),
        "{auth}"
    );
    let db = &request
        [request.find("REPORT of area db").unwrap()..request.find("REPORT of area api").unwrap()];
    assert!(!db.contains("truncated"));
    assert!(db.contains("defect number 1 "));
}

/// Findings past a report's bound are counted, and the area is not
/// covered: they never vanish.
#[tokio::test]
async fn findings_left_out_of_a_report_are_not_covered() {
    let many: Vec<String> = (0..MAX_AREA_FINDINGS + 2)
        .map(|n| format!("defect {n}"))
        .collect();
    let many: Vec<(&str, &str)> = many
        .iter()
        .map(|title| (title.as_str(), "src/x.rs:1"))
        .collect();
    let host = FakeHost::new(vec![
        planned(),
        turn(
            TurnState::Completed,
            vec![
                ("worker-auth", Node::Answer(worker_answer(&many, &[]))),
                ("worker-db", Node::Answer(worker_answer(&[], &[]))),
                ("worker-api", Node::Answer(worker_answer(&[], &[]))),
            ],
        ),
        integrated(&[]),
    ]);
    let (run, _repo) = context(later());
    let (report, _) = drive(&host, &run).await;
    assert_eq!(
        entries(&report.not_covered),
        [(
            "auth",
            FailureClass::Other,
            "2 findings beyond the first 200 of a report were left out of it"
        )]
    );
}

#[test]
fn an_unreadable_answer_is_bounded_and_fenced_apart() {
    let answer = format!("```\nnot json\n```\n{}", "y".repeat(MAX_REPORT_BYTES * 2));
    let text = report_text(&AreaBody::Unreadable {
        answer: answer.clone(),
        error: "the answer has no FINDINGS block".into(),
    });
    assert!(text
        .starts_with("The worker's report could not be read (the answer has no FINDINGS block)"));
    assert!(text.contains("````text\n```\nnot json"), "{}", &text[..200]);
    assert!(text.contains(&format!("the answer was {} bytes", answer.len())));
    assert!(text.len() < MAX_REPORT_BYTES + 512);
}

#[tokio::test]
async fn an_invalid_plan_gets_one_retry_quoting_the_error() {
    let host = FakeHost::new(vec![
        turn(
            TurnState::Completed,
            vec![(
                PLANNER_SLOT,
                Node::Answer(plan_answer(&[("everything", "the whole repo", &[])])),
            )],
        ),
        planned(),
        workers_answered(),
        integrated(&[]),
    ]);
    let (run, _repo) = context(later());
    let (report, calls) = drive(&host, &run).await;
    let sent = host.sent();
    assert_eq!(sent.len(), 4);
    assert!(
        sent[1]
            .contains("Your AREAS block could not be used: the plan has 1 area; it needs 2 to 8"),
        "{}",
        sent[1]
    );
    assert!(sent[1].contains("find the defects"));
    // The retry is a turn of the same planner: no second Apply.
    assert_eq!(calls.len(), 3);
    assert_eq!(
        host.log()[..4],
        ["apply", "send:turn-1", "send:turn-2", "apply"]
    );
    assert!(report.not_covered.is_empty());
    assert_eq!(report.turn_refs[1].purpose, PLAN_PURPOSE);
}

#[tokio::test]
async fn an_invalid_plan_twice_needs_attention_with_the_whole_scope_not_covered() {
    let invalid = || {
        turn(
            TurnState::Completed,
            vec![(
                PLANNER_SLOT,
                Node::Answer(plan_answer(&[("auth", "x", &[]), ("auth", "y", &[])])),
            )],
        )
    };
    let host = FakeHost::new(vec![invalid(), invalid()]);
    let (run, _repo) = context(later());
    let (report, calls) = drive(&host, &run).await;
    assert_eq!(host.sent().len(), 2);
    assert_eq!(calls.len(), 1);
    assert_eq!(report.not_covered.len(), 1);
    let entry = &report.not_covered[0];
    assert_eq!(entry.area, WHOLE_SCOPE);
    assert_eq!(entry.class, FailureClass::Other);
    assert!(
        entry.detail.contains("invalid twice") && entry.detail.contains("two areas are named auth")
    );
    assert_eq!(entry.turn_id.as_deref(), Some("turn-2"));
    assert!(report.findings.is_empty());
    let outcome = outcome_of(&report);
    assert_eq!(outcome.exit_code, exit_code::NEEDS_ATTENTION);
    assert!(host.events.lock().unwrap().iter().any(
        |event| matches!(event, RunEvent::NotCovered { entry, .. } if entry.area == WHOLE_SCOPE)
    ));
}

/// A provider that refuses the request itself (400-403) would refuse the
/// retry too.
#[tokio::test]
async fn a_planner_its_provider_rejects_is_not_retried() {
    let host = FakeHost::new(vec![turn(
        TurnState::NeedsAttention,
        vec![(
            PLANNER_SLOT,
            Node::Fail(FailureClass::ProviderRejected, "HTTP 401"),
        )],
    )]);
    let (run, _repo) = context(later());
    let (report, _) = drive(&host, &run).await;
    assert_eq!(host.sent().len(), 1);
    assert_eq!(report.not_covered[0].area, WHOLE_SCOPE);
    assert_eq!(report.not_covered[0].class, FailureClass::ProviderRejected);
    assert_eq!(
        report.not_covered[0].detail,
        "the planner has no answer: HTTP 401"
    );
    // Nothing follows, so the paused turn is left for the person.
    assert!(host.stopped.lock().unwrap().is_empty());
}

/// What ended the 1.3.0 re-smoke's runs 1 and 6 at once: the planner's
/// first model call asked for more native tool calls than Ollama allows.
const TOO_MANY_TOOL_CALLS: &str = "Activation failed: LLM provider error: ollama returned a \
     response that was refused: Streaming error: native Ollama: too many native tool calls";

fn planner_failed() -> Scripted {
    turn(
        TurnState::NeedsAttention,
        vec![(
            PLANNER_SLOT,
            Node::Fail(FailureClass::ProviderFailure, TOO_MANY_TOOL_CALLS),
        )],
    )
}

fn workers_answered() -> Scripted {
    turn(
        TurnState::Completed,
        vec![
            ("worker-auth", Node::Answer(worker_answer(&[], &[]))),
            ("worker-db", Node::Answer(worker_answer(&[], &[]))),
            ("worker-api", Node::Answer(worker_answer(&[], &[]))),
        ],
    )
}

/// A planner without an answer gets the same one retry as an invalid plan:
/// the plan request again, after its paused turn is stopped, with no
/// second Apply.
#[tokio::test]
async fn a_planner_provider_failure_gets_one_retry() {
    let host = FakeHost::new(vec![
        planner_failed(),
        planned(),
        workers_answered(),
        integrated(&[]),
    ]);
    let (run, _repo) = context(later());
    let (report, calls) = drive(&host, &run).await;
    let sent = host.sent();
    assert_eq!(sent.len(), 4);
    assert_eq!(sent[1], sent[0], "the retry is the plan request itself");
    assert!(!sent[1].contains("could not be used"));
    assert_eq!(calls.len(), 3);
    assert_eq!(
        host.log()[..5],
        [
            "apply",
            "send:turn-1",
            "stop:turn-1",
            "send:turn-2",
            "apply"
        ]
    );
    let failed = host.phases("plan_failed");
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0].starts_with("the planner has no answer (provider_failure: Activation failed")
            && failed[0].ends_with("too many native tool calls); it gets one more turn"),
        "{failed:?}"
    );
    assert!(report.not_covered.is_empty(), "{:?}", report.not_covered);
    assert_eq!(
        purposes(&report),
        [PLAN_PURPOSE, PLAN_PURPOSE, AREAS_PURPOSE, INTEGRATE_PURPOSE]
    );
    assert_eq!(outcome_of(&report).exit_code, exit_code::PASS);
    // The retry stands for the failed attempt: the Outcome builder does not
    // list that planner as not covered.
    assert_eq!(
        report.accounted,
        [("turn-1".to_string(), "turn-1-node-0".to_string())]
    );
}

/// The whole run, as the Outcome builder folds it: a planner retry that
/// planned leaves nothing not covered, so the run passes (the live run
/// first listed the failed attempt's planner as not covered).
#[tokio::test]
async fn a_run_whose_planner_retry_planned_passes() {
    let host = FakeHost::new(vec![
        planner_failed(),
        planned(),
        workers_answered(),
        integrated(&[]),
    ]);
    let (run, _repo) = context(later());
    let outcome = outcome_with(&host, &run).await.unwrap();
    assert_eq!(
        outcome.exit_code,
        exit_code::PASS,
        "{:?} {:?}",
        outcome.attention,
        outcome.not_covered
    );
    assert!(outcome.not_covered.is_empty());
    assert_eq!(outcome.turns.len(), 4);

    // Failing twice: the whole scope, and no separate planner entry.
    let host = FakeHost::new(vec![planner_failed(), planner_failed()]);
    let outcome = outcome_with(&host, &run).await.unwrap();
    let areas: Vec<&str> = outcome
        .not_covered
        .iter()
        .map(|entry| entry.area.as_str())
        .collect();
    assert_eq!(areas, [WHOLE_SCOPE]);
    assert_eq!(
        outcome.attention,
        [
            "The whole scope was not covered",
            "A turn ended needing attention"
        ]
    );
}

/// Two attempts without a plan leave the whole scope not covered, and the
/// attention line says so instead of counting it as one area.
#[tokio::test]
async fn a_planner_failing_twice_leaves_the_whole_scope_not_covered() {
    let host = FakeHost::new(vec![planner_failed(), planner_failed()]);
    let (run, _repo) = context(later());
    let (report, calls) = drive(&host, &run).await;
    assert_eq!(host.sent().len(), 2);
    assert_eq!(calls.len(), 1);
    assert_eq!(report.not_covered.len(), 1);
    let entry = &report.not_covered[0];
    assert_eq!(entry.area, WHOLE_SCOPE);
    assert_eq!(entry.class, FailureClass::ProviderFailure);
    assert!(
        entry
            .detail
            .starts_with("the planner has no answer in two attempts: Activation failed")
            && entry
                .detail
                .contains("(the first: provider_failure: Activation failed"),
        "{}",
        entry.detail
    );
    assert_eq!(entry.turn_id.as_deref(), Some("turn-2"));
    let outcome = outcome_of(&report);
    assert_eq!(outcome.exit_code, exit_code::NEEDS_ATTENTION);
    assert_eq!(outcome.attention, ["The whole scope was not covered"]);

    // A failure and then an invalid plan: the retry was the planner's last.
    let host = FakeHost::new(vec![
        planner_failed(),
        turn(
            TurnState::Completed,
            vec![(
                PLANNER_SLOT,
                Node::Answer(plan_answer(&[("everything", "the whole repo", &[])])),
            )],
        ),
    ]);
    let (report, _) = drive(&host, &run).await;
    assert_eq!(host.sent().len(), 2);
    assert_eq!(report.not_covered.len(), 1);
    assert_eq!(report.not_covered[0].class, FailureClass::Other);
    assert!(
        report.not_covered[0]
            .detail
            .starts_with("the planner's AREAS block was invalid after it had no answer"),
        "{}",
        report.not_covered[0].detail
    );

    // An invalid plan and then no answer: no third attempt.
    let host = FakeHost::new(vec![
        turn(
            TurnState::Completed,
            vec![(
                PLANNER_SLOT,
                Node::Answer(plan_answer(&[("everything", "the whole repo", &[])])),
            )],
        ),
        planner_failed(),
    ]);
    let (report, _) = drive(&host, &run).await;
    assert_eq!(host.sent().len(), 2);
    assert!(host.sent()[1].contains("Your AREAS block could not be used"));
    assert_eq!(report.not_covered[0].class, FailureClass::ProviderFailure);
    assert!(
        report.not_covered[0]
            .detail
            .starts_with("the planner has no answer after its AREAS block was refused"),
        "{}",
        report.not_covered[0].detail
    );
}

/// A person's stop is no failure to retry: the run ends interrupted
/// without another planner turn.
#[tokio::test]
async fn a_stopped_run_gets_no_planner_retry() {
    let host = FakeHost::new(vec![planner_failed()]);
    host.stop_on_wait
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (run, _repo) = context(later());
    let builds = Arc::new(Builds::default());
    let build =
        move |resolved: &ResolvedLoadout, slots: &[SlotPlan], checks: bool, revision: u64| {
            builds.build(resolved, slots, checks, revision)
        };
    let report = drive_audit(&host, &run, &build).await.unwrap();
    assert_eq!(host.sent().len(), 1);
    assert!(host.phases("plan_failed").is_empty());
    assert_eq!(report.not_covered.len(), 1);
    assert_eq!(report.not_covered[0].area, WHOLE_SCOPE);
    assert_eq!(report.not_covered[0].class, FailureClass::ProviderFailure);

    // A stop before a turn starts ends the drive: no turn after a stop.
    let host = FakeHost::new(Vec::new());
    host.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let report = drive_audit(&host, &run, &build).await.unwrap();
    assert!(report.stopped);
    assert!(report.turns.is_empty() && report.not_covered.is_empty());
    assert!(host.sent().is_empty());
}

fn usage(input_tokens: u64, output_tokens: u64, complete: bool) -> RunUsage {
    RunUsage {
        input_tokens,
        output_tokens,
        cost_microunits: 0,
        complete,
        cost_known: true,
        cost_computed: false,
        retries: 0,
    }
}

/// The 1.3.0 rc4 re-smoke's run 6: two workers looped until a person
/// stopped the run during the areas turn. Its Outcome had no turns, nothing
/// not covered and 0 tokens marked complete, although two turns ran. Now
/// the Outcome of a stopped run keeps its turns, what was not covered, the
/// findings already reported (unmerged, since integration never ran) and
/// the usage observed, incomplete when a turn's usage is.
#[tokio::test]
async fn a_stopped_audit_keeps_what_it_observed() {
    let mut plan = planned();
    plan.usage = Some(usage(1_200, 80, true));
    let mut areas = turn(
        TurnState::Stopped,
        vec![
            (
                "worker-auth",
                Node::Answer(worker_answer(
                    &[("token compared with ==", "src/auth.rs:42")],
                    &[],
                )),
            ),
            ("worker-db", Node::Stopped),
            ("worker-api", Node::Stopped),
        ],
    );
    // The stopped workers' last calls never reported their usage.
    areas.usage = Some(usage(9_000, 400, false));
    let host = FakeHost::new(vec![plan, areas]);
    *host.stop_on_turn.lock().unwrap() = Some("turn-2".into());
    let (run, _repo) = context(later());
    let outcome = outcome_with(&host, &run).await.unwrap();

    assert_eq!(outcome.verdict, RunVerdict::Interrupted);
    assert_eq!(outcome.exit_code, exit_code::INTERRUPTED);
    // No follow-up or integrator Apply or turn after the stop.
    assert_eq!(host.sent().len(), 2);
    assert_eq!(host.applied.lock().unwrap().len(), 2);
    let turns: Vec<(&str, TurnState)> = outcome
        .turns
        .iter()
        .map(|turn| (turn.purpose.as_str(), turn.state))
        .collect();
    assert_eq!(
        turns,
        [
            (PLAN_PURPOSE, TurnState::Completed),
            (AREAS_PURPOSE, TurnState::Stopped)
        ]
    );
    assert_eq!(
        entries(&outcome.not_covered),
        [
            (
                "db",
                FailureClass::Stopped,
                "the area worker has no result: it was stopped"
            ),
            (
                "db",
                FailureClass::Stopped,
                "src/db/mod.rs: not read (the area worker has no result)"
            ),
            (
                "api",
                FailureClass::Stopped,
                "the area worker has no result: it was stopped"
            ),
            (
                "api",
                FailureClass::Stopped,
                "src/api/mod.rs: not read (the area worker has no result)"
            ),
            (
                INTEGRATION,
                FailureClass::Stopped,
                "the run was stopped before integration; the area workers' findings are \
                 reported unmerged"
            ),
        ]
    );
    let ids: Vec<&str> = outcome.findings.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, ["auth-F1"]);
    assert_eq!(outcome.usage.input_tokens, 10_200);
    assert_eq!(outcome.usage.output_tokens, 480);
    assert!(!outcome.usage.complete);
    assert_eq!(
        outcome.attention,
        [
            "2 areas were not covered",
            "The integration was not read; the area findings are reported unmerged",
            "A turn ended needing attention"
        ]
    );

    // Stopped before the areas started: the areas of the plan as executed
    // are not covered, and the usage of the one turn that ran is complete.
    let mut plan = planned();
    plan.usage = Some(usage(1_200, 80, true));
    let host = FakeHost::new(vec![plan]);
    *host.stop_on_turn.lock().unwrap() = Some("turn-1".into());
    let outcome = outcome_with(&host, &run).await.unwrap();
    assert_eq!(outcome.exit_code, exit_code::INTERRUPTED);
    assert_eq!(host.sent().len(), 1);
    assert_eq!(outcome.turns.len(), 1);
    let before = "the run was stopped before the areas started";
    assert_eq!(
        entries(&outcome.not_covered),
        [
            ("auth", FailureClass::Stopped, before),
            ("db", FailureClass::Stopped, before),
            ("api", FailureClass::Stopped, before),
        ]
    );
    assert_eq!(outcome.usage.input_tokens, 1_200);
    assert!(outcome.usage.complete);

    // Stopped while a worker had files left to read: no follow-up starts,
    // and those files are not covered, stopped.
    let repo = repository(&[
        ("src/auth/mod.rs", "pub fn login() {}\n"),
        ("src/auth/session.rs", "pub fn renew() {}\n"),
        ("src/db/mod.rs", "pub fn query() {}\n"),
        ("src/api/mod.rs", "pub fn route() {}\n"),
    ]);
    let run = context_in(later(), repo.path());
    let host = FakeHost::new(vec![planned(), workers_answered()]);
    *host.stop_on_turn.lock().unwrap() = Some("turn-2".into());
    let outcome = outcome_with(&host, &run).await.unwrap();
    assert_eq!(outcome.exit_code, exit_code::INTERRUPTED);
    assert_eq!(host.sent().len(), 2);
    assert_eq!(
        entries(&outcome.not_covered),
        [
            (
                "auth",
                FailureClass::Stopped,
                "src/auth/session.rs: not read (the run was stopped before a follow-up read it)"
            ),
            (
                INTEGRATION,
                FailureClass::Stopped,
                "the run was stopped before integration; the area workers' findings are \
                 reported unmerged"
            ),
        ]
    );
}

#[tokio::test]
async fn a_failed_integration_reports_the_workers_findings_unmerged() {
    let host = FakeHost::new(vec![
        planned(),
        turn(
            TurnState::Completed,
            vec![
                (
                    "worker-auth",
                    Node::Answer(worker_answer(
                        &[("token compared with ==", "src/auth.rs:42")],
                        &[],
                    )),
                ),
                (
                    "worker-db",
                    Node::Answer(worker_answer(
                        &[("SQL built by format!", "src/db.rs:7")],
                        &[],
                    )),
                ),
                ("worker-api", Node::Answer(worker_answer(&[], &[]))),
            ],
        ),
        turn(
            TurnState::NeedsAttention,
            vec![(
                INTEGRATOR_SLOT,
                Node::Fail(FailureClass::Budget, "the grant's tokens ran out"),
            )],
        ),
    ]);
    let (run, _repo) = context(later());
    let (report, _) = drive(&host, &run).await;
    let ids: Vec<&str> = report.findings.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, ["auth-F1", "db-F1"]);
    assert!(report
        .findings
        .iter()
        .all(|finding| finding.source == FindingSource::AuditWorker));
    assert_eq!(report.not_covered.len(), 1);
    assert_eq!(report.not_covered[0].area, INTEGRATION);
    assert_eq!(report.not_covered[0].class, FailureClass::Budget);
    assert!(report.not_covered[0].detail.contains("reported unmerged"));
    assert!(report.budget_exhausted);
}

#[tokio::test]
async fn the_wall_clock_stops_the_areas_turn_and_skips_follow_ups_and_integration() {
    let mut areas = turn(
        TurnState::Running,
        vec![
            (
                "worker-auth",
                Node::Answer(worker_answer(
                    &[("token compared with ==", "src/auth.rs:42")],
                    &[],
                )),
            ),
            ("worker-db", Node::Running),
            ("worker-api", Node::Running),
        ],
    );
    areas.until_deadline = true;
    let host = FakeHost::new(vec![planned(), areas]);
    let (run, _repo) = context(Instant::now() + Duration::from_millis(400));
    let (report, calls) = drive(&host, &run).await;
    assert_eq!(
        calls.len(),
        2,
        "no follow-up or integrator Apply after the deadline"
    );
    assert_eq!(host.sent().len(), 2);
    assert_eq!(host.log()[4], "stop:turn-2");
    assert!(report.budget_exhausted);
    let budget = "the run's wall clock ran out before it finished";
    assert_eq!(
        entries(&report.not_covered),
        [
            (
                "db",
                FailureClass::Budget,
                &*format!("the area worker has no result: {budget}")
            ),
            (
                "db",
                FailureClass::Budget,
                "src/db/mod.rs: not read (the area worker has no result)"
            ),
            (
                "api",
                FailureClass::Budget,
                &*format!("the area worker has no result: {budget}")
            ),
            (
                "api",
                FailureClass::Budget,
                "src/api/mod.rs: not read (the area worker has no result)"
            ),
            (
                INTEGRATION,
                FailureClass::Budget,
                "the run's wall clock ran out before integration; the area workers' findings \
                 are reported unmerged"
            ),
        ]
    );
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].source, FindingSource::AuditWorker);
    assert_eq!(report.turn_refs[1].state, TurnState::Stopped);
    assert_eq!(outcome_of(&report).exit_code, exit_code::NEEDS_ATTENTION);
}

/// The wall clock running out during a follow-up ends the follow-ups: what
/// is still unread is not covered, as budget.
#[tokio::test]
async fn the_wall_clock_ends_the_follow_ups() {
    let repo = repository(&[
        ("src/auth/mod.rs", "pub fn login() {}\n"),
        ("src/auth/session.rs", "pub fn renew() {}\n"),
        ("src/db/mod.rs", "pub fn query() {}\n"),
        ("src/api/mod.rs", "pub fn route() {}\n"),
    ]);
    let run = context_in(Instant::now() + Duration::from_millis(400), repo.path());
    let mut follow_up = turn(TurnState::Running, vec![("worker-auth", Node::Running)]);
    follow_up.until_deadline = true;
    let host = FakeHost::new(vec![planned(), workers_answered(), follow_up]);
    let (report, _) = drive(&host, &run).await;
    assert_eq!(
        purposes(&report),
        [PLAN_PURPOSE, AREAS_PURPOSE, FOLLOW_UP_PURPOSE]
    );
    assert_eq!(
        entries(&report.not_covered),
        [
            (
                "auth",
                FailureClass::Budget,
                "src/auth/session.rs: not read (the area worker's last follow-up has no result: \
                 the run's wall clock ran out before it finished)"
            ),
            (
                INTEGRATION,
                FailureClass::Budget,
                "the run's wall clock ran out before integration; the area workers' findings \
                 are reported unmerged"
            ),
        ]
    );
    assert_eq!(
        host.phases("follow_up_failed"),
        [
            "worker-auth has no result in follow-up 1 (budget: the run's wall clock ran out \
          before it finished)"
        ]
    );
    assert!(report.budget_exhausted);
}

#[tokio::test]
async fn a_spent_wall_clock_starts_no_turn() {
    let host = FakeHost::new(Vec::new());
    let (run, _repo) = context(Instant::now());
    let (report, calls) = drive(&host, &run).await;
    assert_eq!(calls.len(), 1);
    assert!(host.sent().is_empty());
    assert_eq!(report.not_covered[0].area, WHOLE_SCOPE);
    assert_eq!(report.not_covered[0].class, FailureClass::Budget);
    assert!(report.budget_exhausted);
}

#[test]
fn slot_plans_are_pure_and_read_only() {
    let (run, _repo) = context(later());
    let plan = parse_plan(&three_areas(), 2, 8).unwrap();
    let planner = plan_slots(&run.resolved).unwrap();
    assert_eq!(planner.len(), 1);
    assert_eq!(planner[0].agent.role, LoadoutRole::Planner);
    assert!(planner[0].instructions.is_none());
    let file = |path: &str| files::RepoFile {
        path: path.into(),
        size: 10,
        kind: files::FileKind::Text,
    };
    let assignment = files::assign(
        &plan,
        &[
            file("src/auth/a.rs"),
            file("src/db/b.rs"),
            file("src/api/c.rs"),
        ],
    );
    let workers = area_slots(&run.resolved, &assignment).unwrap();
    assert_eq!(workers.len(), 3);
    assert!(workers
        .iter()
        .all(|slot| slot.agent.tools.contains(&"bash".to_string())
            && !slot
                .agent
                .tools
                .iter()
                .any(|tool| tool == "write_file" || tool == "edit_file")));
    let integrator = integrate_slots(&run.resolved).unwrap();
    assert_eq!(integrator[0].agent.role, LoadoutRole::Integrator);
    assert!(integrator[0].depends_on.is_empty());
    let api = worker_instructions(None, &assignment.areas[2], &["auth", "db"]);
    assert!(api.starts_with("Your area: api"));
    assert!(api.contains("- src/api/c.rs\n"));
    let follow_up = follow_up_slots(
        &run.resolved,
        &[(&assignment.areas[0], vec!["src/auth/a.rs"])],
        2,
    )
    .unwrap();
    assert_eq!(follow_up[0].slot_id, "worker-auth");
    assert!(follow_up[0].agent.writes.as_deref() == Some(&[][..]));
    let instructions = follow_up[0].instructions.as_deref().unwrap();
    assert!(instructions.contains("Audit only your area") && instructions.contains("Follow-up 2"));
    // A list too long for the instructions names directories, with the
    // instruction to read all of them.
    let many: Vec<String> = (0..2000)
        .map(|index| format!("src/api/v{}/h{index}.rs", index % 4))
        .collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let text = follow_up_instructions(None, &assignment.areas[2], &many, 1);
    assert!(
        text.contains(
            "2000 files of your area were not read to their end, too many to name one by one"
        ) && text.contains("- src/api/v0/ and below: 500 files\n"),
        "{text}"
    );
}

#[test]
fn failures_are_classified_from_the_observation() {
    let node = |state: NodeState, failure: Option<NodeFailure>| NodeObservation {
        node_id: "n".into(),
        slot_id: "worker-a".into(),
        model: ModelIdentity {
            provider: "ollama".into(),
            model: "m".into(),
            runtime: "native".into(),
        },
        required: true,
        kind: "slot".into(),
        generations: vec![GenerationObservation {
            generation: 1,
            state,
            answer: None,
            failure,
        }],
    };
    let refused = node(
        NodeState::Failed,
        Some(NodeFailure {
            class: FailureClass::ProviderRefusal,
            message: "refusal".into(),
        }),
    );
    assert_eq!(
        failure_of(Some(&refused), false).0,
        FailureClass::ProviderRefusal
    );
    let stopped = node(
        NodeState::Stopped,
        Some(NodeFailure {
            class: FailureClass::Stopped,
            message: "stopped".into(),
        }),
    );
    assert_eq!(failure_of(Some(&stopped), false).0, FailureClass::Stopped);
    assert_eq!(failure_of(Some(&stopped), true).0, FailureClass::Budget);
    assert_eq!(
        failure_of(Some(&node(NodeState::Blocked, None)), false).0,
        FailureClass::Blocked
    );
    assert_eq!(
        failure_of(Some(&node(NodeState::Running, None)), true).0,
        FailureClass::Budget
    );
    assert_eq!(failure_of(None, false).0, FailureClass::Other);
}

/// The 1.3.0 rc7 re-smoke's run 9 (`resmoke7-audit/out9`), with every
/// answer and every worker's tool calls as recorded, over that run's own
/// files: its scripted ingest worker read `ingest/feed.go` (802 bytes) with
/// `limit: 64` and nothing more, and rc7 counted the file as read because
/// it fits one read, so the run passed without the Unmarshal defect at
/// line 29. Now coverage counts the bytes each read returned: `feed.go` is
/// not read, its follow-up names it, and the follow-up's read of all of it
/// covers it.
#[tokio::test]
async fn resmoke7_run_9_a_partial_read_of_a_small_file_is_followed_up() {
    let feed = format!(
        "// Package ingest loads the nightly order feed from disk.\npackag{}",
        "e ingest // padding to the recorded size\n"
            .repeat(20)
            .chars()
            .take(802 - 64)
            .collect::<String>()
    );
    assert_eq!(feed.len(), 802);
    let sized = |bytes: usize| "x".repeat(bytes);
    let repo = repository(&[
        ("README.md", &sized(253)),
        ("auth/__init__.py", ""),
        ("auth/tokens.py", &sized(671)),
        ("billing/__init__.py", ""),
        ("billing/pagination.py", &sized(811)),
        ("ingest/feed.go", &feed),
        ("ingest/go.mod", &sized(43)),
        ("notify/__init__.py", ""),
        ("notify/webhook.py", &sized(710)),
        ("tests/test_billing.py", &sized(398)),
    ]);
    let run = context_in(later(), repo.path());
    let workers = [
        "worker-auth",
        "worker-billing",
        "worker-ingest",
        "worker-notify",
        "worker-rest",
    ];
    let host = FakeHost::new(vec![
        turn(
            TurnState::Completed,
            vec![(
                PLANNER_SLOT,
                Node::Answer(fixture("audit-resmoke7-out9-planner.txt")),
            )],
        ),
        recorded_areas("audit-resmoke7-out9", &workers),
        turn(
            TurnState::Completed,
            vec![(
                "worker-ingest",
                Node::Answer(worker_answer(
                    &[("Unchecked json.Unmarshal error", "ingest/feed.go:29")],
                    &[],
                )),
            )],
        ),
        turn(
            TurnState::Completed,
            vec![(
                INTEGRATOR_SLOT,
                Node::Answer(fixture("audit-resmoke7-out9-integrator.txt")),
            )],
        ),
    ])
    .recorded_calls(&fixture("audit-resmoke7-out9-worker-calls.jsonl"))
    .calls("worker-ingest", reads(&["ingest/feed.go"]));
    {
        let calls = host.calls.lock().unwrap();
        let ingest = &calls["worker-ingest"][0];
        // As recorded: go.mod whole, and 64 bytes of feed.go.
        assert_eq!(ingest.len(), 2);
        assert_eq!(
            ingest[1].1,
            serde_json::json!({"limit": 64, "path": "ingest/feed.go"})
        );
        assert_eq!(ingest[1].3["returned_bytes"], 64);
        assert_eq!(ingest[1].3["next_offset"], 64);
    }
    let (report, calls) = drive(&host, &run).await;

    assert_eq!(
        purposes(&report),
        [
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            FOLLOW_UP_PURPOSE,
            INTEGRATE_PURPOSE
        ]
    );
    let follow_up = calls[2].0[0].instructions.as_deref().unwrap();
    assert_eq!(calls[2].0.len(), 1);
    assert!(
        follow_up.contains(
            "not read to their end. Read these files and report additional \
             findings in the same format:\n- ingest/feed.go\nYou are read-only"
        ),
        "{follow_up}"
    );
    assert_eq!(
        host.phases("coverage"),
        [
            "auth: 2 of 2 files examined: 1 read; not read: empty: auth/__init__.py",
            "billing: 2 of 2 files examined: 1 read; not read: empty: billing/__init__.py",
            "ingest: 2 of 2 files examined: 2 read",
            "notify: 2 of 2 files examined: 1 read; not read: empty: notify/__init__.py",
            "rest: 2 of 2 files examined: 2 read",
        ]
    );
    assert!(report.not_covered.is_empty(), "{:?}", report.not_covered);
    assert!(
        host.sent()[3].contains("ingest-followup1-F1"),
        "{}",
        host.sent()[3]
    );
    assert_eq!(outcome_of(&report).exit_code, exit_code::PASS);

    // Without the follow-up's read, the partial read leaves feed.go not
    // covered, by file: the run needs attention instead of passing.
    let host = FakeHost::new(vec![
        turn(
            TurnState::Completed,
            vec![(
                PLANNER_SLOT,
                Node::Answer(fixture("audit-resmoke7-out9-planner.txt")),
            )],
        ),
        recorded_areas("audit-resmoke7-out9", &workers),
        turn(
            TurnState::Completed,
            vec![("worker-ingest", Node::Answer(worker_answer(&[], &[])))],
        ),
        turn(
            TurnState::Completed,
            vec![(
                INTEGRATOR_SLOT,
                Node::Answer(fixture("audit-resmoke7-out9-integrator.txt")),
            )],
        ),
    ])
    .recorded_calls(&fixture("audit-resmoke7-out9-worker-calls.jsonl"))
    .calls(
        "worker-ingest",
        vec![(
            "read_file",
            serde_json::json!({"path": "ingest/feed.go", "limit": 64}),
            true,
        )],
    );
    let (report, _) = drive(&host, &run).await;
    assert_eq!(
        entries(&report.not_covered),
        [(
            "ingest",
            FailureClass::NotReached,
            "ingest/feed.go: not read (the area worker did not read it to its end in its turn or \
             its follow-up, which read nothing new)"
        )]
    );
    assert_eq!(
        host.phases("coverage")[2],
        "ingest: 1 of 2 files examined: 1 read; not read, not covered: ingest/feed.go"
    );
    assert_eq!(outcome_of(&report).exit_code, exit_code::NEEDS_ATTENTION);
}

/// One turn of a [`ScriptedModel`]: each slot with its answer, and the tool
/// calls its workers made.
type ScriptedTurn = (Vec<(String, String)>, Vec<ToolCallRecord>);

/// A scripted model behind a host, for audits of many files: the planner
/// answers `plan`; each worker reads, in the order its instructions name
/// them, the files of its own it has not read yet, at most `reads` of them
/// per activation (all its budget lets it read), and from activation
/// `stall_after` on reads only what it already read; the integrator merges
/// nothing.
struct ScriptedModel {
    repo: std::path::PathBuf,
    plan: String,
    reads: usize,
    stall_after: Option<u32>,
    /// The Team of the last Apply: each slot and its instructions.
    team: Mutex<Vec<(String, Option<String>)>>,
    /// The slots of each Apply.
    applied: Mutex<Vec<Vec<String>>>,
    sent: Mutex<Vec<String>>,
    turns: Mutex<HashMap<String, ScriptedTurn>>,
    /// The files each worker slot read, in order, and its activations.
    read: Mutex<HashMap<String, Vec<String>>>,
    activations: Mutex<HashMap<String, u32>>,
    events: Mutex<Vec<RunEvent>>,
}

impl ScriptedModel {
    fn new(repo: &Path, plan: String, reads: usize) -> Self {
        Self {
            repo: repo.to_path_buf(),
            plan,
            reads,
            stall_after: None,
            team: Mutex::default(),
            applied: Mutex::default(),
            sent: Mutex::default(),
            turns: Mutex::default(),
            read: Mutex::default(),
            activations: Mutex::default(),
            events: Mutex::default(),
        }
    }

    fn phases(&self, name: &str) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                RunEvent::Phase { phase, detail, .. } if phase == name => Some(detail.clone()),
                _ => None,
            })
            .collect()
    }

    /// The files a worker's instructions name, one by one.
    fn named(&self, instructions: &str) -> Vec<String> {
        instructions
            .lines()
            .filter_map(|line| line.strip_prefix("- "))
            .filter(|path| self.repo.join(path).is_file())
            .map(str::to_owned)
            .collect()
    }

    /// One activation of worker `slot`: its read calls, recorded for node
    /// `node_id`.
    fn worker(&self, slot: &str, instructions: &str, node_id: &str) -> Vec<ToolCallRecord> {
        let activation = {
            let mut activations = self.activations.lock().unwrap();
            let count = activations.entry(slot.to_owned()).or_default();
            *count += 1;
            *count
        };
        let mut read = self.read.lock().unwrap();
        let done = read.entry(slot.to_owned()).or_default();
        let paths: Vec<String> = if self.stall_after.is_some_and(|after| activation > after) {
            done.iter().take(self.reads).cloned().collect()
        } else {
            self.named(instructions)
                .into_iter()
                .filter(|path| !done.contains(path))
                .take(self.reads)
                .collect()
        };
        paths
            .into_iter()
            .map(|path| {
                if !done.contains(&path) {
                    done.push(path.clone());
                }
                let arguments = serde_json::json!({ "path": path });
                ToolCallRecord {
                    node_id: node_id.to_owned(),
                    generation: 1,
                    tool: "read_file".into(),
                    result: simulated_read(&self.repo, &arguments),
                    arguments,
                    succeeded: true,
                }
            })
            .collect()
    }
}

#[async_trait]
impl RunHost for ScriptedModel {
    async fn apply_team(&self, _session_id: &str, edit: SessionTeamEdit) -> Result<(), RunError> {
        let team: Vec<(String, Option<String>)> = edit
            .slots
            .iter()
            .map(|slot| (slot.slot_id.clone(), slot.instructions.clone()))
            .collect();
        self.applied
            .lock()
            .unwrap()
            .push(team.iter().map(|(slot, _)| slot.clone()).collect());
        *self.team.lock().unwrap() = team;
        Ok(())
    }

    async fn send_turn(&self, _session_id: &str, request: &str) -> Result<String, RunError> {
        let turn_id = {
            let mut sent = self.sent.lock().unwrap();
            sent.push(request.into());
            format!("turn-{}", sent.len())
        };
        let team = self.team.lock().unwrap().clone();
        let mut nodes = Vec::new();
        let mut calls = Vec::new();
        for (index, (slot, instructions)) in team.iter().enumerate() {
            let node_id = format!("{turn_id}-node-{index}");
            let answer = if slot == PLANNER_SLOT {
                self.plan.clone()
            } else if slot == INTEGRATOR_SLOT {
                integrated_answer(&[])
            } else {
                calls.extend(self.worker(slot, instructions.as_deref().unwrap_or(""), &node_id));
                worker_answer(&[], &[])
            };
            nodes.push((slot.clone(), answer));
        }
        self.turns
            .lock()
            .unwrap()
            .insert(turn_id.clone(), (nodes, calls));
        Ok(turn_id)
    }

    async fn wait_turn(
        &self,
        _session_id: &str,
        turn_id: &str,
        _deadline: Instant,
    ) -> Result<TurnObservation, RunError> {
        let (nodes, _) = self.turns.lock().unwrap()[turn_id].clone();
        Ok(TurnObservation {
            session_id: "ses-1".into(),
            turn_id: turn_id.into(),
            state: TurnState::Completed,
            attention_reason: None,
            nodes: nodes
                .into_iter()
                .enumerate()
                .map(|(index, (slot, answer))| NodeObservation {
                    node_id: format!("{turn_id}-node-{index}"),
                    slot_id: slot,
                    model: ModelIdentity {
                        provider: "ollama".into(),
                        model: "m".into(),
                        runtime: "native".into(),
                    },
                    required: true,
                    kind: "slot".into(),
                    generations: vec![GenerationObservation {
                        generation: 1,
                        state: NodeState::Accepted,
                        answer: Some(answer),
                        failure: None,
                    }],
                })
                .collect(),
            checks: Vec::new(),
            review: None,
            usage: RunUsage::default(),
        })
    }

    async fn stop_turn(&self, _session_id: &str, _turn_id: &str) -> Result<(), RunError> {
        Ok(())
    }

    async fn run_repro(
        &self,
        _session_id: &str,
        _request: &crate::loadout::host::ReproRequest,
    ) -> Result<axocoatl_session::run_outcome::ReproRun, RunError> {
        unreachable!("an audit runs no reproductions")
    }

    async fn read_sandbox_file(
        &self,
        _session_id: &str,
        _path: &str,
        _max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, RunError> {
        unreachable!("an audit reads no sandbox files")
    }

    async fn record(&self, _run_id: &str, event: RunEvent) -> Result<(), RunError> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }

    async fn tool_calls(
        &self,
        _session_id: &str,
        turn_id: &str,
    ) -> Result<Option<Vec<ToolCallRecord>>, RunError> {
        Ok(Some(self.turns.lock().unwrap()[turn_id].1.clone()))
    }
}

/// A repository with `count` small files under `big/` and one under
/// `small/`, and the plan of those two areas.
fn many_files(count: usize) -> (tempfile::TempDir, String) {
    let files: Vec<(String, String)> = (0..count)
        .map(|index| {
            (
                format!("big/m{index:03}.py"),
                format!("def handler_{index}(request):\n    return request\n"),
            )
        })
        .chain([("small/a.py".to_owned(), "A = 1\n".to_owned())])
        .collect();
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let plan = plan_answer(&[
        ("big", "the request handlers", &["big/**"]),
        ("small", "the constants", &["small/**"]),
    ]);
    (repository(&files), plan)
}

/// The run of an audit of many files: the worker Agent's invocations per
/// activation, and its model's context as admission observed it.
fn scale_run(repo: &Path, invocations: u32) -> RunContext {
    let mut run = context_in(later(), repo);
    let worker = run
        .resolved
        .loadout
        .file
        .agents
        .iter_mut()
        .find(|agent| agent.role == LoadoutRole::Worker)
        .unwrap();
    let mut limits = run.resolved.loadout.file.budgets.agent.clone();
    limits.invocations = invocations;
    worker.budget = Some(limits);
    run.agent_contexts.insert("worker".into(), 32_768);
    run
}

async fn drive_scripted(host: &ScriptedModel, run: &RunContext) -> KindReport {
    let builds = Arc::new(Builds::default());
    let build =
        move |resolved: &ResolvedLoadout, slots: &[SlotPlan], checks: bool, revision: u64| {
            builds.build(resolved, slots, checks, revision)
        };
    drive_audit(host, run, &build).await.unwrap()
}

/// A 500-file area is more than one worker reads within the built-in
/// budget (300 invocations: about 120 reads), so before any worker runs
/// the host splits it into five numbered sub-areas of 100 files, each with
/// its own worker; the workers run two to a turn (200 planned reads), and a
/// scripted model that reads 150 files an activation reads every file in
/// the areas turns, with no follow-up.
#[tokio::test]
async fn a_500_file_area_is_split_into_sub_areas_that_each_worker_reads() {
    let (repo, plan) = many_files(500);
    let run = scale_run(repo.path(), 300);
    let host = ScriptedModel::new(repo.path(), plan, 150);
    let report = drive_scripted(&host, &run).await;

    let parts = ["big-1", "big-2", "big-3", "big-4", "big-5"];
    {
        let applied = host.applied.lock().unwrap();
        assert_eq!(applied[1], ["worker-big-1", "worker-big-2"]);
        assert_eq!(applied[2], ["worker-big-3", "worker-big-4"]);
        assert_eq!(applied[3], ["worker-big-5", "worker-small"]);
    }
    assert_eq!(
        purposes(&report),
        [
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            AREAS_PURPOSE,
            AREAS_PURPOSE,
            INTEGRATE_PURPOSE
        ]
    );
    assert_eq!(
        host.phases("assigned")[0],
        "501 files listed by a walk of the directory (not a Git work tree) in 6 areas: big-1 \
         100, big-2 100, big-3 100, big-4 100, big-5 100, small 1"
    );
    assert!(host.phases("assigned")[1]
        .starts_with("big-1 (part 1 of 5 of big, paths big/**): big/m000.py, big/m001.py"));
    assert_eq!(
        report.notes,
        [
            "area big has 500 files to read, about 500 reads of up to 8 KiB (the default read of \
          the worker's model), more than one worker makes within its budget (about 120 reads: \
          300 invocations at 2 a read, 60 held back for looking around and its answer), so the \
          host split it into 5 sub-areas, each with its own worker: big-1, big-2, big-3, \
          big-4, big-5"
        ]
    );
    assert_eq!(
        host.phases("applying_team")[1],
        "audit areas, turn 1 of 3: 2 read-only workers (big-1, big-2)"
    );
    let coverage = host.phases("coverage");
    for (index, part) in parts.iter().enumerate() {
        assert_eq!(
            coverage[index],
            format!("{part}: 100 of 100 files examined: 100 read")
        );
    }
    let read = host.read.lock().unwrap();
    for part in parts {
        assert_eq!(read[&format!("worker-{part}")].len(), 100);
    }
    // Each worker's instructions say which part it reads.
    let first = &host.sent.lock().unwrap()[1];
    assert!(first.contains(
        "This turn audits 2 of the 6 areas of the plan as executed (the others run in other \
         turns) in parallel: big-1, big-2."
    ));
    assert!(report.not_covered.is_empty(), "{:?}", report.not_covered);
    assert_eq!(outcome_of(&report).exit_code, exit_code::PASS);
}

/// A model that reads only 40 files an activation is followed up for as
/// long as each follow-up reads files it had not read: three times for 150
/// files, never a fixed two. A model that stops reading new files gets no
/// further follow-up, and what it left is not covered, by file.
#[tokio::test]
async fn follow_ups_go_on_while_each_reads_new_files() {
    let (repo, plan) = many_files(150);
    // 400 invocations plan 160 reads: the 150 files stay one area.
    let run = scale_run(repo.path(), 400);
    let host = ScriptedModel::new(repo.path(), plan.clone(), 40);
    let report = drive_scripted(&host, &run).await;
    assert_eq!(
        purposes(&report),
        [
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            FOLLOW_UP_PURPOSE,
            FOLLOW_UP_PURPOSE,
            FOLLOW_UP_PURPOSE,
            INTEGRATE_PURPOSE
        ]
    );
    assert_eq!(host.applied.lock().unwrap()[2], ["worker-big"]);
    assert_eq!(host.read.lock().unwrap()["worker-big"].len(), 150);
    assert!(host.sent.lock().unwrap()[4].contains("Follow-up 3 of the audit's areas (big)"));
    assert_eq!(
        host.phases("applying_team")[4],
        "audit follow-up 3: 1 read-only worker (big (30 unread))"
    );
    assert_eq!(
        host.phases("coverage")[0],
        "big: 150 of 150 files examined: 150 read"
    );
    assert!(report.not_covered.is_empty(), "{:?}", report.not_covered);
    assert!(report.notes.is_empty(), "{:?}", report.notes);
    assert_eq!(outcome_of(&report).exit_code, exit_code::PASS);

    // Its second follow-up rereads what it read: no third.
    let mut host = ScriptedModel::new(repo.path(), plan, 40);
    host.stall_after = Some(2);
    let report = drive_scripted(&host, &run).await;
    assert_eq!(
        purposes(&report),
        [
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            FOLLOW_UP_PURPOSE,
            FOLLOW_UP_PURPOSE,
            INTEGRATE_PURPOSE
        ]
    );
    assert_eq!(host.read.lock().unwrap()["worker-big"].len(), 80);
    let entries = entries(&report.not_covered);
    assert_eq!(entries.len(), 70);
    assert_eq!(
        entries[0],
        (
            "big",
            FailureClass::NotReached,
            "big/m080.py: not read (the area worker did not read it to its end in its turn or \
             its 2 follow-ups, the last of which read nothing new)"
        )
    );
    assert_eq!(outcome_of(&report).exit_code, exit_code::NEEDS_ATTENTION);
}

/// A budget of 60 invocations plans 24 reads an activation, so the
/// 500-file area becomes 21 sub-areas: with the small area, 22 workers,
/// which run eight to a turn (192 planned reads), each turn's request
/// naming its own.
#[tokio::test]
async fn many_sub_areas_run_in_turns_of_at_most_200_planned_reads() {
    let (repo, plan) = many_files(500);
    let run = scale_run(repo.path(), 60);
    let host = ScriptedModel::new(repo.path(), plan, 30);
    let report = drive_scripted(&host, &run).await;
    assert_eq!(
        purposes(&report),
        [
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            AREAS_PURPOSE,
            AREAS_PURPOSE,
            INTEGRATE_PURPOSE
        ]
    );
    let applied = host.applied.lock().unwrap();
    assert_eq!(
        applied[1..4].iter().map(Vec::len).collect::<Vec<_>>(),
        [8, 8, 6]
    );
    assert_eq!(applied[1][0], "worker-big-1");
    assert_eq!(applied[3].last().unwrap(), "worker-small");
    let sent = host.sent.lock().unwrap();
    assert!(sent[1].contains(
        "This turn audits 8 of the 22 areas of the plan as executed (the others run in other \
         turns) in parallel: big-1,"
    ));
    assert!(sent[3].contains("This turn audits 6 of the 22 areas"));
    assert_eq!(host.phases("coverage").len(), 22);
    assert!(host
        .read
        .lock()
        .unwrap()
        .values()
        .all(|read| read.len() <= 24));
    assert!(report.not_covered.is_empty(), "{:?}", report.not_covered);
    assert_eq!(outcome_of(&report).exit_code, exit_code::PASS);
}

/// Workers go to turns in order: at most 16 to a turn, at most 200 planned
/// reads, at most 128 KiB of instructions; one that alone needs more has a
/// turn of its own.
#[test]
fn worker_turns_bound_workers_reads_and_instructions() {
    let (run, _repo) = context(later());
    let slot = |bytes: usize| {
        let mut slot = plan_slots(&run.resolved).unwrap().remove(0);
        slot.instructions = Some("x".repeat(bytes));
        slot
    };
    let sizes = |slots: Vec<(usize, SlotPlan, u64)>| -> Vec<Vec<usize>> {
        worker_turns(slots)
            .into_iter()
            .map(|turn| turn.into_iter().map(|(index, _)| index).collect())
            .collect()
    };
    // Twenty light workers: sixteen, then four.
    let light: Vec<(usize, SlotPlan, u64)> = (0..20).map(|index| (index, slot(100), 1)).collect();
    assert_eq!(
        sizes(light).iter().map(Vec::len).collect::<Vec<_>>(),
        [MAX_WORKERS_PER_TURN, 4]
    );
    // Planned reads: 120 + 80 fill a turn, 1 more starts the next; a
    // worker planned past the bound runs alone.
    assert_eq!(
        sizes(vec![
            (0, slot(100), 120),
            (1, slot(100), 80),
            (2, slot(100), 1),
            (3, slot(100), 500),
            (4, slot(100), 1),
        ]),
        [vec![0, 1], vec![2], vec![3], vec![4]]
    );
    // Instructions: three of 50 KiB do not fit 128 KiB.
    assert_eq!(
        sizes(vec![
            (0, slot(50 * 1024), 1),
            (1, slot(50 * 1024), 1),
            (2, slot(50 * 1024), 1),
        ]),
        [vec![0, 1], vec![2]]
    );
    assert!(worker_turns(Vec::new()).is_empty());
}

/// However large the budget, one worker is planned at most a turn's 200
/// reads: with 2,000 invocations the 500-file area is still three
/// sub-areas, each in a turn of its own, and the note says why.
#[tokio::test]
async fn a_large_budget_still_plans_at_most_200_reads_a_worker() {
    let (repo, plan) = many_files(500);
    let run = scale_run(repo.path(), 2000);
    let host = ScriptedModel::new(repo.path(), plan, 200);
    let report = drive_scripted(&host, &run).await;
    assert_eq!(
        report.notes,
        [
            "area big has 500 files to read, about 500 reads of up to 8 KiB (the default read of \
          the worker's model), more than one worker makes within its budget (200 reads, the \
          most the host plans for one turn), so the host split it into 3 sub-areas, each with \
          its own worker: big-1, big-2, big-3"
        ]
    );
    let applied = host.applied.lock().unwrap();
    assert_eq!(applied[1], ["worker-big-1"]);
    assert_eq!(applied[2], ["worker-big-2"]);
    assert_eq!(applied[3], ["worker-big-3", "worker-small"]);
    assert_eq!(
        host.phases("assigned")[0],
        "501 files listed by a walk of the directory (not a Git work tree) in 4 areas: big-1 \
         167, big-2 167, big-3 166, small 1"
    );
    assert!(report.not_covered.is_empty(), "{:?}", report.not_covered);
    assert_eq!(outcome_of(&report).exit_code, exit_code::PASS);
}

/// The plan of three areas whose workers read `src/<area>/mod.rs`, with
/// `ingest` in place of `api`.
fn ingest_areas() -> (Scripted, tempfile::TempDir) {
    let repo = repository(&[
        ("src/auth/mod.rs", "pub fn login() {}\n"),
        ("src/db/mod.rs", "pub fn query() {}\n"),
        ("src/ingest/mod.rs", "pub fn load() {}\n"),
    ]);
    let plan = turn(
        TurnState::Completed,
        vec![(
            PLANNER_SLOT,
            Node::Answer(plan_answer(&[
                ("auth", "login and tokens", &["src/auth/**"]),
                ("db", "queries and migrations", &["src/db/**"]),
                ("ingest", "the order feed", &["src/ingest/**"]),
            ])),
        )],
    );
    (plan, repo)
}

/// What the re-ask of the out4 ingest worker answers: its finding, in the
/// format.
const INGEST_RESTATED: &str = "FINDINGS\n```json\n[{\"id\": \"F1\", \"title\": \"Missing error \
     check for json.Unmarshal\", \"detail\": \"json.Unmarshal's error is not checked\", \
     \"severity\": \"high\", \"location\": \"ingest/feed.go:29\"}]\n```";

/// The 1.3.0 resmoke8's run 4 (`out4`): the ingest worker read both its
/// files but described its finding in prose and wrote only a NOT_REACHED
/// block, so the area was listed as not covered and the run exited 2. Now
/// the worker gets a re-ask, a fresh read-only activation without tools
/// whose instructions quote that exact answer; it answers in the format,
/// the finding joins the area's report, and the run passes.
#[tokio::test]
async fn resmoke8_out4_a_worker_answering_in_prose_is_re_asked_and_the_run_passes() {
    let out4 = fixture("audit-resmoke8-out4-worker-ingest.txt");
    let (plan, repo) = ingest_areas();
    let script = || {
        vec![
            plan.clone(),
            turn_of(
                TurnState::Completed,
                vec![
                    answered("worker-auth", worker_answer(&[], &[])),
                    answered("worker-db", worker_answer(&[], &[])),
                    answered("worker-ingest", out4.clone()),
                ],
            ),
            turn(
                TurnState::Completed,
                vec![("worker-ingest", Node::Answer(INGEST_RESTATED.into()))],
            ),
            integrated(&[("Missing error check for json.Unmarshal", "ingest")]),
        ]
    };
    let run = context_in(later(), repo.path());
    let host = FakeHost::new(script());
    let (report, calls) = drive(&host, &run).await;
    assert_eq!(
        purposes(&report),
        [
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            REASK_PURPOSE,
            INTEGRATE_PURPOSE
        ]
    );
    // The re-ask: the ingest worker alone, read-only, without tools, its
    // instructions quoting the recorded answer and asking for the block.
    let reask = &calls[2].0;
    assert_eq!(reask.len(), 1);
    assert_eq!(reask[0].slot_id, "worker-ingest");
    assert!(reask[0].agent.tools.is_empty());
    assert_eq!(reask[0].agent.writes, Some(Vec::new()));
    let instructions = reask[0].instructions.as_deref().unwrap();
    assert!(instructions.starts_with("Your area: ingest\nScope: the order feed\nRe-ask 1 of 2: "));
    // In a fence longer than the answer's own.
    assert!(
        instructions.contains(&format!("````text\n{out4}\n````\n")),
        "{instructions}"
    );
    assert!(instructions.contains(
        "The host could not read this answer (the answer has no FINDINGS block: write a line \
         FINDINGS, then a fenced JSON array of findings ([] when there are none))."
    ));
    assert!(
        instructions.contains("with an empty array ([]) if it describes none. Do not call tools")
    );
    assert!(!instructions.contains("Read every file"), "{instructions}");
    assert!(host.sent()[2].contains("Re-ask 1 of the audit's areas (ingest)"));
    assert_eq!(
        host.applied.lock().unwrap()[2].slots[0].writes,
        Some(Some(Vec::new()))
    );
    assert_eq!(
        host.phases("reask"),
        ["worker-ingest answered re-ask 1 with a readable FINDINGS block: 1 finding"]
    );
    assert_eq!(
        host.phases("applying_team")[2],
        "audit re-ask 1: 1 read-only worker without tools (ingest)"
    );
    // The restated finding reached the integrator as the area's report.
    let request = &host.sent()[3];
    assert!(request.contains("REPORT of area ingest"));
    assert!(request.contains("\"id\":\"ingest-F1\""), "{request}");
    assert!(!request.contains("could not be read"), "{request}");
    // Coverage was the worker's reads all along: nothing is not covered.
    assert!(report.not_covered.is_empty(), "{:?}", report.not_covered);
    assert!(report.unreadable_findings.is_empty());
    assert!(host
        .phases("coverage")
        .contains(&"ingest: 1 of 1 files examined: 1 read".to_owned()));
    let outcome = outcome_with(&FakeHost::new(script()), &run).await.unwrap();
    assert_eq!(
        outcome.exit_code,
        exit_code::PASS,
        "{:?} {:?}",
        outcome.attention,
        outcome.unreadable_findings
    );

    // A re-ask without a result for a provider's reason is followed by the
    // second; when that one answers in prose too, the area is covered but
    // its findings are unreadable, with every answer kept.
    let host = FakeHost::new(vec![
        plan.clone(),
        turn_of(
            TurnState::Completed,
            vec![
                answered("worker-auth", worker_answer(&[], &[])),
                answered("worker-db", worker_answer(&[], &[])),
                answered("worker-ingest", out4.clone()),
            ],
        ),
        turn(
            TurnState::NeedsAttention,
            vec![(
                "worker-ingest",
                Node::Fail(FailureClass::ProviderFailure, "connection reset"),
            )],
        ),
        turn(
            TurnState::Completed,
            vec![("worker-ingest", Node::Answer("I found one defect.".into()))],
        ),
        integrated(&[("Missing error check for json.Unmarshal", "ingest")]),
    ]);
    let (report, calls) = drive(&host, &run).await;
    assert_eq!(
        purposes(&report),
        [
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            REASK_PURPOSE,
            REASK_PURPOSE,
            INTEGRATE_PURPOSE
        ]
    );
    assert!(calls[3].0[0]
        .instructions
        .as_deref()
        .unwrap()
        .starts_with("Your area: ingest\nScope: the order feed\nRe-ask 2 of 2: "));
    // The failed re-ask is the audit's to account for.
    assert!(report
        .accounted
        .contains(&("turn-3".to_string(), "turn-3-node-0".to_string())));
    assert!(report.not_covered.is_empty(), "{:?}", report.not_covered);
    let no_block = "the answer has no FINDINGS block: write a line FINDINGS, then a fenced \
                    JSON array of findings ([] when there are none)";
    assert_eq!(
        report.unreadable_findings,
        [UnreadableFindings {
            area: "ingest".into(),
            detail: format!(
                "the area worker's FINDINGS block could not be read ({no_block}), nor after 2 \
                 re-asks (the last: {no_block}); its files' coverage stands, and the integrator \
                 received its answer as text"
            ),
            answers: vec![out4.clone(), "I found one defect.".into()],
            node_id: Some("turn-4-node-0".into()),
            turn_id: Some("turn-4".into()),
        }]
    );
    assert_eq!(
        host.phases("reask"),
        [
            "worker-ingest has no result in re-ask 1 (provider_failure: connection reset)",
            &format!("worker-ingest's answer to re-ask 2 could not be read either ({no_block})"),
        ]
    );
    // The integrator still read the answer as text.
    assert!(host.sent()[4].contains("The worker's report could not be read"));
    assert!(host.sent()[4].contains("json.Unmarshal error is not checked"));
    let outcome = outcome_of(&report);
    assert_eq!(outcome.attention, ["Findings unreadable for ingest"]);
    assert_eq!(outcome.exit_code, exit_code::NEEDS_ATTENTION);
}

/// The 1.3.0 resmoke8's run 1 (`out1`): the integrator's answer was a
/// broken tool call written as text, so the host reported "The
/// integration was not read" and the run exited 2. Now it gets two
/// re-asks, each a fresh activation without tools sent the integrate
/// request again with that exact answer quoted; when both answer the same
/// way, the host merges the area findings itself (duplicates by file, line
/// and title removed, each keeping its area), notes it, and the run
/// passes.
#[tokio::test]
async fn resmoke8_out1_an_unreadable_integrator_is_re_asked_then_merged_by_the_host() {
    let out1 = fixture("audit-resmoke8-out1-integrator.txt");
    let script = |third: Node| {
        vec![
            planned(),
            turn(
                TurnState::Completed,
                vec![
                    (
                        "worker-auth",
                        Node::Answer(worker_answer(
                            &[("Token compared with ==", "src/auth/mod.rs:1")],
                            &[],
                        )),
                    ),
                    (
                        "worker-db",
                        Node::Answer(worker_answer(
                            &[
                                ("SQL built by format!", "src/db/mod.rs:1"),
                                // The auth defect again, written another way.
                                ("token compared with  ==!", "./src/auth/mod.rs:1:5"),
                            ],
                            &[],
                        )),
                    ),
                    ("worker-api", Node::Answer(worker_answer(&[], &[]))),
                ],
            ),
            turn(
                TurnState::Completed,
                vec![(INTEGRATOR_SLOT, Node::Answer(out1.clone()))],
            ),
            turn(
                TurnState::Completed,
                vec![(INTEGRATOR_SLOT, Node::Answer(out1.clone()))],
            ),
            turn(TurnState::Completed, vec![(INTEGRATOR_SLOT, third)]),
        ]
    };
    let (run, _repo) = context(later());
    let host = FakeHost::new(script(Node::Answer(out1.clone())));
    let (report, calls) = drive(&host, &run).await;
    assert_eq!(
        purposes(&report),
        [
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            INTEGRATE_PURPOSE,
            REASK_PURPOSE,
            REASK_PURPOSE
        ]
    );
    // Each re-ask: the integrator alone without tools, sent the integrate
    // request again with the recorded answer quoted.
    let integrate = host.sent()[2].clone();
    for (index, round) in [(3, 1), (4, 2)] {
        assert_eq!(calls[index].0.len(), 1);
        assert_eq!(calls[index].0[0].slot_id, INTEGRATOR_SLOT);
        assert!(calls[index].0[0].agent.tools.is_empty());
        let request = &host.sent()[index];
        assert!(request.starts_with(integrate.trim_end()), "{request}");
        assert!(request.contains(&format!(
            "Re-ask {round} of 2: the host could not read the FINDINGS block of the integrator's \
             answer"
        )));
        assert!(
            request.contains(&format!("```text\n{out1}\n```\n")),
            "{request}"
        );
        assert!(request.contains("Do not call tools and do not read files"));
    }
    // Round 2 quotes both answers.
    assert_eq!(host.sent()[4].matches("<function=read_file>").count(), 2);
    // The union of the area findings, the duplicate left out, each with
    // its area and the workers' ids.
    let merged: Vec<(&str, Option<&str>, FindingSource)> = report
        .findings
        .iter()
        .map(|finding| (finding.id.as_str(), finding.area.as_deref(), finding.source))
        .collect();
    assert_eq!(
        merged,
        [
            ("auth-F1", Some("auth"), FindingSource::AuditWorker),
            ("db-F1", Some("db"), FindingSource::AuditWorker),
        ]
    );
    let no_block = "the answer has no FINDINGS block: write a line FINDINGS, then a fenced \
                    JSON array of the merged findings ([] when there are none)";
    assert_eq!(
        report.notes,
        [format!(
            "findings merged by the host: the integrator's FINDINGS block could not be read, \
             nor after 2 re-asks ({no_block}), so the host took the union of the area findings \
             and removed duplicates by file, line and title (3 findings, 2 kept)"
        )]
    );
    assert!(report.not_covered.is_empty(), "{:?}", report.not_covered);
    assert!(report.last_turn_accounted);
    let outcome = outcome_with(&FakeHost::new(script(Node::Answer(out1.clone()))), &run)
        .await
        .unwrap();
    assert_eq!(
        outcome.exit_code,
        exit_code::PASS,
        "{:?} {:?}",
        outcome.attention,
        outcome.not_covered
    );
    assert_eq!(outcome.findings.len(), 2);

    // A re-ask that answers in the format ends it with the integrator's
    // own findings.
    let host = FakeHost::new(script(Node::Answer(integrated_answer(&[(
        "token compared with ==",
        "auth",
    )]))));
    let (report, _) = drive(&host, &run).await;
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].source, FindingSource::Integrator);
    assert!(report.notes.is_empty(), "{:?}", report.notes);
    assert!(!report.last_turn_accounted);
    assert_eq!(
        host.phases("reask").last().unwrap(),
        "the integrator answered re-ask 2 with a readable FINDINGS block"
    );

    // A last re-ask without a result for a provider's reason: the host
    // merges too, and the failed turn needs no attention.
    let failing = script(Node::Fail(FailureClass::ProviderFailure, "HTTP 503"));
    let host = FakeHost::new(failing.clone());
    let (report, _) = drive(&host, &run).await;
    assert_eq!(
        report.notes[0],
        format!(
            "findings merged by the host: the integrator's FINDINGS block could not be read \
             ({no_block}), and its re-ask 2 has no result (provider_failure: HTTP 503), so the \
             host took the union of the area findings and removed duplicates by file, line and \
             title (3 findings, 2 kept)"
        )
    );
    assert!(report
        .accounted
        .contains(&("turn-5".to_string(), "turn-5-node-0".to_string())));
    let mut failing = failing;
    failing[4].state = TurnState::NeedsAttention;
    let outcome = outcome_with(&FakeHost::new(failing), &run).await.unwrap();
    assert_eq!(
        outcome.exit_code,
        exit_code::PASS,
        "{:?} {:?}",
        outcome.attention,
        outcome.not_covered
    );
}

/// An integrator without a result gets one retry turn; when that fails
/// for a provider's reason too, the host merges the findings and the run
/// passes. A provider that refuses the request itself is not retried.
/// The wall clock still leaves the integration not covered.
#[tokio::test]
async fn an_integrator_failing_for_a_provider_s_reason_is_retried_then_merged_by_the_host() {
    let areas = || {
        turn(
            TurnState::Completed,
            vec![
                (
                    "worker-auth",
                    Node::Answer(worker_answer(
                        &[("token compared with ==", "src/auth/mod.rs:1")],
                        &[],
                    )),
                ),
                ("worker-db", Node::Answer(worker_answer(&[], &[]))),
                ("worker-api", Node::Answer(worker_answer(&[], &[]))),
            ],
        )
    };
    let failed = |class, message| {
        turn(
            TurnState::NeedsAttention,
            vec![(INTEGRATOR_SLOT, Node::Fail(class, message))],
        )
    };
    let (run, _repo) = context(later());
    let script = || {
        vec![
            planned(),
            areas(),
            failed(FailureClass::ProviderFailure, "HTTP 502"),
            failed(FailureClass::ProviderFailure, "stream ended early"),
        ]
    };
    let host = FakeHost::new(script());
    let (report, calls) = drive(&host, &run).await;
    assert_eq!(
        purposes(&report),
        [
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            INTEGRATE_PURPOSE,
            INTEGRATE_PURPOSE
        ]
    );
    // The retry is the integrate turn again, with its tools.
    assert_eq!(host.sent()[3], host.sent()[2]);
    assert!(!calls[3].0[0].agent.tools.is_empty());
    assert_eq!(
        host.phases("integrate_failed"),
        [
            "the integrator has no result (provider_failure: HTTP 502); it gets one more turn",
            "the integrator has no result (provider_failure: stream ended early)",
        ]
    );
    assert_eq!(
        report.notes,
        [
            "findings merged by the host: the integrator has no result after its retry \
          (provider_failure: stream ended early), so the host took the union of the area \
          findings and removed duplicates by file, line and title (1 finding, 1 kept)"
        ]
    );
    assert_eq!(report.findings[0].id, "auth-F1");
    assert!(report.not_covered.is_empty());
    let outcome = outcome_with(&FakeHost::new(script()), &run).await.unwrap();
    assert_eq!(
        outcome.exit_code,
        exit_code::PASS,
        "{:?} {:?}",
        outcome.attention,
        outcome.not_covered
    );

    // Refused by its provider: no retry, merged by the host.
    let host = FakeHost::new(vec![
        planned(),
        areas(),
        failed(FailureClass::ProviderRejected, "HTTP 401"),
    ]);
    let (report, _) = drive(&host, &run).await;
    assert_eq!(
        purposes(&report),
        [PLAN_PURPOSE, AREAS_PURPOSE, INTEGRATE_PURPOSE]
    );
    assert_eq!(
        report.notes,
        [
            "findings merged by the host: the integrator has no result (provider_rejected: HTTP \
          401), so the host took the union of the area findings and removed duplicates by file, \
          line and title (1 finding, 1 kept)"
        ]
    );

    // Another failure after the retry leaves the integration not covered.
    let host = FakeHost::new(vec![
        planned(),
        areas(),
        failed(FailureClass::RuntimeLimit, "too many steps"),
        failed(FailureClass::RuntimeLimit, "too many steps"),
    ]);
    let (report, _) = drive(&host, &run).await;
    assert!(report.notes.is_empty(), "{:?}", report.notes);
    assert_eq!(
        entries(&report.not_covered),
        [(
            INTEGRATION,
            FailureClass::RuntimeLimit,
            "the integrator has no result: too many steps; the area workers' findings are \
             reported unmerged"
        )]
    );
    assert_eq!(report.not_covered[0].turn_id.as_deref(), Some("turn-4"));
    assert!(!report.last_turn_accounted);
}

#[test]
fn the_host_merge_keys_findings_by_file_line_and_title() {
    assert_eq!(
        file_and_line("./billing/pagination.py:15"),
        ("billing/pagination.py".into(), "15".into())
    );
    assert_eq!(
        file_and_line("ingest/feed.go:29:7"),
        ("ingest/feed.go".into(), "29".into())
    );
    assert_eq!(
        file_and_line("ingest/feed.go"),
        ("ingest/feed.go".into(), String::new())
    );
    assert_eq!(
        normalized_title("  Missing error-check for `json.Unmarshal`! "),
        "missing error check for json unmarshal"
    );
}
