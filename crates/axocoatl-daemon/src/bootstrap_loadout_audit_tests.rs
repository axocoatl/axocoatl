//! The built-in audit loadout on a live daemon and real Podman, with a
//! scripted local model: how the run checks each area's files from the tool
//! calls its Session recorded, follows up a worker that left files of its
//! area unread, and gives the planner its one retry after a provider
//! failure. Each body runs in a child process with its own data root,
//! because bootstrap reads the process environment.
use super::*;
use crate::loadout::host::ReproRequest;
use axocoatl_session::run_outcome::{FailureClass, ReproRun};
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHILD: &str = "AXOCOATL_LOADOUT_AUDIT_TEST_CHILD";
const MODEL: &str = "audit-model:latest";

/// How the scripted model answers the audit's Agents.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario {
    /// - the planner's first call asks for 129 tool calls at once, which
    ///   the native Ollama provider refuses ("too many native tool
    ///   calls"), as the 1.3.0 re-smoke's planner did; its next call
    ///   answers a plan of two areas, `billing` and `notify`;
    /// - the billing worker reads `billing/pagination.py`, lists
    ///   `billing`, greps it for `get_page` (one match) and for `overflow`
    ///   (none), then answers;
    /// - the notify worker answers at once, every time, with no tool call,
    ///   listing a path that does not exist as not reached, as the
    ///   re-smoke's billing worker did;
    /// - the rest worker reads `README.md`, which no planned area holds.
    NeverReads,
    /// - the planner plans `billing` and `notify` at once;
    /// - the billing worker reads `billing/pagination.py`, and
    ///   `billing/rates.csv`, longer than one read, in two reads (the first
    ///   64 KiB, then 32 KiB from offset 65536), and skips
    ///   `billing/invoice.py`; in its follow-up, which names that file, it
    ///   reads it and reports a finding there;
    /// - the notify and rest workers read their files.
    SkipsThenReads,
}

struct ScriptedAudit {
    scenario: Scenario,
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

    /// Native tool calls, each `(name, arguments)`.
    fn calls(model: &serde_json::Value, calls: &[(&str, serde_json::Value)]) -> ResponseTemplate {
        let calls: Vec<serde_json::Value> = calls
            .iter()
            .enumerate()
            .map(|(index, (name, arguments))| {
                serde_json::json!({"id": format!("call_{index}"), "function": {
                    "index": index, "name": name, "arguments": arguments}})
            })
            .collect();
        Self::reply(
            model,
            serde_json::json!({"role": "assistant", "content": "", "tool_calls": calls}),
        )
    }

    /// A worker's report with one finding at `location`, or none.
    fn report(model: &serde_json::Value, finding: Option<(&str, &str)>) -> ResponseTemplate {
        let findings = match finding {
            Some((title, location)) => serde_json::json!([{"id": "F1", "title": title,
                "detail": "seen in the file", "severity": "high", "location": location}]),
            None => serde_json::json!([]),
        };
        Self::answer(
            model,
            &format!("FINDINGS\n```json\n{findings}\n```\nNOT_REACHED\n```json\n[]\n```"),
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
        let read = |path: &str| ("read_file", serde_json::json!({ "path": path }));
        if text.contains("The area workers of this audit have finished") {
            return Self::answer(model, "FINDINGS\n```json\n[]\n```");
        }
        if text.contains("Your area: rest") {
            if has_results {
                return Self::report(model, None);
            }
            return Self::calls(model, &[read("README.md")]);
        }
        let follow_up = text.contains("of at most 2: the host checked the read_file calls");
        if text.contains("Your area: billing") {
            return match (self.scenario, follow_up, has_results) {
                (Scenario::NeverReads, _, true) => Self::report(model, None),
                (Scenario::NeverReads, _, false) => Self::calls(
                    model,
                    &[
                        read("billing/pagination.py"),
                        ("list_dir", serde_json::json!({"path": "billing"})),
                        (
                            "grep",
                            serde_json::json!({"pattern": "get_page", "path": "billing"}),
                        ),
                        (
                            "grep",
                            serde_json::json!({"pattern": "overflow", "path": "billing"}),
                        ),
                    ],
                ),
                (Scenario::SkipsThenReads, false, false) => Self::calls(
                    model,
                    &[
                        read("billing/pagination.py"),
                        read("billing/rates.csv"),
                        (
                            "read_file",
                            serde_json::json!({"path": "billing/rates.csv", "offset": 65536,
                                "limit": 32768}),
                        ),
                    ],
                ),
                (Scenario::SkipsThenReads, false, true) => Self::report(
                    model,
                    Some((
                        "get_page returns one item too many",
                        "billing/pagination.py:2",
                    )),
                ),
                (Scenario::SkipsThenReads, true, false) => {
                    Self::calls(model, &[read("billing/invoice.py")])
                }
                (Scenario::SkipsThenReads, true, true) => Self::report(
                    model,
                    Some(("invoice total ignores the discount", "billing/invoice.py:2")),
                ),
            };
        }
        if text.contains("Your area: notify") {
            return match (self.scenario, has_results) {
                (Scenario::NeverReads, _) => Self::answer(
                    model,
                    "{\"FINDINGS\": [], \"NOT_REACHED\": [\"notify/legacy/old_handler.py\"]}",
                ),
                (Scenario::SkipsThenReads, false) => {
                    Self::calls(model, &[read("notify/webhook.py")])
                }
                (Scenario::SkipsThenReads, true) => Self::report(model, None),
            };
        }
        if text.contains("Plan this audit before it starts") {
            if self.scenario == Scenario::NeverReads
                && self.planner_calls.fetch_add(1, Ordering::SeqCst) == 0
            {
                let calls: Vec<(&str, serde_json::Value)> = (0..129)
                    .map(|_| ("list_dir", serde_json::json!({"path": "."})))
                    .collect();
                return Self::calls(model, &calls);
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
async fn model_server(scenario: Scenario, planner_calls: Arc<AtomicUsize>) -> MockServer {
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
            // Room for a worker to hold two 64 KiB reads.
            serde_json::json!({"models": [{"name": MODEL, "model": MODEL, "digest": digest,
                "details": {"format": "gguf"}, "context_length": 131072}]}),
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
        .respond_with(ScriptedAudit {
            scenario,
            planner_calls,
        })
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

/// Run this module's test `name` in a child process with its own data
/// root and socket; `true` in the parent, which waits for the child and
/// fails with its output when it fails.
async fn in_child(name: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        return false;
    }
    let root = tempfile::tempdir().unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(900),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("bootstrap::loadout_runs::audit_tests::{name}"),
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
    true
}

/// What one audit on Podman left for its test to check.
struct AuditRun {
    outcome: RunOutcome,
    events: Vec<RunEvent>,
    /// Every tool call each turn's Session recorded, by turn purpose.
    calls: Vec<(String, Vec<crate::loadout::ToolCallRecord>)>,
    /// The slot of each recorded call's node, by node id.
    slots: HashMap<String, String>,
    junit: String,
}

impl AuditRun {
    fn phases(&self, name: &str) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|event| match event {
                RunEvent::Phase { phase, detail, .. } if phase == name => Some(detail.clone()),
                _ => None,
            })
            .collect()
    }

    /// `(slot, tool, arguments, succeeded)` of each call of the turns with
    /// `purpose`, sorted.
    fn calls_of(&self, purpose: &str) -> Vec<(String, String, serde_json::Value, bool)> {
        let mut seen: Vec<(String, String, serde_json::Value, bool)> = self
            .calls
            .iter()
            .filter(|(turn, _)| turn == purpose)
            .flat_map(|(_, calls)| calls.iter())
            .map(|call| {
                (
                    self.slots.get(&call.node_id).cloned().unwrap_or_default(),
                    call.tool.clone(),
                    call.arguments.clone(),
                    call.succeeded,
                )
            })
            .collect();
        seen.sort_by(|a, b| (&a.0, &a.1, a.2.to_string()).cmp(&(&b.0, &b.1, b.2.to_string())));
        seen
    }

    fn purposes(&self) -> Vec<&str> {
        self.outcome
            .turns
            .iter()
            .map(|turn| turn.purpose.as_str())
            .collect()
    }
}

/// Run the built-in audit on real Podman against a committed repository of
/// `files`, with the scripted model playing `scenario`, and hand back what
/// it left. Every container, volume and network of the run's Session is
/// removed by exact name afterwards.
async fn audit_on_podman(scenario: Scenario, files: &[(&str, &str)], task: &str) -> AuditRun {
    let planner_calls = Arc::new(AtomicUsize::new(0));
    let server = model_server(scenario, planner_calls.clone()).await;
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
    for (file, text) in files {
        let path = repo.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    // A Git repository, as the re-smoke's fixture was: the host captures
    // the repository around each activation with Git, and lists its files
    // with git ls-files.
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
            task: task.into(),
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
        let mut calls = Vec::new();
        let mut slots = HashMap::new();
        for turn in &outcome.turns {
            if let Some(observation) = daemon
                .loadout_turn_observation(&accepted.session_id, &turn.turn_id, &[], None)
                .await?
            {
                for node in observation.nodes {
                    slots.insert(node.node_id, node.slot_id);
                }
            }
            let recorded = daemon
                .loadout_turn_tool_calls(&accepted.session_id, &turn.turn_id)?
                .unwrap_or_default();
            calls.push((turn.purpose.clone(), recorded));
        }
        let junit = axocoatl_session::run_junit::render_junit(&outcome)
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        if scenario == Scenario::NeverReads {
            assert_eq!(planner_calls.load(Ordering::SeqCst), 2);
        }
        Ok::<AuditRun, DaemonError>(AuditRun {
            outcome,
            events,
            calls,
            slots,
            junit,
        })
    }
    .await;
    let shutdown = daemon.shutdown_session_runtimes_checked().await;
    drop(cleanup);
    let run = result.unwrap();
    shutdown.unwrap();
    run
}

/// Host-verified coverage on real Podman: the billing worker reads two of
/// its three files to read, one of them longer than one read in two reads
/// at offsets, and skips the third; the host, from the `read_file` calls
/// the Session recorded, runs one follow-up of the billing worker alone
/// naming exactly the skipped file; that activation reads it and reports a
/// finding there; the empty `billing/__init__.py` needs no read; the
/// host-made area rest takes `README.md`; and the run passes.
///
/// ```text
/// CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-daemon --lib \
///   an_audit_on_podman_follows_up_a_skipped_file_and_passes -- --ignored
/// ```
#[tokio::test]
#[ignore = "requires rootless Podman, docker.io/library/alpine:3.20 and the egress sidecar image"]
async fn an_audit_on_podman_follows_up_a_skipped_file_and_passes() {
    if in_child("an_audit_on_podman_follows_up_a_skipped_file_and_passes").await {
        return;
    }
    let rates: String = (0..3500)
        .map(|index| format!("code{index:05},{:017}\n", index * 7))
        .collect();
    assert!(rates.len() > 65536 && rates.len() < 65536 + 32768);
    let run = audit_on_podman(
        Scenario::SkipsThenReads,
        &[
            ("README.md", "# Audit fixture\n"),
            ("billing/rates.csv", &rates),
            ("billing/__init__.py", ""),
            (
                "billing/pagination.py",
                "def get_page(items, page, size):\n    return items[page * size:(page + 1) * size + 1]\n",
            ),
            (
                "billing/invoice.py",
                "def total(lines, discount):\n    return sum(line.cents for line in lines)\n",
            ),
            (
                "notify/webhook.py",
                "WEBHOOK = 'https://hooks.example.invalid/orders'\n",
            ),
        ],
        "Find correctness defects in billing and notify.",
    )
    .await;
    let outcome = &run.outcome;
    assert_eq!(
        outcome.exit_code,
        exit_code::PASS,
        "{:?} {:?} {:?}",
        outcome.attention,
        outcome.not_covered,
        run.events
    );
    assert!(outcome.not_covered.is_empty());
    assert_eq!(
        run.purposes(),
        [
            crate::loadout::audit::PLAN_PURPOSE,
            crate::loadout::audit::AREAS_PURPOSE,
            crate::loadout::audit::FOLLOW_UP_PURPOSE,
            crate::loadout::audit::INTEGRATE_PURPOSE
        ]
    );
    // The plan as executed, listed by git ls-files.
    assert_eq!(
        run.phases("assigned"),
        [
            "6 files listed by git ls-files --cached --others --exclude-standard in 3 areas: \
             billing 4, notify 1, rest 1 (host-made)",
            "billing (paths billing/**): billing/__init__.py (empty), billing/invoice.py, \
             billing/pagination.py, billing/rates.csv",
            "notify (paths notify/**): notify/webhook.py",
            "rest (host-made for the files no planned area's paths name): README.md",
        ]
    );
    // What the Session recorded: in the areas turn the billing worker read
    // pagination.py and both windows of rates.csv, not invoice.py; its
    // follow-up read invoice.py.
    let read = |path: &str| serde_json::json!({ "path": path });
    let areas = run.calls_of(crate::loadout::audit::AREAS_PURPOSE);
    // (Sorted by their arguments' JSON, so the windowed read comes first.)
    assert_eq!(
        areas,
        [
            (
                "worker-billing".to_string(),
                "read_file".to_string(),
                serde_json::json!({"path": "billing/rates.csv", "offset": 65536, "limit": 32768}),
                true
            ),
            (
                "worker-billing".to_string(),
                "read_file".to_string(),
                read("billing/pagination.py"),
                true
            ),
            (
                "worker-billing".to_string(),
                "read_file".to_string(),
                read("billing/rates.csv"),
                true
            ),
            (
                "worker-notify".to_string(),
                "read_file".to_string(),
                read("notify/webhook.py"),
                true
            ),
            (
                "worker-rest".to_string(),
                "read_file".to_string(),
                read("README.md"),
                true
            ),
        ],
        "{:?}",
        run.calls
    );
    // The second read of rates.csv, in the hardened Session container,
    // returned the rest of the file from its offset.
    let recorded = &run
        .calls
        .iter()
        .find(|(purpose, _)| purpose == crate::loadout::audit::AREAS_PURPOSE)
        .unwrap()
        .1;
    let windows: Vec<&serde_json::Value> = recorded
        .iter()
        .filter(|call| call.arguments["path"] == "billing/rates.csv")
        .map(|call| &call.result)
        .collect();
    let first = windows
        .iter()
        .find(|result| result.get("offset").is_none())
        .unwrap();
    let second = windows
        .iter()
        .find(|result| result["offset"] == 65536)
        .unwrap();
    assert_eq!(first["truncated"], true, "{first}");
    assert_eq!(first["next_offset"], 65536, "{first}");
    assert_eq!(first["content"].as_str().unwrap(), &rates[..65536]);
    assert_eq!(second["truncated"], false, "{second}");
    assert_eq!(second["output_limit_bytes"], 32768, "{second}");
    assert_eq!(second["content"].as_str().unwrap(), &rates[65536..]);
    assert_eq!(
        run.calls_of(crate::loadout::audit::FOLLOW_UP_PURPOSE),
        [(
            "worker-billing".to_string(),
            "read_file".to_string(),
            read("billing/invoice.py"),
            true
        )]
    );
    assert_eq!(
        run.phases("applying_team")[2],
        "audit follow-up 1: 1 read-only worker (billing (1 unread))"
    );
    assert_eq!(
        run.phases("coverage"),
        [
            "billing: 4 of 4 files examined: 3 read; not read: empty: billing/__init__.py",
            "notify: 1 of 1 files examined: 1 read",
            "rest: 1 of 1 files examined: 1 read",
        ]
    );
    assert_eq!(
        outcome.notes,
        [
            "the host made area rest for 1 file no planned area's paths name and that share no \
          directory with them: README.md"
        ]
    );
    assert!(
        run.junit
            .contains("<testsuite name=\"coverage\" tests=\"0\""),
        "{}",
        run.junit
    );
}

/// The built-in audit on real Podman, as the 1.3.0 re-smoke ran it:
/// - the planner's provider fails on its first call ("too many native tool
///   calls") and the planner gets one more turn, which plans;
/// - the billing worker reads its file, with the arguments and outcomes the
///   Session recorded for its calls (the host's repository captures left
///   out), so billing is covered;
/// - the notify worker answers without a tool call, every time: from the
///   tool calls its Session recorded it read nothing, so it gets two
///   follow-ups naming `notify/webhook.py`, after which that file is not
///   covered, and what it listed as not reached is only a note;
/// - the run needs attention, and its Outcome and JUnit say why.
///
/// ```text
/// CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-daemon --lib \
///   an_audit_on_podman_judges_workers_by_their_recorded_tool_calls -- --ignored
/// ```
#[tokio::test]
#[ignore = "requires rootless Podman, docker.io/library/alpine:3.20 and the egress sidecar image"]
async fn an_audit_on_podman_judges_workers_by_their_recorded_tool_calls() {
    if in_child("an_audit_on_podman_judges_workers_by_their_recorded_tool_calls").await {
        return;
    }
    let run = audit_on_podman(
        Scenario::NeverReads,
        &[
            ("README.md", "# Audit fixture\n"),
            (
                "billing/pagination.py",
                "def get_page(items, page, size):\n    return items[page * size:(page + 1) * size + 1]\n",
            ),
            (
                "notify/webhook.py",
                "WEBHOOK = 'https://hooks.example.invalid/orders'\n",
            ),
        ],
        "Find correctness defects in billing and notify.",
    )
    .await;
    let outcome = &run.outcome;
    let phases: Vec<String> = run.phases("plan_failed");
    // The planner's provider failed once, and its retry planned.
    assert_eq!(phases.len(), 1, "{:?}", run.events);
    assert!(
        phases[0].contains("too many native tool calls")
            && phases[0].ends_with("it gets one more turn"),
        "{phases:?}"
    );
    use crate::loadout::audit::{
        AREAS_PURPOSE, FOLLOW_UP_PURPOSE, INTEGRATE_PURPOSE, PLAN_PURPOSE,
    };
    assert_eq!(
        run.purposes(),
        [
            PLAN_PURPOSE,
            PLAN_PURPOSE,
            AREAS_PURPOSE,
            FOLLOW_UP_PURPOSE,
            FOLLOW_UP_PURPOSE,
            INTEGRATE_PURPOSE
        ],
        "{outcome:?}"
    );

    // What the Session recorded of the workers' calls: the billing
    // worker's read, listing and greps with their arguments and what they
    // returned, the rest worker's read, and nothing of the host's
    // repository captures or of the notify worker.
    let areas = run.calls_of(AREAS_PURPOSE);
    let seen: Vec<(&str, &str, &serde_json::Value, bool)> = areas
        .iter()
        .map(|(slot, tool, arguments, succeeded)| {
            (slot.as_str(), tool.as_str(), arguments, *succeeded)
        })
        .collect();
    assert_eq!(
        seen,
        [
            (
                "worker-billing",
                "grep",
                &serde_json::json!({"pattern": "get_page", "path": "billing"}),
                true
            ),
            (
                "worker-billing",
                "grep",
                &serde_json::json!({"pattern": "overflow", "path": "billing"}),
                true
            ),
            (
                "worker-billing",
                "list_dir",
                &serde_json::json!({"path": "billing"}),
                true
            ),
            (
                "worker-billing",
                "read_file",
                &serde_json::json!({"path": "billing/pagination.py"}),
                true
            ),
            (
                "worker-rest",
                "read_file",
                &serde_json::json!({"path": "README.md"}),
                true
            ),
        ],
        "{:?}",
        run.calls
    );
    assert!(run.calls_of(FOLLOW_UP_PURPOSE).is_empty());
    // Each result as the tool returned it, read from the Session's content
    // store: the grep's matches name the file it matched.
    let recorded = &run
        .calls
        .iter()
        .find(|(purpose, _)| purpose == AREAS_PURPOSE)
        .unwrap()
        .1;
    let result = |tool: &str, pattern: Option<&str>| {
        recorded
            .iter()
            .find(|call| {
                call.tool == tool
                    && run.slots.get(&call.node_id).map(String::as_str) == Some("worker-billing")
                    && pattern.is_none_or(|pattern| call.arguments["pattern"] == pattern)
            })
            .map(|call| call.result.clone())
            .unwrap()
    };
    assert!(
        result("read_file", None)["content"]
            .as_str()
            .is_some_and(|content| content.starts_with("def get_page(")),
        "{recorded:?}"
    );
    assert!(
        result("list_dir", None)["listing"]
            .as_str()
            .is_some_and(|listing| listing.contains("pagination.py")),
        "{recorded:?}"
    );
    assert_eq!(
        result("grep", Some("get_page"))["matches"],
        "billing/pagination.py:1:def get_page(items, page, size):\n",
        "{recorded:?}"
    );
    assert_eq!(result("grep", Some("overflow"))["matches"], "");

    // Billing and rest are covered; notify, which read nothing in its turn
    // or either follow-up, is not, by file; what it listed is a note.
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
        [(
            "notify",
            FailureClass::NotReached,
            "notify/webhook.py: not read (the area worker did not read it in its turn or its 2 \
             follow-ups)"
        )],
        "{outcome:?}"
    );
    assert_eq!(outcome.attention, ["1 area was not covered"]);
    let listed = "worker-notify listed as not reached: notify/legacy/old_handler.py; a note: the \
                  host decides coverage from the files its workers read";
    assert_eq!(
        outcome
            .notes
            .iter()
            .filter(|note| note.as_str() == listed)
            .count(),
        3,
        "{:?}",
        outcome.notes
    );
    assert!(
        run.junit.contains("name=\"notify\"") && run.junit.contains("notify/webhook.py: not read"),
        "{}",
        run.junit
    );
}
