//! Session lifecycle on a live daemon, as the 1.3 smoke runs found it:
//! closing an idle Session while another Session's turn holds their
//! Workspace, what Close leaves on Podman, and a data directory made before
//! the daemon first started. Each body runs in a child process with its own
//! data root, because bootstrap reads the process environment.
use super::*;
use crate::loadout::api::{RunAccepted, RunRequest};
use crate::loadout::host::{CheckLabel, ReproRequest};
use crate::loadout::{RunContext, RunError, RunHost};
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

/// The daemon's configuration, with the lifecycle loadout beside it.
async fn load_config(dir: &std::path::Path, model_url: &str) -> AxocoatlConfig {
    let config_path = dir.join("axocoatl.yaml");
    std::fs::write(
        &config_path,
        format!(
            "agents: []\nproviders:\n  ollama:\n    base_url: {model_url}\nsandbox:\n  backend: podman\n  network: bridge\nconsolidation:\n  enabled: false\n"
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
///   turn of A and a third run on the repository are refused the same way.
/// - Once B is stopped, A closes. Close removes A's runtime volumes (egress
///   socket, identity socket, service socket, trust) and keeps its Node
///   dependency volume; Reopen starts A again with new runtime volumes, and
///   Delete removes the dependency volume too.
///
/// ```text
/// CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-daemon --lib \
///   an_idle_session_closes_once_another_sessions_turn_lets_go -- --ignored
/// ```
#[tokio::test]
#[ignore = "requires rootless Podman, docker.io/library/alpine:3.20 and the egress sidecar image"]
async fn an_idle_session_closes_once_another_sessions_turn_lets_go() {
    if std::env::var_os(CHILD).is_none() {
        run_child(
            "bootstrap::session_native_lifecycle::lifecycle_tests::an_idle_session_closes_once_another_sessions_turn_lets_go",
            |_| None,
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
