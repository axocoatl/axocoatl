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
pub(crate) const LOADOUT: &str = r#"
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

pub(crate) async fn call(
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

const MODEL: &str = "folder-model:latest";

/// A stand-in for the audited local Ollama, enough for admission to observe
/// [`MODEL`] (version, cloud mode, model, a generation-free load, its
/// loaded context, and the contract probes). No turn ever runs: every
/// run's environment fails under the fake Podman. Returns its base URL.
pub(crate) async fn ollama() -> String {
    use axum::routing::{get, post};
    use axum::Json;
    use serde_json::{json, Value};
    let digest = "a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72";
    let listed = json!({"models": [{"name": MODEL, "model": MODEL, "digest": digest}]});
    let loaded = json!({"models": [{"name": MODEL, "model": MODEL, "digest": digest,
        "details": {"format": "gguf"}, "context_length": 32768, "runner": "llamacpp"}]});
    let app = axum::Router::new()
        .route(
            "/api/version",
            // The release native execution's guarantees were audited against.
            get(|| async { Json(json!({"version": "0.40.6"})) }),
        )
        .route(
            "/api/status",
            get(|| async { Json(json!({"cloud": {"disabled": true}})) }),
        )
        .route(
            "/api/show",
            post(|| async {
                Json(
                    json!({"details": {"format": "gguf"}, "capabilities": ["completion", "tools"]}),
                )
            }),
        )
        .route("/api/tags", get(move || async move { Json(listed) }))
        .route("/api/ps", get(move || async move { Json(loaded) }))
        .route(
            "/api/generate",
            post(|Json(body): Json<Value>| async move {
                Json(
                    json!({"model": body["model"], "created_at": "2026-10-19T00:00:00Z",
                    "response": "", "done": true, "done_reason": "load"}),
                )
            }),
        )
        // The contract probes, as Ollama 0.40 answers them
        // (`axocoatl_llm_ollama::ContractProbe`): the over-long prompt is
        // refused, the one-token and the long request stop at their limit,
        // and the request offered a tool calls it.
        .route(
            "/api/chat",
            post(|Json(body): Json<Value>| async move {
                let over_long = body
                    .pointer("/messages/0/content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| content.starts_with("x x "));
                if over_long {
                    return (
                        StatusCode::BAD_REQUEST,
                        json!({"error": "request exceeds the available context size"}).to_string(),
                    );
                }
                let (message, reason, generated) = match body.get("tools") {
                    Some(_) => (
                        json!({"role": "assistant", "content": "", "tool_calls": [{
                            "id": "call_probe", "function": {"name": "report_ready",
                            "arguments": {"word": "ready"}}}]}),
                        "stop",
                        json!(31),
                    ),
                    None => (
                        json!({"role": "assistant", "content": "ready"}),
                        "length",
                        body["options"]["num_predict"].clone(),
                    ),
                };
                let record = json!({"model": body["model"], "created_at": "2026-10-19T00:00:00Z",
                    "message": message, "done": true, "done_reason": reason,
                    "prompt_eval_count": 14, "eval_count": generated});
                (StatusCode::OK, format!("{record}\n"))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}")
}

pub(crate) fn run(repo: &Path, id: &str) -> serde_json::Value {
    serde_json::json!({
        "loadout": "folder",
        "task": "Say done.",
        "repo": repo.display().to_string(),
        "params": {"writer_model": format!("ollama:{MODEL}")},
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
    let ollama = ollama().await;
    std::fs::write(
        &config_path,
        format!(
            "agents: []\nproviders:\n  ollama:\n    base_url: {ollama}\nsandbox:\n  backend: podman\n  network: bridge\nconsolidation:\n  enabled: false\n"
        ),
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

    // The dry run is its own route: 200 with what admission found, the
    // task optional, and nothing created.
    let mut dry = run(&a, "dry");
    dry.as_object_mut().unwrap().remove("task");
    dry.as_object_mut().unwrap().remove("request_id");
    let (status, preflight) = call(&router, Method::POST, "/api/runs/preflight", Some(dry)).await;
    assert_eq!(status, StatusCode::OK, "{preflight}");
    assert_eq!(preflight["ready"], true, "{preflight}");
    assert_eq!(preflight["ollama_models"][0]["loaded"], true, "{preflight}");
    assert_eq!(
        preflight["ollama_models"][0]["context_tokens"], 32768,
        "{preflight}"
    );
    // POST /api/runs never takes a dry run: any dry_run query is refused
    // with nothing started, never run for real.
    for query in ["?dry_run=1", "?dry_run", "?dry_run=yes", "?dry_run=0"] {
        let (status, refused) = call(
            &router,
            Method::POST,
            &format!("/api/runs{query}"),
            Some(run(&a, "never")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {refused}");
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("POST /api/runs/preflight"),
            "{refused}"
        );
    }
    assert!(session_ids(&router).await.is_empty());
    let (status, runs) = call(&router, Method::GET, "/api/runs", None).await;
    assert_eq!(status, StatusCode::OK, "{runs}");
    assert_eq!(runs, serde_json::json!([]));
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
