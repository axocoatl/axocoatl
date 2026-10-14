//! The HTTP API of a Session whose Workspace folder moved away, as a 1.3.2
//! smoke run found it: `POST /api/runs` on repository A, whose environment
//! failed; `mv A B`; then every `POST /api/runs` was a `400` and `DELETE
//! /api/sessions/<A's Session>` a `500`, both "opening Workspace attempt
//! metadata: No such file or directory". The daemon runs in a child process
//! with its own data root and a fake Podman that starts no container, so
//! every run's environment fails at setup as the smoke run's did.
use crate::auth::AuthConfig;
use crate::middleware::{RateLimitConfig, RateLimiter};
use crate::{build_router, AppState};
use axocoatl_daemon::AxocoatlDaemon;
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const CHILD: &str = "AXOCOATL_MISSING_WORKSPACE_ROUTE_CHILD";

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

/// A fake rootless Podman that refuses to start any container.
fn fake_podman(root: &Path) -> String {
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

async fn call(
    router: &Router,
    method: Method,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "localhost");
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, value)
}

fn run(repo: &Path, id: &str) -> serde_json::Value {
    serde_json::json!({
        "loadout": "folder",
        "task": "Say done.",
        "repo": repo.display().to_string(),
        // Nothing listens there: admission observes no model context, and
        // no turn ever runs.
        "params": {"writer_model": "ollama:folder-model:latest"},
        "request_id": id,
    })
}

async fn session_ids(router: &Router) -> Vec<String> {
    let (status, sessions) = call(router, Method::GET, "/api/sessions", None).await;
    assert_eq!(status, StatusCode::OK, "{sessions}");
    sessions
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap().to_string())
        .collect()
}

async fn moved_workspace_routes_body() {
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("axocoatl.yaml");
    std::fs::write(
        &config_path,
        "agents: []\nproviders:\n  ollama:\n    base_url: http://127.0.0.1:9\nsandbox:\n  backend: podman\n  network: bridge\nconsolidation:\n  enabled: false\n",
    )
    .unwrap();
    std::fs::create_dir(config_dir.path().join("loadouts")).unwrap();
    std::fs::write(config_dir.path().join("loadouts/folder.yaml"), LOADOUT).unwrap();
    let config = axocoatl_config::load_config(&config_path).await.unwrap();
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    daemon.set_config_path(&config_path);
    let state: AppState = Arc::new(tokio::sync::RwLock::new(daemon));
    let router = build_router(
        state.clone(),
        AuthConfig::new(Vec::new(), Vec::new()),
        Vec::new(),
        Arc::new(RateLimiter::new(RateLimitConfig {
            enabled: false,
            ..RateLimitConfig::default()
        })),
    );

    let outside = tempfile::tempdir().unwrap();
    let outside = outside.path().canonicalize().unwrap();
    let (a, b) = (outside.join("a"), outside.join("b"));
    std::fs::create_dir(&a).unwrap();
    std::fs::write(a.join("README.md"), "# a\n").unwrap();
    let (status, run_a) = call(&router, Method::POST, "/api/runs", Some(run(&a, "run-a"))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{run_a}");
    let stale = run_a["session_id"].as_str().unwrap().to_string();

    std::fs::rename(&a, &b).unwrap();
    // A run on the moved repository is admitted, in B.
    let (status, run_b) = call(&router, Method::POST, "/api/runs", Some(run(&b, "run-b"))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{run_b}");
    let (status, session_b) = call(
        &router,
        Method::GET,
        &format!("/api/sessions/{}", run_b["session_id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session_b}");
    assert_eq!(session_b["working_dir"], b.display().to_string());

    // Acting on the old Session is a 400 that names both folders and what
    // to do, and changes nothing.
    let (status, refused) = call(
        &router,
        Method::PUT,
        &format!("/api/sessions/{stale}/environment"),
        Some(serde_json::json!({"image": null, "setup_command": null, "setup_reviewed": true})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    let message = refused["error"].as_str().unwrap();
    for part in [
        stale.as_str(),
        &format!("it works in {}", a.display()),
        &format!("its Workspace's folder is now {}", b.display()),
        "or delete the Session",
    ] {
        assert!(message.contains(part), "{message:?} lacks {part:?}");
    }

    // DELETE without force closes it: 200, still listed, closed. Runs are
    // still admitted.
    let (status, closed) = call(
        &router,
        Method::DELETE,
        &format!("/api/sessions/{stale}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert_eq!(closed["deleted"], false);
    let (status, listed) = call(
        &router,
        Method::GET,
        &format!("/api/sessions/{stale}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["status"], "closed");
    let (status, run_c) = call(&router, Method::POST, "/api/runs", Some(run(&b, "run-c"))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{run_c}");

    // DELETE with force deletes it: 200, and it is gone from the list.
    let (status, deleted) = call(
        &router,
        Method::DELETE,
        &format!("/api/sessions/{stale}?force=true"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["deleted"], true);
    assert!(!session_ids(&router).await.contains(&stale));
    let (status, run_d) = call(&router, Method::POST, "/api/runs", Some(run(&b, "run-d"))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{run_d}");

    state.read().await.shutdown().await.unwrap();
}

#[tokio::test]
async fn runs_and_session_delete_work_after_a_repository_moved_away() {
    if std::env::var_os(CHILD).is_some() {
        moved_workspace_routes_body().await;
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let path = fake_podman(root.path());
    let data = root.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(300),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "missing_workspace_tests::runs_and_session_delete_work_after_a_repository_moved_away",
                "--nocapture",
            ])
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
