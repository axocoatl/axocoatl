//! Run admission on a live daemon: every problem of a run reported at once,
//! usage problems before infrastructure ones, nothing created for a refused
//! run and Podman never asked while a usage problem stands; the dry run;
//! the `run.models` defaults; the context floor; Ollama's own reasons; and a
//! stopped Podman machine started for a run. Each body runs in a child
//! process with its own data root and a fake Podman that logs every call.
use super::*;
use axocoatl_config::loadout::ModelSpec;
use std::os::unix::fs::PermissionsExt;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHILD: &str = "AXOCOATL_ADMISSION_TEST_CHILD";
/// Where the fake Podman writes each command line it gets.
const PODMAN_LOG: &str = "AXOCOATL_ADMISSION_TEST_PODMAN_LOG";
const WRITER: &str = "writer-model:latest";
const REVIEWER: &str = "reviewer-model:latest";
/// Loaded with Ollama's default context below 24 GB of GPU memory.
const SMALL: &str = "small-model:latest";
/// Not pulled.
const MISSING: &str = "missing-model:latest";
const DIGEST: &str = "a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72";

/// An audited Ollama 0.40.6 with cloud models off: WRITER and REVIEWER
/// load with 32768 tokens of context, SMALL with 4096, and MISSING is not
/// pulled (`/api/show` answers 404 with Ollama's message).
async fn ollama() -> MockServer {
    let server = MockServer::start().await;
    crate::ollama_contract_test_support::mount_contract_probes(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .and(body_partial_json(serde_json::json!({"model": MISSING})))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(
                serde_json::json!({"error": format!("model '{MISSING}' not found")}),
            ),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    let models = [(WRITER, 32768), (REVIEWER, 32768), (SMALL, 4096)];
    let tags: Vec<_> = models
        .iter()
        .map(|(name, _)| serde_json::json!({"name": name, "model": name, "digest": DIGEST}))
        .collect();
    let loaded: Vec<_> = models
        .iter()
        .map(|(name, context)| {
            serde_json::json!({"name": name, "model": name, "digest": DIGEST,
                "details": {"format": "gguf"}, "context_length": context, "runner": "llamacpp"})
        })
        .collect();
    for (verb, route, body) in [
        (
            "GET",
            "/api/version",
            serde_json::json!({"version": axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION}),
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
        ("GET", "/api/tags", serde_json::json!({ "models": tags })),
        ("GET", "/api/ps", serde_json::json!({ "models": loaded })),
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
                "model": body["model"], "created_at": "2026-10-19T00:00:00Z",
                "response": "", "done": true, "done_reason": "load"
            }))
        })
        .mount(&server)
        .await;
    server
}

/// A daemon configured with the Ollama at `base_url`, the `run:` section
/// `run`, and no OpenRouter.
async fn daemon(base_url: &str, run: &str) -> (AxocoatlDaemon, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("axocoatl.yaml");
    std::fs::write(
        &path,
        format!(
            r#"
agents:
  - id: conversation
    name: Conversation
    provider: ollama
    model: {WRITER}
    tools: [read_file]
providers:
  ollama:
    base_url: {base_url}
sandbox:
  backend: podman
  network: bridge
consolidation:
  enabled: false
{run}
"#
        ),
    )
    .unwrap();
    let config = axocoatl_config::load_config(&path).await.unwrap();
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    daemon.set_config_path(&path);
    (daemon, dir)
}

fn request(loadout: &str, repo: &std::path::Path, params: &[(&str, &str)], id: &str) -> RunRequest {
    RunRequest {
        loadout: loadout.into(),
        task: "Fix the failing test.".into(),
        repo: repo.display().to_string(),
        params: params
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        keep: Default::default(),
        check_command: Some("true".into()),
        setup_command: None,
        request_id: id.into(),
    }
}

async fn admit(
    daemon: &AxocoatlDaemon,
    request: RunRequest,
) -> Result<(RunAccepted, RunContext), DaemonError> {
    tokio::time::timeout(Duration::from_secs(120), daemon.admit_loadout_run(request))
        .await
        .expect("admission never waits")
}

fn usage(result: Result<(RunAccepted, RunContext), DaemonError>) -> String {
    match result {
        Err(DaemonError::InvalidRequest(message)) => message,
        Err(other) => panic!("expected a usage refusal, got {other}"),
        Ok((accepted, _)) => panic!("admitted {}", accepted.run_id),
    }
}

fn busy(result: Result<(RunAccepted, RunContext), DaemonError>) -> String {
    match result {
        Err(DaemonError::WorkspaceBusy(message)) => message,
        Err(other) => panic!("expected the held Workspace, got {other}"),
        Ok((accepted, _)) => panic!("admitted {}", accepted.run_id),
    }
}

/// What the fake Podman was asked, one command line each.
fn podman_calls() -> Vec<String> {
    let log = std::env::var(PODMAN_LOG).unwrap();
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn forget_podman_calls() {
    std::fs::write(std::env::var(PODMAN_LOG).unwrap(), "").unwrap();
}

/// A Workspace for `repo` held by another Session, so a run that passes
/// admission stops at the Workspace, before its own Session exists.
async fn hold(daemon: &AxocoatlDaemon, repo: &std::path::Path) -> tokio::sync::OwnedMutexGuard<()> {
    let workspace = daemon
        .workspace_store
        .lock()
        .await
        .register(repo, Some("Held"))
        .unwrap();
    daemon
        .attempt_operation_for_workspace(&workspace.id)
        .await
        .lock_owned()
        .await
}

/// Nothing exists for a refused run: no Workspace, Session or run record.
async fn nothing_created(daemon: &AxocoatlDaemon) {
    assert!(daemon.list_workspaces().await.is_empty());
    assert!(daemon.list_sessions().await.is_empty());
    assert!(daemon.list_loadout_runs().await.unwrap().is_empty());
}

async fn usage_child_body() {
    let server = ollama().await;
    let (daemon, _dir) = daemon(&server.uri(), "").await;
    let repo = tempfile::tempdir().unwrap();
    let writer = format!("ollama:{WRITER}");
    forget_podman_calls();
    let fix = |reviewer: &str, id: &str| {
        request(
            "fix",
            repo.path(),
            &[("writer_model", &writer), ("reviewer_model", reviewer)],
            id,
        )
    };

    // A reviewer on a provider that cannot review: exit code 3, before
    // Podman, with the providers that can.
    let message = usage(admit(&daemon, fix("anthropic:opus", "anthropic")).await);
    assert_eq!(
        message,
        crate::loadout::admission::unsupported_provider(
            "the reviewer",
            Some("reviewer_model"),
            &ModelSpec::parse("anthropic:opus").unwrap()
        )
    );
    for (reviewer, id) in [("openai:gpt-5.5", "openai"), ("claude-code:opus", "claude")] {
        let message = usage(admit(&daemon, fix(reviewer, id)).await);
        assert!(
            message.contains("runs only on Ollama or OpenRouter")
                && message.contains("cannot review"),
            "{message}"
        );
    }
    // OpenRouter that is not configured.
    let message = usage(admit(&daemon, fix("openrouter:qwen/qwen3-coder", "openrouter")).await);
    assert!(
        message.starts_with(
            "the reviewer runs on OpenRouter, and this daemon's configuration has no \
             providers.openrouter"
        ),
        "{message}"
    );
    // An Ollama model that is not pulled: Ollama's reason and the pull.
    let missing = format!("ollama:{MISSING}");
    let message = usage(admit(&daemon, fix(&missing, "missing")).await);
    assert_eq!(
        message,
        format!(
            "the reviewer runs {missing}, which the Ollama at {} cannot serve: Model not found: \
             {MISSING} on ollama. Ollama does not have {MISSING}: run `ollama pull {MISSING}`",
            server.uri()
        )
    );
    // A model Ollama loads with 4096 tokens: refused with both numbers and
    // OLLAMA_CONTEXT_LENGTH, before any call of the run.
    let small = format!("ollama:{SMALL}");
    let message = usage(
        admit(
            &daemon,
            request(
                "fix",
                repo.path(),
                &[("writer_model", &small), ("reviewer_model", &writer)],
                "small",
            ),
        )
        .await,
    );
    assert!(
        message.starts_with(&format!(
            "Agent writer runs {small}, which the Ollama at {} loads with a 4096-token context, \
             and a loadout run needs at least 16384",
            server.uri()
        )),
        "{message}"
    );
    assert!(message.contains("OLLAMA_CONTEXT_LENGTH=32768"), "{message}");
    assert!(
        !server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.url.path() == "/api/chat"
                && !crate::ollama_contract_test_support::is_contract_probe(request)),
        "no model call is made"
    );

    // Every problem of fix-claude-code at once: the Claude Code token,
    // checked first, and the reviewer's provider (usage), then the image
    // (infrastructure).
    let message = usage(
        admit(
            &daemon,
            request(
                "fix-claude-code",
                repo.path(),
                &[("reviewer_model", "anthropic:opus")],
                "claude-everything",
            ),
        )
        .await,
    );
    let lines: Vec<&str> = message.lines().collect();
    assert_eq!(lines.len(), 4, "{message}");
    assert_eq!(
        lines[0],
        "3 problems stop this run, and nothing was started:"
    );
    assert!(
        lines[1].starts_with(
            "  1. Agent writer (claude-code) sends the stored credential \"claude-code-oauth\""
        ) && lines[1].contains("axocoatl connect claude-code"),
        "{message}"
    );
    assert!(
        lines[2].starts_with("  2. the reviewer runs anthropic:opus"),
        "{message}"
    );
    assert!(
        lines[3].starts_with("  3. recipe images: no image is built from the recipes claude-code"),
        "{message}"
    );

    // Both missing parameters at once, with a missing check command, and
    // the configuration default that would set them.
    let mut bare = request("fix", repo.path(), &[], "bare");
    bare.check_command = None;
    let message = usage(admit(&daemon, bare).await);
    assert!(
        message.starts_with("2 problems stop this run, and nothing was started:\n  1. loadout parameters writer_model and reviewer_model: required: pass --model writer=provider:model --model reviewer=provider:model"),
        "{message}"
    );
    assert!(
        message.contains("run.models.writer and run.models.reviewer"),
        "{message}"
    );
    assert!(
        message.contains("\n  2. the check tests runs the repository's own check command"),
        "{message}"
    );

    // None of these asked Podman, and none created anything.
    assert_eq!(podman_calls(), Vec::<String>::new());
    nothing_created(&daemon).await;

    // The dry run lists the same problems and asks Podman, read-only.
    let preflight = daemon
        .preflight_loadout_run(request(
            "fix-claude-code",
            repo.path(),
            &[("reviewer_model", "anthropic:opus")],
            "dry",
        ))
        .await;
    assert!(!preflight.ready);
    assert_eq!(preflight.exit_code, exit_code::USAGE);
    let codes: Vec<&str> = preflight
        .problems
        .iter()
        .map(|problem| problem.code.as_str())
        .collect();
    assert_eq!(codes[..3], ["credential", "model_provider", "recipe_image"]);
    assert_eq!(preflight.problems[2].exit_code, exit_code::INFRASTRUCTURE);
    if cfg!(target_os = "macos") {
        // A stopped machine is not a problem: a run starts it.
        assert_eq!(codes.len(), 3, "{:?}", preflight.problems);
        assert_eq!(
            preflight.sandbox.as_deref(),
            Some("the Podman machine is stopped; a run starts it first")
        );
    }
    assert_eq!(preflight.params["writer_model"], "anthropic:sonnet");
    assert!(!podman_calls().iter().any(|call| call == "machine start"));
    nothing_created(&daemon).await;
    daemon.shutdown().await.unwrap();
}

async fn defaults_child_body() {
    let server = ollama().await;
    let (daemon, _dir) = daemon(
        &server.uri(),
        &format!("run:\n  models:\n    writer: \"ollama:{WRITER}\"\n    reviewer: \"ollama:{REVIEWER}\"\n"),
    )
    .await;
    let repo = tempfile::tempdir().unwrap();
    let writer = format!("ollama:{WRITER}");
    let reviewer = format!("ollama:{REVIEWER}");

    // The dry run of `fix` with no --model: ready, with the onboarded
    // models and where they came from.
    let preflight = daemon
        .preflight_loadout_run(request("fix", repo.path(), &[], "dry-fix"))
        .await;
    assert!(preflight.ready, "{:?}", preflight.problems);
    assert_eq!(preflight.exit_code, 0);
    assert_eq!(preflight.params["writer_model"], writer);
    assert_eq!(preflight.params["reviewer_model"], reviewer);
    assert_eq!(
        preflight.notes,
        [
            format!(
                "writer_model = {writer} (from your setup: run.models.writer in the \
                 configuration)"
            ),
            format!(
                "reviewer_model = {reviewer} (from your setup: run.models.reviewer in the \
                 configuration)"
            )
        ]
    );
    assert_eq!(
        preflight.sandbox.as_deref(),
        Some("Podman answers and runs rootless")
    );
    nothing_created(&daemon).await;

    // fix-claude-code keeps Claude Code's model, and says why the local
    // writer default does not apply.
    let preflight = daemon
        .preflight_loadout_run(request("fix-claude-code", repo.path(), &[], "dry-claude"))
        .await;
    assert_eq!(preflight.params["writer_model"], "anthropic:sonnet");
    assert_eq!(preflight.params["reviewer_model"], reviewer);
    assert!(
        preflight.notes.iter().any(|note| note
            == &format!(
                "run.models.writer {writer} was not used for writer_model: loadout \
                 fix-claude-code's writer runs claude-code, which calls anthropic's model API"
            )),
        "{:?}",
        preflight.notes
    );

    // A run with no --model gets past admission to the Workspace.
    let held = hold(&daemon, repo.path()).await;
    busy(admit(&daemon, request("fix", repo.path(), &[], "defaults")).await);
    // A default never reviews its own writer's change: --model writer= on
    // the reviewer's model is refused, naming the default.
    let message = usage(
        admit(
            &daemon,
            request("fix", repo.path(), &[("writer_model", &reviewer)], "same"),
        )
        .await,
    );
    assert!(
        message.starts_with(&format!(
            "the reviewer {reviewer} is the writer's model ({reviewer}), with run.models.reviewer \
             from your setup"
        )),
        "{message}"
    );
    // Named on the command line, both the same is the person's choice (a
    // warning), and --model wins over the configuration.
    busy(
        admit(
            &daemon,
            request(
                "fix",
                repo.path(),
                &[("writer_model", &reviewer), ("reviewer_model", &reviewer)],
                "explicit",
            ),
        )
        .await,
    );
    drop(held);

    // Admitted: the run's manifest has the defaults, and its notes (the
    // `note` events `axocoatl run` prints, and the Outcome's notes) say
    // where they came from. Its environment cannot start under the fake
    // Podman, so it ends at once.
    let (accepted, _) = admit(&daemon, request("fix", repo.path(), &[], "admitted"))
        .await
        .unwrap();
    let manifest = daemon
        .loadout_runs
        .store()
        .unwrap()
        .manifest(&accepted.run_id)
        .unwrap();
    assert_eq!(manifest.params["writer_model"], writer);
    assert_eq!(manifest.params["reviewer_model"], reviewer);
    let events = daemon
        .loadout_run_events(&accepted.run_id, None, 100)
        .await
        .unwrap();
    let notes: Vec<&str> = events
        .events
        .iter()
        .filter_map(|(_, event)| match event {
            RunEvent::Phase { phase, detail, .. } if phase == "note" => Some(detail.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        notes,
        [
            format!(
                "writer_model = {writer} (from your setup: run.models.writer in the \
                 configuration)"
            ),
            format!(
                "reviewer_model = {reviewer} (from your setup: run.models.reviewer in the \
                 configuration)"
            )
        ]
    );
    daemon.shutdown().await.unwrap();
}

async fn stopped_machine_child_body() {
    let server = ollama().await;
    let (daemon, _dir) = daemon(&server.uri(), "").await;
    let repo = tempfile::tempdir().unwrap();
    let models = [
        ("writer_model", format!("ollama:{WRITER}")),
        ("reviewer_model", format!("ollama:{REVIEWER}")),
    ];
    let params: Vec<(&str, &str)> = models
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    forget_podman_calls();
    // The dry run only says what it found: the machine stays stopped.
    let preflight = daemon
        .preflight_loadout_run(request("fix", repo.path(), &params, "dry"))
        .await;
    assert!(preflight.ready, "{:?}", preflight.problems);
    assert_eq!(
        preflight.sandbox.as_deref(),
        Some("the Podman machine is stopped; a run starts it first")
    );
    assert!(!podman_calls().iter().any(|call| call == "machine start"));
    // A run starts it, as a Session does, and gets past admission.
    let held = hold(&daemon, repo.path()).await;
    busy(admit(&daemon, request("fix", repo.path(), &params, "run")).await);
    let calls = podman_calls();
    assert_eq!(
        calls.iter().filter(|call| *call == "machine start").count(),
        1,
        "{calls:?}"
    );
    let start = calls
        .iter()
        .position(|call| call == "machine start")
        .unwrap();
    assert!(
        calls[start..]
            .iter()
            .any(|call| call == "info --format {{.Host.Security.Rootless}}"),
        "{calls:?}"
    );
    drop(held);
    daemon.shutdown().await.unwrap();
}

async fn no_podman_child_body() {
    let server = ollama().await;
    let (daemon, _dir) = daemon(&server.uri(), "").await;
    let repo = tempfile::tempdir().unwrap();
    let writer = format!("ollama:{WRITER}");
    let reviewer = format!("ollama:{REVIEWER}");
    let params = [
        ("writer_model", writer.as_str()),
        ("reviewer_model", reviewer.as_str()),
    ];
    let preflight = daemon
        .preflight_loadout_run(request("fix", repo.path(), &params, "dry"))
        .await;
    assert!(!preflight.ready);
    assert_eq!(preflight.exit_code, exit_code::INFRASTRUCTURE);
    assert_eq!(preflight.problems.len(), 1, "{:?}", preflight.problems);
    assert_eq!(preflight.problems[0].code, "podman");
    assert!(
        preflight.problems[0]
            .message
            .contains("podman not installed"),
        "{:?}",
        preflight.problems
    );
    // A run is refused as infrastructure (exit code 5), naming the
    // install, and creates nothing.
    match admit(&daemon, request("fix", repo.path(), &params, "run")).await {
        Err(DaemonError::Session(message)) => assert!(
            message.starts_with("a loadout Session needs rootless Podman, and Podman is not ready")
                && message.contains("not installed"),
            "{message}"
        ),
        Err(other) => panic!("expected the missing Podman, got {other}"),
        Ok((accepted, _)) => panic!("admitted {}", accepted.run_id),
    }
    nothing_created(&daemon).await;
    daemon.shutdown().await.unwrap();
}

/// How the fake Podman starts.
#[derive(Clone, Copy)]
enum Podman {
    /// Not on PATH.
    Absent,
    /// A rootless Podman whose machine is stopped until `machine start`.
    Stopped,
    /// A rootless Podman that answers.
    Running,
}

/// Run `name` in a child process with its own data root and `podman`.
async fn run_child(name: &str, podman: Podman) {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let log = root.path().join("podman.log");
    if !matches!(podman, Podman::Absent) {
        let fake = bin.join("podman");
        std::fs::write(
            &fake,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
state="{state}"
case "$*" in
  --version) printf 'podman version 5.0.0\n' ;;
  'machine list --format json')
    if [ -e "$state" ]; then printf '[{{"Running":true}}]\n'; else printf '[{{"Running":false}}]\n'; fi ;;
  'machine start') : > "$state"; printf 'Machine "podman-machine-default" started successfully\n' ;;
  'info --format json')
    [ -e "$state" ] || {{ printf 'Cannot connect to Podman\n' >&2; exit 125; }}
    printf '{{}}\n' ;;
  'info --format {{{{.Host.Security.Rootless}}}}')
    [ -e "$state" ] || {{ printf 'Cannot connect to Podman\n' >&2; exit 125; }}
    printf 'true\n' ;;
  'ps '*) ;;
  'rm '*|'volume rm '*|'network rm '*) ;;
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#,
                log = log.display(),
                state = root.path().join("machine-running").display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        if matches!(podman, Podman::Running) {
            std::fs::write(root.path().join("machine-running"), "").unwrap();
        }
    }
    let result = tokio::time::timeout(
        Duration::from_secs(300),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, "1")
            .env(PODMAN_LOG, &log)
            .env("AXOCOATL_DATA_DIR", root.path().join("data"))
            .env("AXOCOATL_SOCKET_PATH", root.path().join("ipc/daemon.sock"))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env_remove("CONTAINER_HOST")
            .env_remove("CONTAINER_CONNECTION")
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
}

/// Reviewers on providers that cannot review, an unconfigured OpenRouter,
/// a model Ollama does not have, a context too small for a run and a run
/// missing everything are each refused with exit code 3 and one message
/// listing every problem; Podman is never asked and nothing is created.
/// The dry run lists the same problems.
#[tokio::test]
async fn usage_problems_are_reported_together_before_podman_and_create_nothing() {
    if std::env::var_os(CHILD).is_some() {
        usage_child_body().await;
        return;
    }
    run_child(
        "bootstrap::loadout_runs::admission_tests::usage_problems_are_reported_together_before_podman_and_create_nothing",
        Podman::Stopped,
    )
    .await;
}

/// `run.models` gives `fix` its writer and reviewer when the run names
/// none; the dry run, the manifest and the run's notes say where they came
/// from; --model wins; and a default never reviews its own writer's model.
#[tokio::test]
async fn run_models_defaults_apply_where_the_run_names_no_model() {
    if std::env::var_os(CHILD).is_some() {
        defaults_child_body().await;
        return;
    }
    run_child(
        "bootstrap::loadout_runs::admission_tests::run_models_defaults_apply_where_the_run_names_no_model",
        Podman::Running,
    )
    .await;
}

/// A stopped Podman machine (after a reboot) is started for a run, as it
/// is for a Session; a dry run leaves it stopped.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_run_starts_a_stopped_podman_machine() {
    if std::env::var_os(CHILD).is_some() {
        stopped_machine_child_body().await;
        return;
    }
    run_child(
        "bootstrap::loadout_runs::admission_tests::a_run_starts_a_stopped_podman_machine",
        Podman::Stopped,
    )
    .await;
}

/// Without Podman a run is refused as infrastructure, naming the install.
#[tokio::test]
async fn without_podman_a_run_is_refused_as_infrastructure() {
    if std::env::var_os(CHILD).is_some() {
        no_podman_child_body().await;
        return;
    }
    run_child(
        "bootstrap::loadout_runs::admission_tests::without_podman_a_run_is_refused_as_infrastructure",
        Podman::Absent,
    )
    .await;
}

/// A busy Ollama (503) is asked once more, as a Session's provider is: the
/// observation admission makes succeeds on the second answer.
#[tokio::test]
async fn a_busy_ollama_is_asked_once_more() {
    let server = ollama().await;
    Mock::given(method("GET"))
        .and(path("/api/version"))
        .respond_with(
            ResponseTemplate::new(503)
                .insert_header("Retry-After", "0")
                .set_body_json(serde_json::json!({"error": "server busy, please try again"})),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    let uri = server.uri();
    let observed = crate::session_dispatch::with_provider_retry(|| {
        axocoatl_llm_ollama::observe_native_ollama_context(&uri, WRITER)
    })
    .await
    .unwrap();
    assert_eq!(observed.context_tokens, 32768);
    let versions = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/api/version")
        .count();
    assert!(versions >= 2, "{versions}");
}
