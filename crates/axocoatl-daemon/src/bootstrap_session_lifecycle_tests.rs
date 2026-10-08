//! Session lifecycle on a live daemon, as the 1.3 smoke runs found it:
//! closing an idle Session or creating another while another Session's turn
//! or another operation holds their Workspace, what Close leaves on Podman,
//! the runtime volumes a daemon removes when it starts (and another
//! daemon's it leaves), and a data directory made before the daemon first
//! started. Each body runs in a child process with its own data root,
//! because bootstrap reads the process environment.
use super::*;
use crate::loadout::api::{RunAccepted, RunRequest};
use crate::loadout::host::{CheckLabel, ReproRequest};
use crate::loadout::{RunContext, RunError, RunHost};
use crate::RuntimeVolumeCheck;
use axocoatl_session::run_outcome::{NetworkSummary, ReproRun, RunOutcome, TurnObservation};
use axocoatl_session::run_outcome::{TurnState, RUN_OUTCOME_SCHEMA};
use axocoatl_session::run_record::RunEvent;
use std::os::unix::fs::PermissionsExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHILD: &str = "AXOCOATL_LIFECYCLE_TEST_CHILD";
const MODEL: &str = "lifecycle-model:latest";
/// A task the model never finishes answering while the test runs: its
/// turn holds the Workspace until the test stops it.
const STALL: &str = "Hold the Workspace until you are stopped.";
const USER_LOADOUT_DIR: &str = "loadouts";

/// One writer that may only read files, no checks and no review: its turn
/// completes as soon as the model answers.
const LOADOUT: &str = r#"
schema: axocoatl.loadout/1
id: lifecycle
version: 1
name: Lifecycle
kind: custom
params:
  writer_model: { kind: model, required: true }
agents:
  - id: writer
    role: writer
    model: { param: writer_model }
    tools: [read_file]
budgets:
  agent: { activations: 2, invocations: 20, tokens: 100000, cost_usd: 1 }
  wall_clock: 10m
prompt: "{task}"
"#;

/// The audited local Ollama the native provider admits. `/api/chat`
/// answers "Done." at once, except for [`STALL`], which it holds for five
/// minutes.
async fn model_server() -> MockServer {
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
        .respond_with(|request: &wiremock::Request| {
            if String::from_utf8_lossy(&request.body).contains(STALL) {
                return ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(300))
                    .set_body_raw("{}\n", "application/x-ndjson");
            }
            let reply = serde_json::json!({
                "model": MODEL, "created_at": "2026-10-07T00:00:00Z",
                "message": {"role": "assistant", "content": "Done."},
                "done": true, "done_reason": "stop", "prompt_eval_count": 20, "eval_count": 2
            });
            ResponseTemplate::new(200).set_body_raw(format!("{reply}\n"), "application/x-ndjson")
        })
        .mount(&server)
        .await;
    server
}

/// What the task that sent each turn returned, by turn id (`None` while it
/// runs).
type Sends = Arc<std::sync::Mutex<HashMap<String, Option<Result<(), String>>>>>;

/// The run driver's host over this daemon, as the server's run host is:
/// each turn is sent as `/ws` sends it and observed through the
/// control-plane projection.
struct Host {
    daemon: Arc<AxocoatlDaemon>,
    labels: std::sync::Mutex<Vec<CheckLabel>>,
    sends: Sends,
}

impl Host {
    fn new(daemon: &Arc<AxocoatlDaemon>) -> Self {
        Self {
            daemon: daemon.clone(),
            labels: std::sync::Mutex::new(Vec::new()),
            sends: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait::async_trait]
impl RunHost for Host {
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

    async fn run_repro(&self, _: &str, _: &ReproRequest) -> Result<ReproRun, RunError> {
        Err(RunError::NotImplemented("this loadout reproduces nothing"))
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

/// Run `name` in a child process whose data root is `data` inside a new
/// temporary directory, with `prepare` run on that directory first; the
/// PATH it returns, if any, is the child's.
async fn run_child(name: &str, prepare: impl FnOnce(&std::path::Path) -> Option<String>) {
    let root = tempfile::tempdir().unwrap();
    let path = prepare(root.path());
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", name, "--nocapture", "--include-ignored"])
        .env(CHILD, "1")
        .env("AXOCOATL_DATA_DIR", root.path().join("data"))
        .env("AXOCOATL_SOCKET_PATH", "ipc/daemon.sock")
        .current_dir(root.path())
        .kill_on_drop(true);
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let result = tokio::time::timeout(Duration::from_secs(900), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

/// A fake Podman for bodies that never start a container: it reports a
/// running machine and rootless Podman.
fn fake_podman(root: &std::path::Path) -> String {
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let podman = bin.join("podman");
    std::fs::write(
        &podman,
        r#"#!/bin/sh
case "$*" in
  --version) printf 'podman version 5.0.0\n' ;;
  'machine list --format json') printf '[{"Running":true}]\n' ;;
  'info --format json') printf '{}\n' ;;
  'info --format {{.Host.Security.Rootless}}') printf 'true\n' ;;
  'ps '*) ;;
  'rm '*|'volume rm '*|'network rm '*) ;;
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
    format!("{}:/usr/bin:/bin", bin.display())
}

/// The configured Agent a workbench Session is created with.
const HELPER: &str = "helper";

/// The daemon's configuration, with the lifecycle loadout beside it. Its one
/// configured Agent ([`HELPER`]) is what a workbench Session selects.
async fn load_config(dir: &std::path::Path, model_url: &str) -> AxocoatlConfig {
    let config_path = dir.join("axocoatl.yaml");
    std::fs::write(
        &config_path,
        format!(
            "agents:\n  - id: {HELPER}\n    name: Helper\n    provider: ollama\n    model: {MODEL}\n    role: autonomous\n    tools: [read_file]\nproviders:\n  ollama:\n    base_url: {model_url}\nsandbox:\n  backend: podman\n  network: bridge\nconsolidation:\n  enabled: false\n"
        ),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join(USER_LOADOUT_DIR)).unwrap();
    std::fs::write(dir.join(USER_LOADOUT_DIR).join("lifecycle.yaml"), LOADOUT).unwrap();
    axocoatl_config::load_config(&config_path).await.unwrap()
}

fn run_request(repo: &std::path::Path, task: &str, id: &str) -> RunRequest {
    RunRequest {
        loadout: "lifecycle".into(),
        task: task.into(),
        repo: repo.display().to_string(),
        params: [("writer_model".to_string(), format!("ollama:{MODEL}"))]
            .into_iter()
            .collect(),
        keep: Default::default(),
        check_command: None,
        setup_command: None,
        request_id: id.into(),
    }
}

/// A data root that a daemon already owned (its regular format lock), and
/// so still uses the 1.0 Session format, refuses a run with the command
/// that upgrades it. A directory made before the daemon first started, such
/// as `$W/data` in the smoke runs, holds no Session and starts native (see
/// `bootstrap::session_recovery::tests`).
async fn legacy_root_body() {
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("axocoatl.yaml");
    let config = load_config(config_dir.path(), "http://127.0.0.1:9").await;
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    daemon.set_config_path(&config_path);
    assert!(!daemon.uses_native_session_history());
    let repo = tempfile::tempdir().unwrap();
    let error = daemon
        .admit_loadout_run(run_request(repo.path(), "Say done.", "legacy"))
        .await
        .map(|(accepted, _)| accepted.run_id)
        .unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("`axocoatl session upgrade --confirm`"),
        "{message}"
    );
    assert!(message.contains("1.0 Session format"), "{message}");
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_run_on_a_legacy_data_root_names_the_upgrade_command() {
    if std::env::var_os(CHILD).is_some() {
        legacy_root_body().await;
        return;
    }
    run_child(
        "bootstrap::session_native_lifecycle::lifecycle_tests::a_run_on_a_legacy_data_root_names_the_upgrade_command",
        |root| {
            let data = root.join("data");
            std::fs::create_dir(&data).unwrap();
            std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
            // The format boundary a 1.0 daemon leaves: a regular lock file.
            std::fs::write(
                data.join(axocoatl_session::execution_ownership::LEGACY_LOCK_NAME),
                b"",
            )
            .unwrap();
            Some(fake_podman(root))
        },
    )
    .await;
}

/// With AXOCOATL_E2E_MODEL_CACHE set to a directory holding the
/// all-MiniLM-L6-v2 files, put them in the child's data root (made
/// owner-only first) so its configured Agent's memory does not download
/// them.
fn copy_model_cache(root: &std::path::Path) -> Option<String> {
    let cache = std::path::PathBuf::from(std::env::var_os("AXOCOATL_E2E_MODEL_CACHE")?);
    let files = ["config.json", "tokenizer.json", "model.safetensors"];
    if !files.iter().all(|name| cache.join(name).is_file()) {
        return None;
    }
    let data = root.join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let models = data.join("models").join("all-MiniLM-L6-v2");
    std::fs::create_dir_all(&models).unwrap();
    for name in files {
        std::fs::copy(cache.join(name), models.join(name)).unwrap();
    }
    None
}

/// Whether Podman has a volume named `name`.
async fn volume_exists(name: &str) -> bool {
    let status = tokio::process::Command::new("podman")
        .args(["volume", "exists", name])
        .status()
        .await
        .unwrap();
    match status.code() {
        Some(0) => true,
        Some(1) => false,
        other => panic!("podman volume exists {name}: {other:?}"),
    }
}

async fn container_exists(name: &str) -> bool {
    let status = tokio::process::Command::new("podman")
        .args(["container", "exists", name])
        .status()
        .await
        .unwrap();
    status.code() == Some(0)
}

/// Removes what the test's Sessions left on Podman, by exact name, whatever
/// an assertion did.
struct Cleanup(std::sync::Mutex<Vec<String>>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for id in self
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
        {
            let containers: Vec<String> =
                ["axo-ses-", "axo-egr-", "axo-brw-", "axo-pvw-", "axo-svc-"]
                    .iter()
                    .map(|prefix| format!("{prefix}{id}"))
                    .collect();
            let _ = std::process::Command::new("podman")
                .args(["rm", "-f", "--ignore"])
                .args(&containers)
                .output();
            let volumes = SessionSandbox::runtime_volume_names(id)
                .into_iter()
                .chain([SessionSandbox::dependency_volume_name(id)]);
            for volume in volumes {
                let _ = std::process::Command::new("podman")
                    .args(["volume", "rm", "-f", &volume])
                    .output();
            }
        }
    }
}

/// The live case of the qa smoke run, on real Podman: run A finished and
/// its Session is idle; run B's turn holds the same Workspace.
/// - Closing or deleting A is refused at once (not after 60 s) with a busy
///   Workspace that names B's Session and turn, and changes nothing; a new
///   turn of A, a third run on the repository, a new Session on it (both
///   ways of creating one; not after B's whole turn) and a file change in A
///   are refused the same way.
/// - Once B is stopped, an operation that is not a turn holds the
///   Workspace: Close and Delete refuse at once, naming it, and change
///   nothing; once it is released, Close goes on.
/// - Once B is stopped, A closes. Close removes A's runtime volumes (egress
///   socket, identity socket, service socket, trust) and keeps its Node
///   dependency volume; Reopen starts A again with new runtime volumes, and
///   Delete removes the dependency volume too.
///
/// ```text
/// CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-daemon --lib \
///   an_idle_session_closes_once_another_sessions_turn_lets_go -- --ignored
/// ```
///
/// Set AXOCOATL_E2E_MODEL_CACHE to a directory with the all-MiniLM-L6-v2
/// files to avoid downloading the embedding model.
#[tokio::test]
#[ignore = "requires rootless Podman, docker.io/library/alpine:3.20 and the egress sidecar image"]
async fn an_idle_session_closes_once_another_sessions_turn_lets_go() {
    if std::env::var_os(CHILD).is_none() {
        run_child(
            "bootstrap::session_native_lifecycle::lifecycle_tests::an_idle_session_closes_once_another_sessions_turn_lets_go",
            copy_model_cache,
        )
        .await;
        return;
    }
    let server = model_server().await;
    let config_dir = tempfile::tempdir().unwrap();
    let config = load_config(config_dir.path(), &server.uri()).await;
    let daemon = Arc::new(AxocoatlDaemon::bootstrap_headless(config).await.unwrap());
    daemon.set_config_path(&config_dir.path().join("axocoatl.yaml"));
    // Outside the data root, which the containers must not reach. A root
    // Node project, so each Session also has a dependency volume.
    let outside = tempfile::Builder::new()
        .prefix("axocoatl-lifecycle-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let repo = outside.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# Lifecycle\n").unwrap();
    std::fs::write(repo.join("package.json"), "{\"name\": \"lifecycle\"}\n").unwrap();
    let cleanup = Cleanup(std::sync::Mutex::new(Vec::new()));

    let result = async {
        // Run A: its turn completes, and its Session is idle.
        let (a, a_context) = daemon
            .admit_loadout_run(run_request(&repo, "Say done.", "run-a"))
            .await?;
        cleanup.0.lock().unwrap().push(a.session_id.clone());
        let outcome = crate::loadout::driver::run_to_outcome(&Host::new(&daemon), &a_context)
            .await
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        assert_eq!(outcome.schema, RUN_OUTCOME_SCHEMA);
        assert_eq!(outcome.turns.len(), 1, "{outcome:?}");
        assert_eq!(outcome.turns[0].state, TurnState::Completed, "{outcome:?}");
        let a_id = a.session_id.clone();
        assert!(volume_exists(&format!("axo-egr-{a_id}")).await);
        assert!(volume_exists(&SessionSandbox::dependency_volume_name(&a_id)).await);

        // Run B: its turn holds the Workspace until the test stops it.
        let (b, b_context) = daemon
            .admit_loadout_run(run_request(&repo, STALL, "run-b"))
            .await?;
        cleanup.0.lock().unwrap().push(b.session_id.clone());
        let b_driver = {
            let host = Host::new(&daemon);
            tokio::spawn(
                async move { crate::loadout::driver::run_to_outcome(&host, &b_context).await },
            )
        };
        let started = tokio::time::Instant::now();
        loop {
            let calls = server.received_requests().await.unwrap_or_default();
            if calls.iter().any(|request| {
                request.url.path() == "/api/chat"
                    && String::from_utf8_lossy(&request.body).contains(STALL)
            }) {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(180),
                "run B's provider call never started"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        // Closing or deleting idle A is refused at once, naming B's turn.
        for (action, force) in [("close", false), ("delete", true)] {
            let asked = tokio::time::Instant::now();
            let refused = tokio::time::timeout(Duration::from_secs(20), async {
                if force {
                    daemon.delete_session(&a_id).await
                } else {
                    daemon.close_session(&a_id).await
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{action} of the idle Session waited for the Workspace"));
            let message = match refused {
                Err(DaemonError::WorkspaceBusy(message)) => message,
                other => panic!("{action}: expected a busy Workspace, got {other:?}"),
            };
            assert!(
                asked.elapsed() < Duration::from_secs(10),
                "{action} took {:?}",
                asked.elapsed()
            );
            assert!(
                message.contains(&format!("Session {a_id} was not {action}d")),
                "{message}"
            );
            assert!(message.contains(&b.session_id), "{message}");
            assert!(message.contains("is running"), "{message}");
            assert!(!message.contains("cleanup did not finish"), "{message}");
        }
        // Nothing changed: A is still open, and its container still runs.
        let session = daemon.get_session(&a_id).await.unwrap();
        assert_eq!(session.status, axocoatl_session::SessionStatus::Active);
        assert!(container_exists(&format!("axo-ses-{a_id}")).await);
        // A new turn of A cannot start either: busy, not an infrastructure
        // failure, which is what a run's next turn gets (exit 7).
        let (sink, receiver) =
            tokio::sync::mpsc::unbounded_channel::<axocoatl_actor::AgentStreamChunk>();
        drop(receiver);
        let turn = format!("turn-{}", uuid::Uuid::new_v4());
        let refused = tokio::time::timeout(
            Duration::from_secs(60),
            daemon.execute_session_turn_streaming(
                &a_id,
                &turn,
                Some(turn.clone()),
                None,
                "Say done again.",
                Vec::new(),
                Vec::new(),
                None,
                None,
                sink,
            ),
        )
        .await
        .expect("a turn start never waits for the Workspace");
        match refused {
            Err(error @ DaemonError::WorkspaceBusy(_)) => {
                let error = RunError::from(error);
                assert!(matches!(error, RunError::Busy(_)), "{error}");
            }
            Err(other) => panic!("expected a busy Workspace, got {other}"),
            Ok(_) => panic!("a turn of A started while B held the Workspace"),
        }
        // A third run on the repository is refused at once, naming B.
        match daemon
            .admit_loadout_run(run_request(&repo, "Say done.", "run-c"))
            .await
            .map(|(accepted, _): (RunAccepted, RunContext)| accepted)
        {
            Err(DaemonError::WorkspaceBusy(message)) => {
                assert!(message.contains(&b.session_id), "{message}")
            }
            Err(other) => panic!("expected a busy Workspace, got {other}"),
            Ok(accepted) => panic!("admitted {}", accepted.run_id),
        }
        // A new Session on the repository is refused at once, naming B, by
        // path and by Workspace, and none is created.
        let workspace_id = daemon.get_session(&a_id).await.unwrap().workspace_id;
        let sessions = daemon.list_sessions().await.len();
        for by_workspace in [false, true] {
            let asked = tokio::time::Instant::now();
            let mode = SessionMode::SingleAgent {
                agent_id: HELPER.into(),
            };
            let created = tokio::time::timeout(Duration::from_secs(60), async {
                if by_workspace {
                    daemon
                        .create_session_in_workspace(
                            &workspace_id,
                            "probe",
                            mode,
                            Vec::new(),
                            Vec::new(),
                            None,
                            None,
                            false,
                            true,
                        )
                        .await
                } else {
                    daemon
                        .create_session(
                            "probe",
                            &repo.display().to_string(),
                            mode,
                            Vec::new(),
                            Vec::new(),
                            None,
                        )
                        .await
                }
            })
            .await
            .expect("creating a Session never waits for another Session's turn");
            match created {
                Err(DaemonError::WorkspaceBusy(message)) => {
                    assert!(
                        message.starts_with("No Session was created: its Workspace "),
                        "{message}"
                    );
                    assert!(message.contains(&b.session_id), "{message}");
                    assert!(message.contains(&b.run_id), "{message}");
                    assert!(message.contains("is running"), "{message}");
                }
                Err(other) => panic!("expected a busy Workspace, got {other}"),
                Ok(session) => panic!("created {}", session.id),
            }
            assert!(
                asked.elapsed() < Duration::from_secs(10),
                "{:?}",
                asked.elapsed()
            );
        }
        assert_eq!(daemon.list_sessions().await.len(), sessions);
        // So is a change to A's files: it would wait for B's whole turn.
        match tokio::time::timeout(
            Duration::from_secs(20),
            daemon.session_write_file(&a_id, "README.md", "# Changed\n"),
        )
        .await
        .expect("a file change never waits for another Session's turn")
        {
            Err(DaemonError::WorkspaceBusy(message)) => {
                assert!(
                    message.starts_with("The change was not made: its Workspace "),
                    "{message}"
                );
                assert!(message.contains(&b.session_id), "{message}");
            }
            other => panic!("expected a busy Workspace, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(repo.join("README.md")).unwrap(),
            "# Lifecycle\n"
        );

        // B is stopped; A then closes at once.
        daemon.request_loadout_run_stop(&b.run_id).await?;
        let b_outcome = tokio::time::timeout(Duration::from_secs(180), b_driver)
            .await
            .expect("the stopped run ends")
            .unwrap()
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        assert_eq!(
            b_outcome.exit_code,
            axocoatl_session::run_outcome::exit_code::INTERRUPTED,
            "{b_outcome:?}"
        );
        // An operation that is not a turn holds the Workspace: Close and
        // Delete refuse at once (they never wait for another operation),
        // naming it, and change nothing.
        let held = daemon
            .take_lifecycle_workspace_operation(
                &workspace_id,
                super::workspace_operation::WorkspaceRequest {
                    doing: "the test's own operation".into(),
                    refused: "unused".into(),
                },
            )
            .await?;
        for action in ["close", "delete"] {
            let asked = tokio::time::Instant::now();
            let refused = if action == "close" {
                daemon.close_session(&a_id).await
            } else {
                daemon.delete_session(&a_id).await
            };
            match refused {
                Err(DaemonError::WorkspaceBusy(message)) => {
                    assert!(
                        message.starts_with(&format!(
                            "Session {a_id} was not {action}d: its Workspace "
                        )),
                        "{message}"
                    );
                    assert!(
                        message.contains("is held by another operation: the test's own operation"),
                        "{message}"
                    );
                    assert!(!message.contains("did not end within"), "{message}");
                }
                other => panic!("{action}: expected a busy Workspace, got {other:?}"),
            }
            assert!(
                asked.elapsed() < Duration::from_secs(2),
                "{action} took {:?}",
                asked.elapsed()
            );
        }
        assert_eq!(
            daemon.get_session(&a_id).await.unwrap().status,
            axocoatl_session::SessionStatus::Active
        );
        assert!(container_exists(&format!("axo-ses-{a_id}")).await);
        // Released: Close goes on at once.
        drop(held);
        let asked = tokio::time::Instant::now();
        daemon.close_session(&a_id).await?;
        assert!(
            asked.elapsed() < Duration::from_secs(30),
            "{:?}",
            asked.elapsed()
        );

        // Close removed A's containers and runtime volumes, and kept its
        // dependency volume for Reopen.
        assert!(!container_exists(&format!("axo-ses-{a_id}")).await);
        for volume in SessionSandbox::runtime_volume_names(&a_id) {
            assert!(!volume_exists(&volume).await, "{volume} survived Close");
        }
        assert!(volume_exists(&SessionSandbox::dependency_volume_name(&a_id)).await);

        // Reopen starts A again with new runtime volumes; closing it again
        // removes them again.
        let reopened = daemon.reopen_session(&a_id).await?;
        assert_eq!(reopened.status, axocoatl_session::SessionStatus::Active);
        assert!(container_exists(&format!("axo-ses-{a_id}")).await);
        assert!(volume_exists(&format!("axo-egr-{a_id}")).await);
        assert!(volume_exists(&format!("axo-egi-{a_id}")).await);
        daemon.close_session(&a_id).await?;
        for volume in SessionSandbox::runtime_volume_names(&a_id) {
            assert!(!volume_exists(&volume).await, "{volume} survived Close");
        }

        // B closes the same way.
        daemon.close_session(&b.session_id).await?;
        for volume in SessionSandbox::runtime_volume_names(&b.session_id) {
            assert!(!volume_exists(&volume).await, "{volume} survived Close");
        }

        // Delete removes the dependency volume as well.
        daemon.delete_session(&a_id).await?;
        assert!(!volume_exists(&SessionSandbox::dependency_volume_name(&a_id)).await);
        Ok::<(), DaemonError>(())
    }
    .await;
    let shutdown = daemon.shutdown_session_runtimes_checked().await;
    drop(cleanup);
    result.unwrap();
    shutdown.unwrap();
}

/// Wait until run `stall`'s provider call has started: its turn then holds
/// the Workspace until it is stopped.
async fn wait_for_stalled_call(server: &MockServer) {
    let started = tokio::time::Instant::now();
    loop {
        let calls = server.received_requests().await.unwrap_or_default();
        if calls.iter().any(|request| {
            request.url.path() == "/api/chat"
                && String::from_utf8_lossy(&request.body).contains(STALL)
        }) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(180),
            "the stalled provider call never started"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Every lifecycle action of a Session on real Podman, while an operation
/// that is not another Session's turn holds the Workspace:
/// - a file change of another Session (the real writer lease a file write
///   holds): Close, Delete and an environment change of the idle Session,
///   and a new Session on the Workspace, are refused at once (not after 10
///   or 60 s), naming that change, and nothing changes;
/// - a file change of the Session itself: Close is refused at once the same
///   way; and so is Reopen of a closed Session during another's file change;
/// - its own turn: Close stops the turn and closes the Session, waiting only
///   for the turn's command to reach a safe point; a second Close of it at
///   the same time is refused at once or finds it closed, never waits.
///
/// ```text
/// CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-daemon --lib \
///   lifecycle_actions_never_wait_for_another_operation -- --ignored
/// ```
#[tokio::test]
#[ignore = "requires rootless Podman, docker.io/library/alpine:3.20 and the egress sidecar image"]
async fn lifecycle_actions_never_wait_for_another_operation() {
    if std::env::var_os(CHILD).is_none() {
        run_child(
            "bootstrap::session_native_lifecycle::lifecycle_tests::lifecycle_actions_never_wait_for_another_operation",
            copy_model_cache,
        )
        .await;
        return;
    }
    let server = model_server().await;
    let config_dir = tempfile::tempdir().unwrap();
    let config = load_config(config_dir.path(), &server.uri()).await;
    let daemon = Arc::new(AxocoatlDaemon::bootstrap_headless(config).await.unwrap());
    daemon.set_config_path(&config_dir.path().join("axocoatl.yaml"));
    let outside = tempfile::Builder::new()
        .prefix("axocoatl-lifecycle-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let repo = outside.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# Lifecycle\n").unwrap();
    let cleanup = Cleanup(std::sync::Mutex::new(Vec::new()));

    let result = async {
        // A: a run's Session, idle between turns (its registration released
        // the Workspace). B: a workbench Session on the same Workspace.
        let (a, a_context) = daemon
            .admit_loadout_run(run_request(&repo, "Say done.", "run-a"))
            .await?;
        cleanup.0.lock().unwrap().push(a.session_id.clone());
        let outcome = crate::loadout::driver::run_to_outcome(&Host::new(&daemon), &a_context)
            .await
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        assert_eq!(outcome.turns[0].state, TurnState::Completed, "{outcome:?}");
        let a_id = a.session_id.clone();
        let mode = || SessionMode::SingleAgent {
            agent_id: HELPER.into(),
        };
        let b = daemon
            .create_session_with_environment(
                "b",
                &repo.display().to_string(),
                mode(),
                Vec::new(),
                Vec::new(),
                None,
                None,
                true,
                true,
            )
            .await?;
        cleanup.0.lock().unwrap().push(b.id.clone());
        assert_eq!(
            b.environment.state,
            SessionEnvironmentState::Ready,
            "{:?}",
            b.environment
        );
        let workspace_id = b.workspace_id.clone();
        let sessions = daemon.list_sessions().await.len();

        // B's file change holds the Workspace (the lease a file write holds
        // while it runs).
        let change = daemon.session_writer(&b.id).await?;
        let holder = format!(
            "is held by another operation: a change to Session {}'s files or Git",
            b.id
        );
        let refused_at_once =
            |what: &str, result: Result<(), DaemonError>, asked: tokio::time::Instant| {
                match result {
                    Err(DaemonError::WorkspaceBusy(message)) => {
                        assert!(message.contains(&holder), "{what}: {message}");
                        assert!(!message.contains("did not end within"), "{what}: {message}");
                    }
                    other => panic!("{what}: expected a busy Workspace, got {other:?}"),
                }
                assert!(
                    asked.elapsed() < Duration::from_secs(2),
                    "{what} took {:?}",
                    asked.elapsed()
                );
            };
        let asked = tokio::time::Instant::now();
        refused_at_once("close", daemon.close_session(&a_id).await, asked);
        let asked = tokio::time::Instant::now();
        refused_at_once("delete", daemon.delete_session(&a_id).await, asked);
        let asked = tokio::time::Instant::now();
        let session = daemon.get_session(&a_id).await.unwrap();
        refused_at_once(
            "an environment change",
            daemon
                .configure_session_environment(
                    &a_id,
                    session.image.clone(),
                    session.environment.setup_command.clone(),
                    session.environment.setup_approved,
                    session.environment.setup_reviewed,
                )
                .await
                .map(|_| ()),
            asked,
        );
        let asked = tokio::time::Instant::now();
        refused_at_once(
            "a new Session",
            daemon
                .create_session_in_workspace(
                    &workspace_id,
                    "probe",
                    mode(),
                    Vec::new(),
                    Vec::new(),
                    None,
                    None,
                    false,
                    true,
                )
                .await
                .map(|_| ()),
            asked,
        );
        // Nothing changed: A is open, its container runs, and no Session was
        // created.
        assert_eq!(
            daemon.get_session(&a_id).await.unwrap().status,
            axocoatl_session::SessionStatus::Active
        );
        assert!(container_exists(&format!("axo-ses-{a_id}")).await);
        assert_eq!(daemon.list_sessions().await.len(), sessions);
        drop(change);

        // B's own file change: Close of B is refused at once, naming it.
        let change = daemon.session_writer(&b.id).await?;
        let asked = tokio::time::Instant::now();
        refused_at_once("close of B", daemon.close_session(&b.id).await, asked);
        drop(change);

        // B closes. Reopening it while A's file change holds the Workspace is
        // refused at once, naming that change, and B stays closed.
        daemon.close_session(&b.id).await?;
        let change = daemon.session_writer(&a_id).await?;
        let asked = tokio::time::Instant::now();
        match daemon.reopen_session(&b.id).await {
            Err(DaemonError::WorkspaceBusy(message)) => {
                assert!(
                    message.starts_with(&format!(
                        "Session {} was not reopened: its Workspace ",
                        b.id
                    )),
                    "{message}"
                );
                assert!(
                    message.contains(&format!(
                        "is held by another operation: a change to Session {a_id}'s files or Git"
                    )),
                    "{message}"
                );
            }
            other => panic!("reopen: expected a busy Workspace, got {other:?}"),
        }
        assert!(
            asked.elapsed() < Duration::from_secs(2),
            "{:?}",
            asked.elapsed()
        );
        assert_eq!(
            daemon.get_session(&b.id).await.unwrap().status,
            axocoatl_session::SessionStatus::Closed
        );
        drop(change);

        // C, a run whose turn stalls, is admitted.
        let (c, c_context) = daemon
            .admit_loadout_run(run_request(&repo, STALL, "run-c"))
            .await?;
        cleanup.0.lock().unwrap().push(c.session_id.clone());
        let c_driver = {
            let host = Host::new(&daemon);
            tokio::spawn(
                async move { crate::loadout::driver::run_to_outcome(&host, &c_context).await },
            )
        };
        wait_for_stalled_call(&server).await;
        // Closing C stops its own turn and closes it, waiting only for that
        // turn; a second Close of C at the same time never waits for the
        // first.
        let asked = tokio::time::Instant::now();
        let (first, second) = tokio::join!(daemon.close_session(&c.session_id), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let asked = tokio::time::Instant::now();
            (daemon.close_session(&c.session_id).await, asked.elapsed())
        });
        let closed_in = asked.elapsed();
        eprintln!("Close of C during its own turn took {closed_in:?}");
        first?;
        match second {
            (Ok(()), _) => {}
            (Err(DaemonError::WorkspaceBusy(message)), elapsed) => {
                assert!(
                    message.starts_with(&format!(
                        "Session {} was not closed: another Close, Delete or environment change \
                         of Session {} is in progress",
                        c.session_id, c.session_id
                    )),
                    "{message}"
                );
                assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
            }
            (Err(other), _) => panic!("second Close: {other}"),
        }
        assert!(closed_in < Duration::from_secs(30), "{closed_in:?}");
        assert_eq!(
            daemon.get_session(&c.session_id).await.unwrap().status,
            axocoatl_session::SessionStatus::Closed
        );
        for volume in SessionSandbox::runtime_volume_names(&c.session_id) {
            assert!(!volume_exists(&volume).await, "{volume} survived Close");
        }
        let c_outcome = tokio::time::timeout(Duration::from_secs(180), c_driver)
            .await
            .expect("the closed run ends")
            .unwrap();
        eprintln!("run C after Close: {c_outcome:?}");

        // A closes at once now that nothing holds the Workspace.
        daemon.close_session(&a_id).await?;
        for session in [&a_id, &b.id, &c.session_id] {
            daemon.delete_session(session).await?;
        }
        Ok::<(), DaemonError>(())
    }
    .await;
    let shutdown = daemon.shutdown_session_runtimes_checked().await;
    drop(cleanup);
    result.unwrap();
    shutdown.unwrap();
}

/// The phase a child of [`leaked_runtime_volumes_are_reaped_by_their_own_daemon_only`]
/// or [`leaked_dependency_volumes_are_reaped_by_their_own_daemon_only`]
/// runs, and the directory its phases share their Session ids through.
const PHASE: &str = "AXOCOATL_LIFECYCLE_TEST_PHASE";
const SHARED: &str = "AXOCOATL_LIFECYCLE_TEST_SHARED";
const REAP_TEST: &str = "bootstrap::session_native_lifecycle::lifecycle_tests::leaked_runtime_volumes_are_reaped_by_their_own_daemon_only";
const DEPENDENCY_REAP_TEST: &str = "bootstrap::session_native_lifecycle::lifecycle_tests::leaked_dependency_volumes_are_reaped_by_their_own_daemon_only";

/// Make `data` a data root no daemon has used yet (owner-only), with the
/// embedding model from AXOCOATL_E2E_MODEL_CACHE when it is set.
fn fresh_data_root(data: &std::path::Path) {
    std::fs::create_dir_all(data).unwrap();
    std::fs::set_permissions(data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let Some(cache) = std::env::var_os("AXOCOATL_E2E_MODEL_CACHE") else {
        return;
    };
    let cache = std::path::PathBuf::from(cache);
    let models = data.join("models").join("all-MiniLM-L6-v2");
    for name in ["config.json", "tokenizer.json", "model.safetensors"] {
        if cache.join(name).is_file() {
            std::fs::create_dir_all(&models).unwrap();
            std::fs::copy(cache.join(name), models.join(name)).unwrap();
        }
    }
}

/// Run `phase` of the reap test `test` in a child whose data root is
/// `data`, in `cwd`, sharing Session ids through `shared`.
async fn run_reap_phase(
    test: &str,
    phase: &str,
    data: &std::path::Path,
    cwd: &std::path::Path,
    shared: &std::path::Path,
) {
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture", "--include-ignored"])
        .env(CHILD, "1")
        .env(PHASE, phase)
        .env(SHARED, shared)
        .env("AXOCOATL_DATA_DIR", data)
        .env("AXOCOATL_SOCKET_PATH", "ipc/daemon.sock")
        .current_dir(cwd)
        .kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(600), command.output())
        .await
        .unwrap()
        .unwrap();
    let stderr = String::from_utf8_lossy(&result.stderr);
    for line in stderr.lines().filter(|line| line.starts_with("phase ")) {
        eprintln!("{line}");
    }
    assert!(
        result.status.success(),
        "phase {phase}:\n{}\n{stderr}",
        String::from_utf8_lossy(&result.stdout)
    );
}

fn read_shared(name: &str) -> String {
    let shared = std::path::PathBuf::from(std::env::var_os(SHARED).unwrap());
    std::fs::read_to_string(shared.join(name)).unwrap()
}

fn write_shared(name: &str, value: &str) {
    let shared = std::path::PathBuf::from(std::env::var_os(SHARED).unwrap());
    std::fs::write(shared.join(name), value).unwrap();
}

/// A run whose turn completes at once, on `repo`: its Session (under
/// `network: egress`) is left open with its runtime volumes.
async fn completed_run(daemon: &Arc<AxocoatlDaemon>, repo: &std::path::Path, id: &str) -> String {
    let (accepted, context) = daemon
        .admit_loadout_run(run_request(repo, "Say done.", id))
        .await
        .unwrap();
    let outcome = crate::loadout::driver::run_to_outcome(&Host::new(daemon), &context)
        .await
        .unwrap();
    assert_eq!(outcome.turns[0].state, TurnState::Completed, "{outcome:?}");
    for volume in ["axo-egr-", "axo-egi-"] {
        assert!(volume_exists(&format!("{volume}{}", accepted.session_id)).await);
    }
    accepted.session_id
}

/// The runtime volumes a run's Session has on Podman.
async fn runtime_volumes_present(session: &str) -> Vec<String> {
    let mut present = Vec::new();
    for volume in SessionSandbox::runtime_volume_names(session) {
        if volume_exists(&volume).await {
            present.push(volume);
        }
    }
    present
}

/// The reap check the daemon made at start, as `doctor` reads it.
fn reap_report(daemon: &AxocoatlDaemon) -> axocoatl_isolation::runtime_volumes::RuntimeVolumeReap {
    match daemon.runtime_volume_check() {
        RuntimeVolumeCheck::Checked { report } => report,
        other => panic!("the daemon did not check its runtime volumes: {other:?}"),
    }
}

async fn reap_phase(phase: &str) {
    let server = model_server().await;
    let config_dir = tempfile::tempdir().unwrap();
    let config = load_config(config_dir.path(), &server.uri()).await;
    let daemon = Arc::new(AxocoatlDaemon::bootstrap_headless(config).await.unwrap());
    daemon.set_config_path(&config_dir.path().join("axocoatl.yaml"));
    let repo = std::path::PathBuf::from(read_shared("repo"));
    let report = reap_report(&daemon);
    eprintln!("phase {phase}: the daemon's start check: {report:?}");
    match phase {
        // Daemon A's first data root at this path: a run's Session, left
        // with its volumes when the data root is then removed.
        "wiped" => {
            assert!(report.failed.is_empty(), "{report:?}");
            let session = completed_run(&daemon, &repo, "run-wiped").await;
            write_shared("wiped", &session);
        }
        // Daemon A again, on a new data root at the same path: its authority
        // is the same, and the removed root's Session is unknown to it.
        "a1" => {
            let wiped = read_shared("wiped");
            for volume in SessionSandbox::runtime_volume_names(&wiped) {
                if volume.starts_with("axo-egr-") || volume.starts_with("axo-egi-") {
                    assert!(report.removed.contains(&volume), "{volume}: {report:?}");
                }
            }
            assert!(runtime_volumes_present(&wiped).await.is_empty());
            assert!(report.failed.is_empty(), "{report:?}");
            // One Session stays open; the other is closed as a 1.2 daemon
            // closed it, or as a Close cut short by a crash leaves it:
            // closed, its runtime volumes still there.
            let open = completed_run(&daemon, &repo, "run-open").await;
            let closed = completed_run(&daemon, &repo, "run-closed").await;
            daemon.session_store.lock().await.close(&closed).unwrap();
            write_shared("open", &open);
            write_shared("closed", &closed);
        }
        // Daemon B, another data root on the same Podman machine.
        "b1" => {
            let other = completed_run(&daemon, &repo, "run-other").await;
            write_shared("other", &other);
        }
        // Daemon A restarts: it removes the closed Session's volumes, keeps
        // the open one's and leaves daemon B's alone.
        "a2" => {
            let (open, closed, other) = (
                read_shared("open"),
                read_shared("closed"),
                read_shared("other"),
            );
            assert!(report.failed.is_empty(), "{report:?}");
            for volume in ["axo-egr-", "axo-egi-"] {
                let closed = format!("{volume}{closed}");
                assert!(report.removed.contains(&closed), "{closed}: {report:?}");
                assert!(!volume_exists(&closed).await, "{closed}");
                let open = format!("{volume}{open}");
                assert!(!report.removed.contains(&open), "{open}: {report:?}");
                assert!(volume_exists(&open).await, "{open}");
                let other = format!("{volume}{other}");
                assert!(!report.removed.contains(&other), "{other}: {report:?}");
                assert!(volume_exists(&other).await, "{other}");
            }
            assert!(report.kept_open >= 2, "{report:?}");
            assert!(report.other_daemons >= 2, "{report:?}");
            assert!(report
                .removed
                .iter()
                .all(|volume| !volume.ends_with(&open) && !volume.ends_with(&other)));
            // Delete removes the rest of its own.
            daemon.delete_session(&open).await.unwrap();
            daemon.delete_session(&closed).await.unwrap();
            assert!(runtime_volumes_present(&open).await.is_empty());
        }
        // Daemon B restarts and keeps its open Session's volumes; Delete
        // removes them.
        "b2" => {
            let other = read_shared("other");
            assert!(!report.removed.iter().any(|volume| volume.ends_with(&other)));
            assert!(!runtime_volumes_present(&other).await.is_empty());
            daemon.delete_session(&other).await.unwrap();
            assert!(runtime_volumes_present(&other).await.is_empty());
        }
        other => panic!("unknown phase {other}"),
    }
    daemon.shutdown_session_runtimes_checked().await.unwrap();
}

/// Two daemons, each with its own data root, on one Podman machine. Each
/// removes, when it starts, only the runtime volumes that carry its own
/// runtime authority and whose Session is unknown to its data root (the
/// data root was removed, as the browser test harness removes its fixture
/// daemon's) or closed (as a 1.2 daemon closed it, or as a Close cut short
/// by a crash leaves it); it keeps an open Session's and never touches the
/// other daemon's. Its start check reports what it removed and kept.
///
/// ```text
/// CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-daemon --lib \
///   leaked_runtime_volumes_are_reaped_by_their_own_daemon_only -- --ignored
/// ```
#[tokio::test]
#[ignore = "requires rootless Podman, docker.io/library/alpine:3.20 and the egress sidecar image"]
async fn leaked_runtime_volumes_are_reaped_by_their_own_daemon_only() {
    if let Some(phase) = std::env::var_os(PHASE) {
        reap_phase(&phase.to_string_lossy()).await;
        return;
    }
    let root = tempfile::Builder::new()
        .prefix("axocoatl-reap-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let shared = root.path().join("shared");
    let repo = root.path().join("repo");
    let (a, b) = (root.path().join("a"), root.path().join("b"));
    for dir in [&shared, &repo, &a, &b] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(repo.join("README.md"), "# Reap\n").unwrap();
    std::fs::write(shared.join("repo"), repo.display().to_string()).unwrap();
    let (data_a, data_b) = (a.join("data"), b.join("data"));
    // Removes, whatever an assertion did, every volume and container the
    // phases' Sessions have, by exact name.
    let cleanup = Cleanup(std::sync::Mutex::new(Vec::new()));
    let remember = |name: &str| {
        if let Ok(id) = std::fs::read_to_string(shared.join(name)) {
            cleanup.0.lock().unwrap().push(id);
        }
    };

    fresh_data_root(&data_a);
    run_reap_phase(REAP_TEST, "wiped", &data_a, &a, &shared).await;
    remember("wiped");
    std::fs::remove_dir_all(&data_a).unwrap();
    fresh_data_root(&data_a);
    run_reap_phase(REAP_TEST, "a1", &data_a, &a, &shared).await;
    remember("open");
    remember("closed");
    fresh_data_root(&data_b);
    run_reap_phase(REAP_TEST, "b1", &data_b, &b, &shared).await;
    remember("other");
    run_reap_phase(REAP_TEST, "a2", &data_a, &a, &shared).await;
    run_reap_phase(REAP_TEST, "b2", &data_b, &b, &shared).await;
    drop(cleanup);
}

/// The runtime authority label Podman lists for `volume`, if it has one.
async fn volume_authority(volume: &str) -> Option<String> {
    let output = tokio::process::Command::new("podman")
        .args([
            "volume",
            "inspect",
            "--format",
            "{{index .Labels \"io.axocoatl.runtime-authority\"}}",
            volume,
        ])
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "podman volume inspect {volume}");
    let label = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!label.is_empty() && label != "<no value>").then_some(label)
}

/// Make `volume` again without a label, as a daemon before 1.3.0 made a
/// dependency volume, so it stands for one such a daemon's Delete left.
async fn unlabelled_volume(volume: &str) {
    let status = tokio::process::Command::new("podman")
        .args(["volume", "create", volume])
        .stdout(std::process::Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(status.success(), "podman volume create {volume}");
    assert_eq!(volume_authority(volume).await, None);
}

/// A run on the Node project `repo`: its Session has a dependency volume
/// labelled with this daemon's runtime authority.
async fn node_run(daemon: &Arc<AxocoatlDaemon>, repo: &std::path::Path, id: &str) -> String {
    let session = completed_run(daemon, repo, id).await;
    let dependencies = SessionSandbox::dependency_volume_name(&session);
    assert!(volume_exists(&dependencies).await, "{dependencies}");
    assert_eq!(
        volume_authority(&dependencies).await.as_deref(),
        Some(daemon.local_runtime_authority.as_str()),
        "{dependencies}"
    );
    session
}

async fn dependency_reap_phase(phase: &str) {
    let server = model_server().await;
    let config_dir = tempfile::tempdir().unwrap();
    let config = load_config(config_dir.path(), &server.uri()).await;
    let daemon = Arc::new(AxocoatlDaemon::bootstrap_headless(config).await.unwrap());
    daemon.set_config_path(&config_dir.path().join("axocoatl.yaml"));
    let repo = std::path::PathBuf::from(read_shared("repo"));
    let report = reap_report(&daemon);
    eprintln!("phase {phase}: the daemon's start check: {report:?}");
    let dependencies = |name: &str| SessionSandbox::dependency_volume_name(&read_shared(name));
    match phase {
        // Daemon A's first data root at this path: a run's Session, whose
        // dependency volume stays when the data root is then removed.
        "wiped" => {
            assert!(report.failed.is_empty(), "{report:?}");
            let session = node_run(&daemon, &repo, "dep-wiped").await;
            write_shared("wiped", &session);
        }
        // Daemon A again, on a new data root at the same path: the removed
        // root's Session is unknown to it and the volume carries its
        // authority, so it goes.
        "a1" => {
            let wiped = dependencies("wiped");
            assert!(report.removed.contains(&wiped), "{wiped}: {report:?}");
            assert!(!volume_exists(&wiped).await, "{wiped}");
            assert!(report.failed.is_empty(), "{report:?}");
            // One Session stays open, one is closed (Close keeps its
            // dependency volume) and one is deleted, its dependency volume
            // then made again without a label, as one a daemon before 1.3.0
            // left behind.
            let open = node_run(&daemon, &repo, "dep-open").await;
            let closed = node_run(&daemon, &repo, "dep-closed").await;
            daemon.close_session(&closed).await.unwrap();
            assert!(
                volume_exists(&SessionSandbox::dependency_volume_name(&closed)).await,
                "Close removed the dependency volume"
            );
            let deleted = node_run(&daemon, &repo, "dep-deleted").await;
            daemon.delete_session(&deleted).await.unwrap();
            let deleted_volume = SessionSandbox::dependency_volume_name(&deleted);
            assert!(!volume_exists(&deleted_volume).await, "{deleted_volume}");
            unlabelled_volume(&deleted_volume).await;
            write_shared("open", &open);
            write_shared("closed", &closed);
            write_shared("deleted", &deleted);
        }
        // Daemon B, another data root on the same Podman machine: the same
        // three, under its own authority.
        "b1" => {
            let open = node_run(&daemon, &repo, "dep-other-open").await;
            let closed = node_run(&daemon, &repo, "dep-other-closed").await;
            daemon.close_session(&closed).await.unwrap();
            let deleted = node_run(&daemon, &repo, "dep-other-deleted").await;
            daemon.delete_session(&deleted).await.unwrap();
            unlabelled_volume(&SessionSandbox::dependency_volume_name(&deleted)).await;
            write_shared("other-open", &open);
            write_shared("other-closed", &closed);
            write_shared("other-deleted", &deleted);
        }
        // Daemon A restarts: it removes the unlabelled volume of the Session
        // it deleted, keeps its open and closed Sessions', and leaves daemon
        // B's alone: B's labelled ones, the unlabelled one of the Session B
        // deleted, and one no data root shows it made.
        "a2" => {
            assert!(report.failed.is_empty(), "{report:?}");
            let deleted = dependencies("deleted");
            assert!(report.removed.contains(&deleted), "{deleted}: {report:?}");
            assert!(!volume_exists(&deleted).await, "{deleted}");
            for kept in [
                "open",
                "closed",
                "other-open",
                "other-closed",
                "other-deleted",
                "unknown",
            ] {
                let volume = dependencies(kept);
                assert!(!report.removed.contains(&volume), "{volume}: {report:?}");
                assert!(volume_exists(&volume).await, "{volume}");
            }
            assert!(report.kept_dependencies >= 2, "{report:?}");
            assert!(report.other_daemons >= 2, "{report:?}");
            assert!(report.unlabelled_kept >= 2, "{report:?}");
            // Delete removes the rest of its own.
            for session in [read_shared("open"), read_shared("closed")] {
                daemon.delete_session(&session).await.unwrap();
                let volume = SessionSandbox::dependency_volume_name(&session);
                assert!(!volume_exists(&volume).await, "{volume}");
            }
        }
        // Daemon B restarts: it removes the unlabelled volume of the Session
        // it deleted and keeps its open and closed Sessions'.
        "b2" => {
            assert!(report.failed.is_empty(), "{report:?}");
            let deleted = dependencies("other-deleted");
            assert!(report.removed.contains(&deleted), "{deleted}: {report:?}");
            assert!(!volume_exists(&deleted).await, "{deleted}");
            for kept in ["other-open", "other-closed", "unknown"] {
                let volume = dependencies(kept);
                assert!(!report.removed.contains(&volume), "{volume}: {report:?}");
                assert!(volume_exists(&volume).await, "{volume}");
            }
            for session in [read_shared("other-open"), read_shared("other-closed")] {
                daemon.delete_session(&session).await.unwrap();
                let volume = SessionSandbox::dependency_volume_name(&session);
                assert!(!volume_exists(&volume).await, "{volume}");
            }
        }
        other => panic!("unknown phase {other}"),
    }
    daemon.shutdown_session_runtimes_checked().await.unwrap();
}

/// Two daemons, each with its own data root, on one Podman machine, with
/// runs on a Node project, whose Sessions have dependency volumes
/// (`axo-ses-<id>-node-modules`) that Close keeps. When it starts, each
/// removes the dependency volumes that carry its own runtime authority and
/// whose Session is unknown to its data root (the data root was removed)
/// and the unlabelled ones (as a daemon before 1.3.0 made them) of a
/// Session its data root created and deleted; it keeps its open and closed
/// Sessions' and never touches the other daemon's, labelled or not, or one
/// no data root shows it made.
///
/// ```text
/// CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-daemon --lib \
///   leaked_dependency_volumes_are_reaped_by_their_own_daemon_only -- --ignored
/// ```
#[tokio::test]
#[ignore = "requires rootless Podman, docker.io/library/alpine:3.20 and the egress sidecar image"]
async fn leaked_dependency_volumes_are_reaped_by_their_own_daemon_only() {
    if let Some(phase) = std::env::var_os(PHASE) {
        dependency_reap_phase(&phase.to_string_lossy()).await;
        return;
    }
    let root = tempfile::Builder::new()
        .prefix("axocoatl-reap-deps-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let shared = root.path().join("shared");
    let repo = root.path().join("repo");
    let (a, b) = (root.path().join("a"), root.path().join("b"));
    for dir in [&shared, &repo, &a, &b] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(repo.join("README.md"), "# Reap\n").unwrap();
    std::fs::write(repo.join("package.json"), "{}\n").unwrap();
    std::fs::write(shared.join("repo"), repo.display().to_string()).unwrap();
    let (data_a, data_b) = (a.join("data"), b.join("data"));
    // Removes, whatever an assertion did, every volume and container the
    // phases' Sessions have, by exact name.
    let cleanup = Cleanup(std::sync::Mutex::new(Vec::new()));
    let remember = |name: &str| {
        if let Ok(id) = std::fs::read_to_string(shared.join(name)) {
            cleanup.0.lock().unwrap().push(id);
        }
    };
    // A dependency volume without a label that no data root shows it made.
    let unknown = format!("ses-{}", uuid::Uuid::new_v4());
    std::fs::write(shared.join("unknown"), &unknown).unwrap();
    remember("unknown");
    unlabelled_volume(&SessionSandbox::dependency_volume_name(&unknown)).await;

    fresh_data_root(&data_a);
    run_reap_phase(DEPENDENCY_REAP_TEST, "wiped", &data_a, &a, &shared).await;
    remember("wiped");
    std::fs::remove_dir_all(&data_a).unwrap();
    fresh_data_root(&data_a);
    run_reap_phase(DEPENDENCY_REAP_TEST, "a1", &data_a, &a, &shared).await;
    for name in ["open", "closed", "deleted"] {
        remember(name);
    }
    fresh_data_root(&data_b);
    run_reap_phase(DEPENDENCY_REAP_TEST, "b1", &data_b, &b, &shared).await;
    for name in ["other-open", "other-closed", "other-deleted"] {
        remember(name);
    }
    run_reap_phase(DEPENDENCY_REAP_TEST, "a2", &data_a, &a, &shared).await;
    run_reap_phase(DEPENDENCY_REAP_TEST, "b2", &data_b, &b, &shared).await;
    drop(cleanup);
}
