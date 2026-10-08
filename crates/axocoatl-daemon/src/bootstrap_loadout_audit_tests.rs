//! The built-in audit loadout on a live daemon and real Podman, with a
//! scripted local model: how the run judges each area worker from the tool
//! calls its Session recorded, and the planner's one retry after a provider
//! failure. The body runs in a child process with its own data root,
//! because bootstrap reads the process environment.
use super::*;
use crate::loadout::host::ReproRequest;
use axocoatl_session::run_outcome::{FailureClass, ReproRun};
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHILD: &str = "AXOCOATL_LOADOUT_AUDIT_TEST_CHILD";
const MODEL: &str = "audit-model:latest";

/// How the scripted model answers each Agent of the audit:
/// - the planner's first call asks for 129 tool calls at once, which the
///   native Ollama provider refuses ("too many native tool calls"), as the
///   1.3.0 re-smoke's planner did; its next call answers a plan of two
///   areas, `billing` and `notify`;
/// - the billing worker reads `billing/pagination.py`, lists `billing`,
///   greps it for `get_page` (one match) and for `overflow` (none), then
///   answers with nothing not reached;
/// - the notify worker answers at once, with no tool call, and lists a
///   path that does not exist as not reached, as the re-smoke's billing
///   worker did;
/// - the integrator answers with no findings.
struct ScriptedAudit {
    planner_calls: Arc<AtomicUsize>,
}

impl ScriptedAudit {
    fn reply(model: &serde_json::Value, message: serde_json::Value) -> ResponseTemplate {
        let reply = serde_json::json!({
            "model": model, "created_at": "2026-10-07T00:00:00Z",
            "message": message,
            "done": true, "done_reason": "stop", "prompt_eval_count": 40, "eval_count": 8
        });
        ResponseTemplate::new(200).set_body_raw(format!("{reply}\n"), "application/x-ndjson")
    }

    fn answer(model: &serde_json::Value, content: &str) -> ResponseTemplate {
        Self::reply(
            model,
            serde_json::json!({"role": "assistant", "content": content}),
        )
    }
}

impl wiremock::Respond for ScriptedAudit {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: serde_json::Value = request.body_json().unwrap_or_default();
        let model = &body["model"];
        let text = String::from_utf8_lossy(&request.body);
        let has_results = body["messages"]
            .as_array()
            .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
        if text.contains("The area workers of this audit have finished") {
            return Self::answer(model, "FINDINGS\n```json\n[]\n```");
        }
        if text.contains("Your area: billing") {
            if has_results {
                return Self::answer(
                    model,
                    "FINDINGS\n```json\n[]\n```\nNOT_REACHED\n```json\n[]\n```",
                );
            }
            return Self::reply(
                model,
                serde_json::json!({"role": "assistant", "content": "", "tool_calls": [
                    {"id": "call_read", "function": {"index": 0, "name": "read_file",
                        "arguments": {"path": "billing/pagination.py"}}},
                    {"id": "call_list", "function": {"index": 1, "name": "list_dir",
                        "arguments": {"path": "billing"}}},
                    {"id": "call_grep", "function": {"index": 2, "name": "grep",
                        "arguments": {"pattern": "get_page", "path": "billing"}}},
                    {"id": "call_nomatch", "function": {"index": 3, "name": "grep",
                        "arguments": {"pattern": "overflow", "path": "billing"}}}
                ]}),
            );
        }
        if text.contains("Your area: notify") {
            return Self::answer(
                model,
                "{\"FINDINGS\": [], \"NOT_REACHED\": [\"notify/legacy/old_handler.py\"]}",
            );
        }
        if text.contains("Plan this audit before it starts") {
            if self.planner_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let calls: Vec<serde_json::Value> = (0..129)
                    .map(|index| {
                        serde_json::json!({"id": format!("call_{index}"), "function": {
                            "index": index, "name": "list_dir", "arguments": {"path": "."}}})
                    })
                    .collect();
                return Self::reply(
                    model,
                    serde_json::json!({"role": "assistant", "content": "", "tool_calls": calls}),
                );
            }
            return Self::answer(
                model,
                "AREAS\n```json\n{\"areas\": [\
                 {\"name\": \"billing\", \"scope\": \"invoice pagination\", \"paths\": [\"billing/**\"]}, \
                 {\"name\": \"notify\", \"scope\": \"outbound webhooks\", \"paths\": [\"notify/**\"]}]}\n```",
            );
        }
        ResponseTemplate::new(500).set_body_string("unexpected request")
    }
}

/// The audited local Ollama server the native provider admits, answering
/// `/api/chat` with [`ScriptedAudit`].
async fn model_server(planner_calls: Arc<AtomicUsize>) -> MockServer {
    let server = MockServer::start().await;
    let digest = "a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72";
    for (verb, route, body) in [
        (
            "GET",
            "/api/version",
            serde_json::json!({"version": "0.20.6"}),
        ),
        (
            "GET",
            "/api/status",
            serde_json::json!({"cloud": {"disabled": true}}),
        ),
        (
            "POST",
            "/api/show",
            serde_json::json!({"details": {"format": "gguf"}, "capabilities": ["completion", "tools"]}),
        ),
        (
            "GET",
            "/api/tags",
            serde_json::json!({"models": [{"name": MODEL, "model": MODEL, "digest": digest}]}),
        ),
        (
            "GET",
            "/api/ps",
            serde_json::json!({"models": [{"name": MODEL, "model": MODEL, "digest": digest,
                "details": {"format": "gguf"}, "context_length": 32768}]}),
        ),
    ] {
        Mock::given(method(verb))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = request.body_json().unwrap_or_default();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": body["model"], "created_at": "2026-10-07T00:00:00Z",
                "response": "", "done": true, "done_reason": "load"
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ScriptedAudit { planner_calls })
        .mount(&server)
        .await;
    server
}

/// What the task that sent each turn returned, by turn id (`None` while it
/// runs).
type Sends = Arc<std::sync::Mutex<HashMap<String, Option<Result<(), String>>>>>;

/// The run driver's host over this daemon, as the server's `DaemonRunHost`
/// is, tool calls included: each turn is sent as `/ws` sends it and
/// observed through the control-plane projection.
struct AuditHost {
    daemon: Arc<AxocoatlDaemon>,
    labels: std::sync::Mutex<Vec<CheckLabel>>,
    sends: Sends,
}

#[async_trait::async_trait]
impl crate::loadout::RunHost for AuditHost {
    async fn apply_team(
        &self,
        session_id: &str,
        edit: crate::SessionTeamEdit,
    ) -> Result<(), RunError> {
        *self.labels.lock().unwrap() = CheckLabel::of_edit(&edit);
        Ok(self.daemon.apply_loadout_team(session_id, edit).await?)
    }

    async fn send_turn(&self, session_id: &str, request: &str) -> Result<String, RunError> {
        let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
        self.sends.lock().unwrap().insert(turn_id.clone(), None);
        let (daemon, sends) = (self.daemon.clone(), self.sends.clone());
        let (session, turn, input) = (session_id.to_string(), turn_id.clone(), request.to_string());
        tokio::spawn(async move {
            let (sink, receiver) =
                tokio::sync::mpsc::unbounded_channel::<axocoatl_actor::AgentStreamChunk>();
            drop(receiver);
            let result = daemon
                .execute_session_turn_streaming(
                    &session,
                    &turn,
                    Some(turn.clone()),
                    None,
                    &input,
                    Vec::new(),
                    Vec::new(),
                    None,
                    None,
                    sink,
                )
                .await;
            sends.lock().unwrap().insert(
                turn,
                Some(result.map(|_| ()).map_err(|error| error.to_string())),
            );
        });
        Ok(turn_id)
    }

    async fn wait_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        deadline: std::time::Instant,
    ) -> Result<TurnObservation, RunError> {
        loop {
            let labels = self.labels.lock().unwrap().clone();
            let observed = self
                .daemon
                .loadout_turn_observation(session_id, turn_id, &labels, None)
                .await?;
            let sent = self.sends.lock().unwrap().get(turn_id).cloned().flatten();
            match (&observed, &sent) {
                (Some(observation), _) if observation.state != TurnState::Running => {
                    return Ok(observation.clone())
                }
                (None, Some(Err(error))) => {
                    return Err(RunError::Infrastructure(format!(
                        "the turn could not start: {error}"
                    )))
                }
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                return observed
                    .ok_or_else(|| RunError::Infrastructure("the turn has no projection".into()));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn stop_turn(&self, session_id: &str, turn_id: &str) -> Result<(), RunError> {
        self.daemon
            .stop_session_turn(session_id, turn_id)
            .await
            .map(|_| ())
            .map_err(Into::into)
    }

    async fn run_repro(
        &self,
        _session_id: &str,
        _request: &ReproRequest,
    ) -> Result<ReproRun, RunError> {
        Err(RunError::NotImplemented("an audit reproduces nothing"))
    }

    async fn read_sandbox_file(
        &self,
        session_id: &str,
        path: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, RunError> {
        Ok(self
            .daemon
            .loadout_read_sandbox_file(session_id, path, max_bytes)
            .await?)
    }

    async fn record(&self, run_id: &str, event: RunEvent) -> Result<(), RunError> {
        self.daemon
            .record_loadout_run_event(run_id, &event)
            .map(|_| ())
            .map_err(Into::into)
    }

    async fn recorded_events(&self, run_id: &str) -> Result<Vec<RunEvent>, RunError> {
        Ok(self.daemon.loadout_run_recorded_events(run_id)?)
    }

    async fn tool_calls(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<Vec<crate::loadout::ToolCallRecord>>, RunError> {
        Ok(self.daemon.loadout_turn_tool_calls(session_id, turn_id)?)
    }

    async fn network_summary(&self, session_id: &str) -> Result<NetworkSummary, RunError> {
        Ok(self.daemon.loadout_network_summary(session_id).await)
    }

    async fn stop_requested(&self, run_id: &str) -> bool {
        self.daemon.loadout_run_stop_requested(run_id)
    }

    async fn finish(&self, run_id: &str, outcome: &RunOutcome) -> Result<(), RunError> {
        Ok(self.daemon.finish_loadout_run(run_id, outcome)?)
    }
}

/// Removes the containers, volumes and networks of every Session the test
/// created, by exact name, whatever an assertion did.
struct Cleanup(std::sync::Mutex<Vec<String>>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for id in self
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
        {
            let names: Vec<String> = ["axo-ses-", "axo-egr-", "axo-brw-", "axo-pvw-", "axo-svc-"]
                .iter()
                .map(|prefix| format!("{prefix}{id}"))
                .collect();
            let _ = std::process::Command::new("podman")
                .args(["rm", "-f", "--ignore"])
                .args(&names)
                .output();
            for kind in ["volume", "network"] {
                if let Ok(listed) = std::process::Command::new("podman")
                    .args([kind, "ls", "--format", "{{.Name}}"])
                    .output()
                {
                    for name in String::from_utf8_lossy(&listed.stdout)
                        .lines()
                        .filter(|name| name.ends_with(id.as_str()))
                    {
                        let _ = std::process::Command::new("podman")
                            .args([kind, "rm", "-f", name])
                            .output();
                    }
                }
            }
        }
    }
}

/// The built-in audit on real Podman, as the 1.3.0 re-smoke ran it:
/// - the planner's provider fails on its first call ("too many native tool
///   calls") and the planner gets one more turn, which plans;
/// - the notify worker answers without a tool call: from the tool calls its
///   Session recorded, it examined nothing, so notify is not covered and
///   the path it listed that does not exist is a gap, not a note;
/// - the billing worker read its area, with the arguments and outcomes the
///   Session recorded for its calls (the host's repository captures left
///   out), so billing is covered;
/// - the run needs attention, and its Outcome and JUnit say why.
///
/// ```text
/// CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-daemon --lib \
///   an_audit_on_podman_judges_workers_by_their_recorded_tool_calls -- --ignored
/// ```
#[tokio::test]
#[ignore = "requires rootless Podman, docker.io/library/alpine:3.20 and the egress sidecar image"]
async fn an_audit_on_podman_judges_workers_by_their_recorded_tool_calls() {
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(900),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "bootstrap::loadout_runs::audit_tests::an_audit_on_podman_judges_workers_by_their_recorded_tool_calls",
                    "--nocapture",
                    "--ignored",
                ])
                .env(CHILD, "1")
                .env("AXOCOATL_DATA_DIR", root.path().join("data"))
                .env("AXOCOATL_SOCKET_PATH", "ipc/daemon.sock")
                .current_dir(root.path())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    let planner_calls = Arc::new(AtomicUsize::new(0));
    let server = model_server(planner_calls.clone()).await;
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("axocoatl.yaml");
    std::fs::write(
        &config_path,
        format!(
            "agents: []\nproviders:\n  ollama:\n    base_url: {}\nsandbox:\n  backend: podman\n  network: bridge\nconsolidation:\n  enabled: false\n",
            server.uri()
        ),
    )
    .unwrap();
    let config = axocoatl_config::load_config(&config_path).await.unwrap();
    let daemon = Arc::new(AxocoatlDaemon::bootstrap_headless(config).await.unwrap());
    daemon.set_config_path(&config_path);
    // The repository is outside the data root, which the container must not
    // reach.
    let outside = tempfile::Builder::new()
        .prefix("axocoatl-loadout-audit-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let repo = outside.path().join("repo");
    for (file, text) in [
        ("README.md", "# Audit fixture\n"),
        (
            "billing/pagination.py",
            "def get_page(items, page, size):\n    return items[page * size:(page + 1) * size + 1]\n",
        ),
        (
            "notify/webhook.py",
            "WEBHOOK = 'https://hooks.example.invalid/orders'\n",
        ),
    ] {
        let path = repo.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    // A Git repository, as the re-smoke's fixture was: the host captures
    // the repository around each activation with Git.
    for args in [
        &["init", "-q"][..],
        &["add", "-A"],
        &[
            "-c",
            "user.name=Audit fixture",
            "-c",
            "user.email=audit@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
    let model = format!("ollama:{MODEL}");
    let (accepted, context) = daemon
        .admit_loadout_run(RunRequest {
            loadout: "audit".into(),
            task: "Find correctness defects in billing and notify.".into(),
            repo: repo.display().to_string(),
            params: [
                ("planner_model".to_string(), model.clone()),
                ("worker_model".to_string(), model.clone()),
                ("integrator_model".to_string(), model),
            ]
            .into_iter()
            .collect(),
            keep: Default::default(),
            check_command: None,
            setup_command: None,
            request_id: "podman-audit".into(),
        })
        .await
        .unwrap();
    let cleanup = Cleanup(std::sync::Mutex::new(vec![accepted.session_id.clone()]));
    let result = async {
        let ready = daemon.loadout_run(&accepted.run_id).await?;
        assert!(ready.outcome.is_none(), "the environment failed: {ready:?}");
        let host = AuditHost {
            daemon: daemon.clone(),
            labels: std::sync::Mutex::new(Vec::new()),
            sends: Arc::new(std::sync::Mutex::new(HashMap::new())),
        };
        let outcome = crate::loadout::driver::run_to_outcome(&host, &context)
            .await
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        let events = daemon.loadout_run_recorded_events(&accepted.run_id)?;
        let phases: Vec<(String, String)> = events
            .iter()
            .filter_map(|event| match event {
                RunEvent::Phase { phase, detail, .. } => Some((phase.clone(), detail.clone())),
                _ => None,
            })
            .collect();

        // The planner's provider failed once, and its retry planned.
        assert_eq!(planner_calls.load(Ordering::SeqCst), 2, "{phases:?}");
        let failed: Vec<&String> = phases
            .iter()
            .filter(|(phase, _)| phase == "plan_failed")
            .map(|(_, detail)| detail)
            .collect();
        assert_eq!(failed.len(), 1, "{phases:?}");
        assert!(
            failed[0].contains("too many native tool calls")
                && failed[0].ends_with("it gets one more turn"),
            "{failed:?}"
        );
        let purposes: Vec<&str> = outcome
            .turns
            .iter()
            .map(|turn| turn.purpose.as_str())
            .collect();
        assert_eq!(
            purposes,
            [
                crate::loadout::audit::PLAN_PURPOSE,
                crate::loadout::audit::PLAN_PURPOSE,
                crate::loadout::audit::AREAS_PURPOSE,
                crate::loadout::audit::INTEGRATE_PURPOSE
            ],
            "{outcome:?}"
        );

        // What the Session recorded of the workers' calls: the billing
        // worker's read, listing and greps with their arguments and what
        // they returned, and nothing of the host's repository captures or
        // of the notify worker.
        let areas_turn = &outcome.turns[2].turn_id;
        let calls = daemon
            .loadout_turn_tool_calls(&accepted.session_id, areas_turn)?
            .expect("the areas turn has a record of tool calls");
        let mut seen: Vec<(String, serde_json::Value, bool)> = calls
            .iter()
            .map(|call| (call.tool.clone(), call.arguments.clone(), call.succeeded))
            .collect();
        seen.sort_by(|a, b| (&a.0, a.1.to_string()).cmp(&(&b.0, b.1.to_string())));
        assert_eq!(
            seen,
            [
                (
                    "grep".to_string(),
                    serde_json::json!({"pattern": "get_page", "path": "billing"}),
                    true
                ),
                (
                    "grep".to_string(),
                    serde_json::json!({"pattern": "overflow", "path": "billing"}),
                    true
                ),
                (
                    "list_dir".to_string(),
                    serde_json::json!({"path": "billing"}),
                    true
                ),
                (
                    "read_file".to_string(),
                    serde_json::json!({"path": "billing/pagination.py"}),
                    true
                ),
            ],
            "{calls:?}"
        );
        assert!(
            calls.iter().all(|call| call.node_id == calls[0].node_id),
            "every call is the billing worker's"
        );
        // Each result as the tool returned it, read from the Session's
        // content store: the grep's matches name the file it matched.
        let result = |tool: &str, pattern: Option<&str>| {
            calls
                .iter()
                .find(|call| {
                    call.tool == tool
                        && pattern.is_none_or(|pattern| call.arguments["pattern"] == pattern)
                })
                .map(|call| call.result.clone())
                .unwrap()
        };
        assert!(
            result("read_file", None)["content"]
                .as_str()
                .is_some_and(|content| content.starts_with("def get_page(")),
            "{calls:?}"
        );
        assert!(
            result("list_dir", None)["listing"]
                .as_str()
                .is_some_and(|listing| listing.contains("pagination.py")),
            "{calls:?}"
        );
        assert_eq!(
            result("grep", Some("get_page"))["matches"],
            "billing/pagination.py:1:def get_page(items, page, size):\n",
            "{calls:?}"
        );
        assert_eq!(result("grep", Some("overflow"))["matches"], "");

        // Billing is covered; notify, which examined nothing, is not, and
        // the path it listed that does not exist is a gap, not a note.
        assert_eq!(
            outcome.exit_code,
            exit_code::NEEDS_ATTENTION,
            "{:?} {:?}",
            outcome.attention,
            outcome.not_covered
        );
        let entries: Vec<(&str, FailureClass, &str)> = outcome
            .not_covered
            .iter()
            .map(|entry| (entry.area.as_str(), entry.class, entry.detail.as_str()))
            .collect();
        assert_eq!(
            entries,
            [
                (
                    "notify",
                    FailureClass::NotReached,
                    "the area worker examined nothing of its area: it made no tool call"
                ),
                (
                    "notify",
                    FailureClass::NotReached,
                    "notify/legacy/old_handler.py (the area worker reported it did not reach \
                     this; no such path exists, but the worker did not examine its area)"
                ),
            ],
            "{outcome:?}"
        );
        assert_eq!(outcome.attention, ["1 area was not covered"]);
        assert!(outcome.notes.is_empty(), "{:?}", outcome.notes);
        let junit = axocoatl_session::run_junit::render_junit(&outcome)
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        assert!(
            junit.contains("name=\"notify (1 of 2)\"")
                && junit.contains("examined nothing of its area"),
            "{junit}"
        );
        Ok::<(), DaemonError>(())
    }
    .await;
    let shutdown = daemon.shutdown_session_runtimes_checked().await;
    drop(cleanup);
    result.unwrap();
    shutdown.unwrap();
}
