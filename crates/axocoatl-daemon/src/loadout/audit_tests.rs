//! The audit driver against a scripted `RunHost` and a recording edit
//! builder: the turns it starts, the teams it applies, the requests it
//! sends and the report it hands to the Outcome.
use super::*;
use crate::bootstrap::session_team::SessionTeamSlotEdit;
use crate::loadout::{KeepMode, RunOptions};
use axocoatl_config::loadout::{builtin_loadouts, resolve_loadout, ModelSpec, ParamValues};
use axocoatl_session::run_outcome::{
    exit_code, FindingSource, GenerationObservation, LoadoutRef, ModelIdentity, NetworkSummary,
    NodeFailure, RunOutcome, RunUsage, RunVerdict, VerdictInputs, RUN_OUTCOME_SCHEMA,
};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

const PLANNER_MODEL: &str = "ollama:qwen3:32b";
const WORKER_MODEL: &str = "openrouter:qwen/qwen3-coder";
const INTEGRATOR_MODEL: &str = "openrouter:openai/gpt-oss-120b";

fn context(deadline: Instant) -> RunContext {
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
            repo: "/repo".into(),
            params,
            keep: KeepMode::None,
            check_command: None,
            setup_command: None,
        },
        deadline,
    }
}

fn later() -> Instant {
    Instant::now() + Duration::from_secs(600)
}

#[derive(Clone)]
enum Node {
    Answer(String),
    Fail(FailureClass, &'static str),
    Running,
}

#[derive(Clone)]
struct Scripted {
    state: TurnState,
    nodes: Vec<(String, Node)>,
    /// Wait until the deadline and report the turn still running.
    until_deadline: bool,
}

fn turn(state: TurnState, nodes: Vec<(&str, Node)>) -> Scripted {
    Scripted {
        state,
        nodes: nodes
            .into_iter()
            .map(|(slot, node)| (slot.to_owned(), node))
            .collect(),
        until_deadline: false,
    }
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
}

impl FakeHost {
    fn new(script: Vec<Scripted>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            ..Self::default()
        }
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
            usage: RunUsage::default(),
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
        ("api", "HTTP handlers", &[]),
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
        turn(
            TurnState::Completed,
            vec![(
                INTEGRATOR_SLOT,
                Node::Answer(integrated_answer(&[
                    ("token compared with ==", "auth"),
                    ("SQL built by format!", "db"),
                ])),
            )],
        ),
    ]);
    let run = context(later());
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
        assert!(instructions.contains("FINDINGS") && instructions.contains("NOT_REACHED"));
    }
    let auth = calls[1].0[0].instructions.as_deref().unwrap();
    assert!(auth.contains("Your area: auth") && auth.contains("login and tokens"));
    assert!(
        auth.contains("- src/auth/**") && auth.contains("(db, api)"),
        "{auth}"
    );
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
        sent[0].contains("one AREAS block of 2-8 areas"),
        "{}",
        sent[0]
    );
    assert!(sent[1].contains("3 areas of the plan in parallel: auth, db, api"));
    assert!(sent[2].contains("REPORT of area auth") && sent[2].contains("token compared with =="));
    assert!(sent[2].contains("REPORT of area db") && sent[2].contains("SQL built by format!"));
    assert!(sent[2].contains("REPORT of area api"));
    assert!(!sent[2].contains("Not covered"));

    assert_eq!(report.findings.len(), 2);
    assert!(report
        .findings
        .iter()
        .all(|finding| finding.source == FindingSource::Integrator));
    assert_eq!(report.findings[1].area.as_deref(), Some("db"));
    assert!(report.not_covered.is_empty());
    assert!(!report.fail_on_findings && !report.budget_exhausted);
    assert_eq!(
        report
            .turn_refs
            .iter()
            .map(|turn| turn.purpose.as_str())
            .collect::<Vec<_>>(),
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

#[tokio::test]
async fn a_failed_worker_and_unreached_parts_are_not_covered() {
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
                    Node::Fail(FailureClass::ProviderRefusal, "classifier stop"),
                ),
                ("worker-api", Node::Answer("I looked around.".into())),
            ],
        ),
        turn(
            TurnState::Completed,
            vec![(
                INTEGRATOR_SLOT,
                Node::Answer(integrated_answer(&[("token compared with ==", "auth")])),
            )],
        ),
    ]);
    let run = context(later());
    let (report, _) = drive(&host, &run).await;

    let entries: Vec<(&str, FailureClass)> = report
        .not_covered
        .iter()
        .map(|entry| (entry.area.as_str(), entry.class))
        .collect();
    assert_eq!(
        entries,
        [
            ("auth: src/auth/oauth.rs", FailureClass::NotReached),
            ("db", FailureClass::ProviderRefusal),
            ("api", FailureClass::Other),
        ]
    );
    let db = &report.not_covered[1];
    assert!(db.detail.contains("classifier stop"), "{}", db.detail);
    assert_eq!(db.turn_id.as_deref(), Some("turn-2"));
    assert_eq!(db.node_id.as_deref(), Some("turn-2-node-1"));
    assert!(report.not_covered[2].detail.contains("no FINDINGS block"));

    // The paused areas turn is stopped before the integrator's Apply.
    assert_eq!(
        host.log(),
        [
            "apply",
            "send:turn-1",
            "apply",
            "send:turn-2",
            "stop:turn-2",
            "apply",
            "send:turn-3"
        ]
    );
    assert_eq!(report.turn_refs[1].state, TurnState::Stopped);
    assert_eq!(report.turns[1].state, TurnState::Stopped);

    // The integrator gets the readable report, the unreadable answer as text,
    // no report for the failed area, and the not-covered list.
    let request = &host.sent()[2];
    assert!(request.contains("REPORT of area auth"));
    assert!(request.contains("REPORT of area api") && request.contains("I looked around."));
    assert!(!request.contains("REPORT of area db"));
    assert!(request.contains("- db (provider_refusal)"), "{request}");
    assert!(request.contains("- auth: src/auth/oauth.rs (not_reached)"));

    let outcome = outcome_of(&report);
    assert_eq!(outcome.exit_code, exit_code::NEEDS_ATTENTION);
    assert!(outcome
        .attention
        .iter()
        .any(|reason| reason.contains("3 areas were not covered")));
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
        turn(
            TurnState::Completed,
            vec![(INTEGRATOR_SLOT, Node::Answer(integrated_answer(&[])))],
        ),
    ]);
    let run = context(later());
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
            vec![(INTEGRATOR_SLOT, Node::Answer(integrated_answer(&[])))],
        ),
    ]);
    let run = context(later());
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
    let run = context(later());
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

#[tokio::test]
async fn a_planner_without_an_answer_is_not_retried() {
    let host = FakeHost::new(vec![turn(
        TurnState::NeedsAttention,
        vec![(
            PLANNER_SLOT,
            Node::Fail(FailureClass::ProviderRejected, "HTTP 401"),
        )],
    )]);
    let run = context(later());
    let (report, _) = drive(&host, &run).await;
    assert_eq!(host.sent().len(), 1);
    assert_eq!(report.not_covered[0].area, WHOLE_SCOPE);
    assert_eq!(report.not_covered[0].class, FailureClass::ProviderRejected);
    // Nothing follows, so the paused turn is left for the person.
    assert!(host.stopped.lock().unwrap().is_empty());
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
    let run = context(later());
    let (report, _) = drive(&host, &run).await;
    let ids: Vec<&str> = report.findings.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, ["auth-F1", "db-F1"]);
    assert!(report
        .findings
        .iter()
        .all(|finding| finding.source == FindingSource::AuditWorker));
    assert_eq!(report.not_covered.len(), 1);
    assert_eq!(report.not_covered[0].area, INTEGRATOR_SLOT);
    assert_eq!(report.not_covered[0].class, FailureClass::Budget);
    assert!(report.not_covered[0].detail.contains("reported unmerged"));
    assert!(report.budget_exhausted);
}

#[tokio::test]
async fn the_wall_clock_stops_the_areas_turn_and_skips_integration() {
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
    let run = context(Instant::now() + Duration::from_millis(400));
    let (report, calls) = drive(&host, &run).await;
    assert_eq!(calls.len(), 2, "no integrator Apply after the deadline");
    assert_eq!(host.sent().len(), 2);
    assert_eq!(host.log()[4], "stop:turn-2");
    assert!(report.budget_exhausted);
    let entries: Vec<(&str, FailureClass)> = report
        .not_covered
        .iter()
        .map(|entry| (entry.area.as_str(), entry.class))
        .collect();
    assert_eq!(
        entries,
        [
            ("db", FailureClass::Budget),
            ("api", FailureClass::Budget),
            (INTEGRATOR_SLOT, FailureClass::Budget),
        ]
    );
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].source, FindingSource::AuditWorker);
    assert_eq!(report.turn_refs[1].state, TurnState::Stopped);
    assert_eq!(outcome_of(&report).exit_code, exit_code::NEEDS_ATTENTION);
}

#[tokio::test]
async fn a_spent_wall_clock_starts_no_turn() {
    let host = FakeHost::new(Vec::new());
    let run = context(Instant::now());
    let (report, calls) = drive(&host, &run).await;
    assert_eq!(calls.len(), 1);
    assert!(host.sent().is_empty());
    assert_eq!(report.not_covered[0].area, WHOLE_SCOPE);
    assert_eq!(report.not_covered[0].class, FailureClass::Budget);
    assert!(report.budget_exhausted);
}

#[test]
fn slot_plans_are_pure_and_read_only() {
    let run = context(later());
    let plan = parse_plan(&three_areas(), 2, 8).unwrap();
    let planner = plan_slots(&run.resolved).unwrap();
    assert_eq!(planner.len(), 1);
    assert_eq!(planner[0].agent.role, LoadoutRole::Planner);
    assert!(planner[0].instructions.is_none());
    let workers = area_slots(&run.resolved, &plan).unwrap();
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
    let api = worker_instructions(None, &plan.areas[2], &plan);
    assert!(api.starts_with("Your area: api"));
    assert!(api.contains("the plan names none"));
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
