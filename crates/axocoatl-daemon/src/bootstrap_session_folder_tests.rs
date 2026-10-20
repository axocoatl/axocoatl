//! A Session whose Workspace folder is gone, moved, or replaced, on a live
//! daemon, as a 1.3.2 smoke run found it: a run whose environment failed
//! left a Session on repository A; A was moved to B; every later run was
//! refused because the old Session's attempt metadata could not be opened,
//! and deleting that Session failed the same way. Each body runs in a child
//! process with its own data root and a fake Podman (bootstrap reads the
//! process environment), so no container ever starts.
use super::*;
use crate::loadout::api::RunRequest;
use std::os::unix::fs::PermissionsExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHILD: &str = "AXOCOATL_SESSION_FOLDER_TEST_CHILD";
const MODEL: &str = "folder-model:latest";
const USER_LOADOUT_DIR: &str = "loadouts";

/// One writer that may only read files, no checks and no review.
const LOADOUT: &str = r#"
schema: axocoatl.loadout/1
id: folder
version: 1
name: Folder
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

/// The audited local Ollama the native provider admits.
async fn model_server() -> MockServer {
    let server = MockServer::start().await;
    crate::ollama_contract_test_support::mount_contract_probes(&server).await;
    let digest = "a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72";
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
        (
            "GET",
            "/api/tags",
            serde_json::json!({"models": [{"name": MODEL, "model": MODEL, "digest": digest}]}),
        ),
        (
            "GET",
            "/api/ps",
            serde_json::json!({"models": [{"name": MODEL, "model": MODEL, "digest": digest,
                "details": {"format": "gguf"}, "context_length": 32768, "runner": "llamacpp"}]}),
        ),
    ] {
        Mock::given(method(verb))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
    }
    // A load acknowledges the model it was asked for.
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

/// The daemon's configuration, with the folder loadout beside it.
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
    std::fs::write(dir.join(USER_LOADOUT_DIR).join("folder.yaml"), LOADOUT).unwrap();
    axocoatl_config::load_config(&config_path).await.unwrap()
}

fn run_request(repo: &std::path::Path, id: &str) -> RunRequest {
    RunRequest {
        loadout: "folder".into(),
        task: "Say done.".into(),
        repo: repo.display().to_string(),
        params: [("writer_model".to_string(), format!("ollama:{MODEL}"))]
            .into_iter()
            .collect(),
        keep: Default::default(),
        check_command: None,
        setup_command: None,
        environment: Default::default(),
        request_id: id.into(),
    }
}

/// A fake rootless Podman that refuses to start any container, so every
/// run's environment fails at setup, as the smoke run's did. Each command
/// is appended to `podman.log` beside it.
fn fake_podman(root: &std::path::Path) -> String {
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let podman = bin.join("podman");
    std::fs::write(
        &podman,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
case "$*" in
  --version) printf 'podman version 5.0.0\n' ;;
  'machine list --format json') printf '[{{"Running":true}}]\n' ;;
  'info --format json') printf '{{}}\n' ;;
  'info --format {{{{.Host.Security.Rootless}}}}') printf 'true\n' ;;
  'ps '*) ;;
  'rm '*|'volume rm '*|'network rm '*) ;;
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#,
            log = root.join("podman.log").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
    format!("{}:/usr/bin:/bin", bin.display())
}

/// Run `name` in a child process with its own data root and the fake
/// Podman.
async fn run_child(name: &str) {
    let root = tempfile::tempdir().unwrap();
    let path = fake_podman(root.path());
    let data = root.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(300),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, "1")
            .env("AXOCOATL_DATA_DIR", &data)
            .env("AXOCOATL_SOCKET_PATH", root.path().join("ipc/daemon.sock"))
            .env("PATH", path)
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

async fn daemon(server: &MockServer, config_dir: &std::path::Path) -> Arc<AxocoatlDaemon> {
    let config = load_config(config_dir, &server.uri()).await;
    let daemon = Arc::new(AxocoatlDaemon::bootstrap_headless(config).await.unwrap());
    daemon.set_config_path(&config_dir.join("axocoatl.yaml"));
    assert!(daemon.uses_native_session_history());
    daemon
}

/// A repository directory in a new temporary directory, outside the data
/// root.
fn repository(outside: &std::path::Path, name: &str) -> std::path::PathBuf {
    let repo = outside.join(name);
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), format!("# {name}\n")).unwrap();
    repo.canonicalize().unwrap()
}

/// Every way a person acts on a Session, each refused while its folder is
/// gone or moved: the message names the folder (and where the Workspace now
/// is) and says what to do, and nothing about the Session changes.
async fn assert_refused(daemon: &Arc<AxocoatlDaemon>, session_id: &str, expected: &[&str]) {
    let before = daemon.get_session(session_id).await.unwrap();
    let check = |what: &str, message: String| {
        for part in expected {
            assert!(message.contains(part), "{what}: {message:?} lacks {part:?}");
        }
    };
    let terminal = daemon
        .spawn_session_terminal(session_id, "sh", 24, 80)
        .await
        .unwrap_err();
    check("terminal", terminal.to_string());
    let environment = daemon
        .configure_session_environment(session_id, None, None, false, true)
        .await
        .unwrap_err();
    check("environment", environment.to_string());
    let (sink, _receiver) =
        tokio::sync::mpsc::unbounded_channel::<axocoatl_actor::AgentStreamChunk>();
    let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
    let turn = daemon
        .execute_session_turn_streaming(
            session_id,
            &turn_id,
            Some(turn_id.clone()),
            None,
            "Say done.",
            Vec::new(),
            Vec::new(),
            None,
            None,
            sink,
        )
        .await
        .map(|_| ())
        .unwrap_err();
    check("turn", turn.to_string());
    let after = daemon.get_session(session_id).await.unwrap();
    assert_eq!(after.environment, before.environment);
    assert_eq!(after.status, before.status);
}

/// What a person reads for Session `id` whose folder `path` is gone.
fn gone(id: &str, path: &std::path::Path, why: &str) -> Vec<String> {
    vec![
        id.to_string(),
        format!("{} {why}", path.display()),
        format!(
            "Restore the folder at {}, or delete the Session",
            path.display()
        ),
    ]
}

/// What the daemon noted (and logged once) about Session `id`'s folder.
fn noted(daemon: &AxocoatlDaemon, id: &str) -> Option<(std::path::PathBuf, FolderGone)> {
    daemon
        .unavailable_session_folders
        .lock()
        .unwrap()
        .get(id)
        .cloned()
}

fn entries(path: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The smoke run: a run on A whose environment failed, then `mv A B`.
/// - A run on B is admitted, in B, in A's Workspace, which moved to B.
/// - The old Session is refused with A, B and what to do, before and after
///   a new directory is made at A, which nothing ever touches.
/// - Close keeps the Session (closed); Delete removes it; runs on B are
///   admitted throughout, and shutdown stops cleanly.
async fn moved_repository_body() {
    let server = model_server().await;
    let config_dir = tempfile::tempdir().unwrap();
    let daemon = daemon(&server, config_dir.path()).await;
    let outside = tempfile::tempdir().unwrap();
    let a = repository(outside.path(), "a");
    let (run_a, _) = daemon
        .admit_loadout_run(run_request(&a, "run-a"))
        .await
        .unwrap();
    let stale = daemon.get_session(&run_a.session_id).await.unwrap();
    assert_eq!(stale.environment.state, SessionEnvironmentState::Failed);
    assert_eq!(stale.working_dir, a);

    let b = outside.path().canonicalize().unwrap().join("b");
    std::fs::rename(&a, &b).unwrap();
    let (run_b, _) = daemon
        .admit_loadout_run(run_request(&b, "run-b"))
        .await
        .unwrap();
    let session_b = daemon.get_session(&run_b.session_id).await.unwrap();
    assert_eq!(session_b.working_dir, b);
    assert_eq!(run_b.workspace_id, run_a.workspace_id);
    let workspace = daemon.get_workspace(&run_b.workspace_id).await.unwrap();
    assert_eq!(workspace.canonical_path, b);
    // Admission skipped the old Session, and the log says so once for it
    // and its folder.
    assert_eq!(
        noted(&daemon, &stale.id),
        Some((a.clone(), FolderGone::Missing))
    );

    let moved = [
        stale.id.clone(),
        format!("it works in {}", a.display()),
        format!("its Workspace's folder is now {}", b.display()),
        format!("move the folder back to {}", a.display()),
        "or delete the Session".to_string(),
    ];
    let moved: Vec<&str> = moved.iter().map(String::as_str).collect();
    assert_refused(&daemon, &stale.id, &moved).await;
    // A directory made at A again is not the Session's folder: it is still
    // refused, and A is never used.
    std::fs::create_dir(&a).unwrap();
    assert_refused(&daemon, &stale.id, &moved).await;
    daemon
        .admit_loadout_run(run_request(&b, "run-b2"))
        .await
        .unwrap();
    assert!(entries(&a).is_empty(), "{:?}", entries(&a));
    std::fs::remove_dir(&a).unwrap();

    // Close (DELETE without force) keeps the Session, closed; it still
    // blocks nothing.
    daemon.close_session(&stale.id).await.unwrap();
    let closed = daemon.get_session(&stale.id).await.unwrap();
    assert_eq!(closed.status, axocoatl_session::SessionStatus::Closed);
    daemon
        .admit_loadout_run(run_request(&b, "run-b3"))
        .await
        .unwrap();
    let reopen = daemon.reopen_session(&stale.id).await.unwrap_err();
    for part in &moved {
        assert!(reopen.to_string().contains(part), "{reopen}");
    }
    // Delete removes it.
    daemon.delete_session(&stale.id).await.unwrap();
    assert!(daemon.get_session(&stale.id).await.is_none());
    assert_eq!(noted(&daemon, &stale.id), None);
    assert!(daemon
        .list_sessions()
        .await
        .iter()
        .all(|session| session.id != stale.id));
    daemon
        .admit_loadout_run(run_request(&b, "run-b4"))
        .await
        .unwrap();
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_run_on_a_moved_repository_works_in_its_new_folder() {
    if std::env::var_os(CHILD).is_some() {
        moved_repository_body().await;
        return;
    }
    run_child(
        "bootstrap::session_folder::folder_tests::a_run_on_a_moved_repository_works_in_its_new_folder",
    )
    .await;
}

/// `mv A B; mkdir A`, then a run on B: B is a new Workspace, and the run
/// never touches A, whose old Session still works in A.
async fn moved_with_a_new_directory_body() {
    let server = model_server().await;
    let config_dir = tempfile::tempdir().unwrap();
    let daemon = daemon(&server, config_dir.path()).await;
    let outside = tempfile::tempdir().unwrap();
    let a = repository(outside.path(), "a");
    let (run_a, _) = daemon
        .admit_loadout_run(run_request(&a, "run-a"))
        .await
        .unwrap();
    let b = outside.path().canonicalize().unwrap().join("b");
    std::fs::rename(&a, &b).unwrap();
    std::fs::create_dir(&a).unwrap();

    let (run_b, _) = daemon
        .admit_loadout_run(run_request(&b, "run-b"))
        .await
        .unwrap();
    assert_ne!(run_b.workspace_id, run_a.workspace_id);
    let session_b = daemon.get_session(&run_b.session_id).await.unwrap();
    assert_eq!(session_b.working_dir, b);
    assert_eq!(
        daemon
            .get_workspace(&run_a.workspace_id)
            .await
            .unwrap()
            .canonical_path,
        a
    );
    assert!(entries(&a).is_empty(), "{:?}", entries(&a));
    // B's own files are where the person left them.
    assert_eq!(entries(&b), ["README.md"]);
    daemon.delete_session(&run_a.session_id).await.unwrap();
    daemon.delete_session(&run_b.session_id).await.unwrap();
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_run_on_a_moved_repository_never_uses_a_new_directory_at_the_old_path() {
    if std::env::var_os(CHILD).is_some() {
        moved_with_a_new_directory_body().await;
        return;
    }
    run_child(
        "bootstrap::session_folder::folder_tests::a_run_on_a_moved_repository_never_uses_a_new_directory_at_the_old_path",
    )
    .await;
}

/// A Session whose folder was deleted, replaced by a file, or replaced by a
/// link to another directory: runs on another repository are admitted, the
/// Session is refused with its folder named and what to do, the link is
/// never followed, Close and Delete succeed, and the daemon shuts down and
/// starts again cleanly.
async fn gone_folder_body(case: &str) {
    let server = model_server().await;
    let config_dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let a = repository(outside.path(), "a");
    let elsewhere = repository(outside.path(), "elsewhere");
    let other = repository(outside.path(), "other");
    let (stale, closing) = {
        let daemon = daemon(&server, config_dir.path()).await;
        let (stale, _) = daemon
            .admit_loadout_run(run_request(&a, "run-a"))
            .await
            .unwrap();
        let (closing, _) = daemon
            .admit_loadout_run(run_request(&a, "run-a2"))
            .await
            .unwrap();
        daemon.shutdown().await.unwrap();
        (stale.session_id, closing.session_id)
    };
    std::fs::remove_dir_all(&a).unwrap();
    let why = match case {
        "missing" => "no longer exists",
        "file" => {
            std::fs::write(&a, b"not a folder").unwrap();
            "is no longer a directory"
        }
        "link" => {
            std::os::unix::fs::symlink(&elsewhere, &a).unwrap();
            "is now a symbolic link, which Axocoatl does not follow"
        }
        other => panic!("unknown case {other}"),
    };
    // The daemon starts with the folder gone.
    let daemon = daemon(&server, config_dir.path()).await;
    daemon
        .admit_loadout_run(run_request(&other, "run-other"))
        .await
        .unwrap();
    let expected = gone(&stale, &a, why);
    let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
    assert_refused(&daemon, &stale, &expected).await;
    // Close keeps the Session, closed; Delete removes it.
    daemon.close_session(&closing).await.unwrap();
    assert_eq!(
        daemon.get_session(&closing).await.unwrap().status,
        axocoatl_session::SessionStatus::Closed
    );
    daemon.delete_session(&stale).await.unwrap();
    assert!(daemon.get_session(&stale).await.is_none());
    daemon
        .admit_loadout_run(run_request(&other, "run-other2"))
        .await
        .unwrap();
    daemon.delete_session(&closing).await.unwrap();
    assert!(daemon
        .list_sessions()
        .await
        .iter()
        .all(|session| session.id != stale && session.id != closing));
    // Nothing followed the link into the other directory.
    assert_eq!(entries(&elsewhere), ["README.md"]);
    match case {
        "file" => assert_eq!(std::fs::read(&a).unwrap(), b"not a folder"),
        "link" => assert_eq!(std::fs::read_link(&a).unwrap(), elsewhere),
        _ => assert!(std::fs::symlink_metadata(&a).is_err()),
    }
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_session_whose_folder_is_missing_blocks_nothing() {
    if std::env::var_os(CHILD).is_some() {
        gone_folder_body("missing").await;
        return;
    }
    run_child(
        "bootstrap::session_folder::folder_tests::a_session_whose_folder_is_missing_blocks_nothing",
    )
    .await;
}

#[tokio::test]
async fn a_session_whose_folder_is_now_a_file_blocks_nothing() {
    if std::env::var_os(CHILD).is_some() {
        gone_folder_body("file").await;
        return;
    }
    run_child(
        "bootstrap::session_folder::folder_tests::a_session_whose_folder_is_now_a_file_blocks_nothing",
    )
    .await;
}

#[tokio::test]
async fn a_session_whose_folder_is_now_a_link_blocks_nothing_and_is_not_followed() {
    if std::env::var_os(CHILD).is_some() {
        gone_folder_body("link").await;
        return;
    }
    run_child(
        "bootstrap::session_folder::folder_tests::a_session_whose_folder_is_now_a_link_blocks_nothing_and_is_not_followed",
    )
    .await;
}
