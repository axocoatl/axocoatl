//! Which folders `POST /api/workspaces` opens, and removing a Workspace
//! with `DELETE /api/workspaces/{id}`. A new user's first dialog starts in
//! their home folder: the home folder, the folders above it and Axocoatl's
//! own directories are refused with a code, a broad folder opens only with
//! `confirm_broad`, and a Workspace leaves the list without its folder being
//! touched. The daemon runs in a child process with its own HOME, data root
//! and a fake Podman that is never asked to start anything.
use crate::auth::AuthConfig;
use crate::middleware::{RateLimitConfig, RateLimiter};
use crate::{build_router, AppState};
use axocoatl_daemon::AxocoatlDaemon;
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const CHILD: &str = "AXOCOATL_WORKSPACE_SCOPE_ROUTE_CHILD";

/// A fake rootless Podman that answers readiness and removes nothing real.
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
    body: Option<Value>,
) -> (StatusCode, Value) {
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
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, value)
}

async fn open(router: &Router, path: &Path, confirm_broad: bool) -> (StatusCode, Value) {
    let mut body = json!({"path": path.display().to_string()});
    if confirm_broad {
        body["confirm_broad"] = json!(true);
    }
    call(router, Method::POST, "/api/workspaces", Some(body)).await
}

fn refused(status: StatusCode, body: &Value, code: &str) {
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], code, "{body}");
    assert!(
        !body["error"].as_str().unwrap_or_default().is_empty(),
        "{body}"
    );
}

async fn workspace_ids(router: &Router) -> Vec<String> {
    let (status, listed) = call(router, Method::GET, "/api/workspaces", None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    listed
        .as_array()
        .unwrap()
        .iter()
        .map(|workspace| workspace["id"].as_str().unwrap().to_string())
        .collect()
}

async fn workspace_scope_routes_body() {
    let root = std::env::current_dir().unwrap().canonicalize().unwrap();
    let home = root.join("home");
    let data = root.join("data");
    // The per-user configuration directory, as on Linux: beside the data
    // root, inside the home folder.
    let config_dir = home.join(".config").join("axocoatl");
    let config_path = config_dir.join("config.yaml");
    std::fs::write(
        &config_path,
        "agents:\n  - id: lead\n    name: Lead\n    provider: ollama\n    model: scope-model\n    system_prompt: Scope fixture.\nproviders:\n  ollama:\n    base_url: http://127.0.0.1:9\nsandbox:\n  backend: podman\n  network: none\nconsolidation:\n  enabled: false\n",
    )
    .unwrap();
    let project = home.join("src").join("project");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::create_dir_all(home.join("src").join("notes")).unwrap();

    let config = axocoatl_config::load_config(&config_path).await.unwrap();
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    assert!(!daemon.control_plane_dir_paths().contains(&config_dir));
    daemon.set_user_config_path(&config_path);
    // The per-user configuration directory is hidden from every Session
    // whose Workspace contains it, like the data root.
    assert!(daemon.control_plane_dir_paths().contains(&config_dir));
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

    // Broad folders: 409 `workspace_too_broad`, nothing registered.
    for broad in [Path::new("/"), root.as_path(), home.as_path()] {
        let (status, body) = open(&router, broad, false).await;
        refused(status, &body, "workspace_too_broad");
    }
    let (status, body) = open(&router, &home, false).await;
    refused(status, &body, "workspace_too_broad");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("is your home folder"),
        "{body}"
    );
    // A folder that contains Axocoatl's per-user configuration directory.
    let (status, body) = open(&router, &home.join(".config"), false).await;
    refused(status, &body, "workspace_too_broad");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("configuration directory"),
        "{body}"
    );
    assert!(workspace_ids(&router).await.is_empty());

    // Axocoatl's own directories: 409 `workspace_protected`, even confirmed.
    for protected in [
        data.clone(),
        data.join("workspaces"),
        config_dir.clone(),
        root.join("ipc"),
    ] {
        for confirm in [false, true] {
            let (status, body) = open(&router, &protected, confirm).await;
            refused(status, &body, "workspace_protected");
        }
    }
    assert!(workspace_ids(&router).await.is_empty());

    // The legacy create path refuses an unopened broad folder too.
    let (status, body) = call(
        &router,
        Method::POST,
        "/api/sessions",
        Some(json!({
            "name": "In home",
            "working_dir": home.display().to_string(),
            "mode": {"kind": "single_agent", "agent_id": "lead"},
        })),
    )
    .await;
    refused(status, &body, "workspace_too_broad");
    assert!(workspace_ids(&router).await.is_empty());

    // The folder picker marks Git repositories and says why a folder would
    // be refused.
    let (status, listing) = call(
        &router,
        Method::GET,
        &format!(
            "/api/fs/list?path={}",
            urlencoding(&home.join("src").display().to_string())
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listing}");
    let dirs = listing["dirs"].as_array().unwrap();
    let git_of = |name: &str| {
        dirs.iter()
            .find(|entry| entry["name"] == name)
            .unwrap_or_else(|| panic!("{name} listed: {listing}"))["git"]
            .clone()
    };
    assert_eq!(git_of("project"), json!(true));
    assert_eq!(git_of("notes"), json!(false));
    assert!(listing.get("workspace_scope").is_none(), "{listing}");
    let (status, listing) = call(
        &router,
        Method::GET,
        &format!(
            "/api/fs/list?path={}",
            urlencoding(&home.display().to_string())
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listing}");
    assert_eq!(listing["workspace_scope"]["code"], "workspace_too_broad");
    assert_eq!(listing["git"], false);
    let (status, listing) = call(
        &router,
        Method::GET,
        &format!(
            "/api/fs/list?path={}",
            urlencoding(&project.display().to_string())
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listing}");
    assert_eq!(listing["git"], true);

    // Confirmed, the home folder opens; removing it leaves the folder.
    let (status, confirmed) = open(&router, &home, true).await;
    assert_eq!(status, StatusCode::OK, "{confirmed}");
    let home_id = confirmed["id"].as_str().unwrap().to_string();
    let (status, removed) = call(
        &router,
        Method::DELETE,
        &format!("/api/workspaces/{home_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["id"], home_id.as_str());
    assert!(!workspace_ids(&router).await.contains(&home_id));
    assert!(project.join(".git").is_dir());
    let (status, missing) = call(
        &router,
        Method::DELETE,
        &format!("/api/workspaces/{home_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");

    // A project folder opens; while its Session is open it cannot be
    // removed, and once that Session is closed it can.
    let (status, workspace) = open(&router, &project, false).await;
    assert_eq!(status, StatusCode::OK, "{workspace}");
    let workspace_id = workspace["id"].as_str().unwrap().to_string();
    let (status, session) = call(
        &router,
        Method::POST,
        &format!("/api/workspaces/{workspace_id}/sessions"),
        Some(json!({
            "name": "Open Session",
            "mode": {"kind": "single_agent", "agent_id": "lead"},
            // Unreviewed setup: the Session waits for a decision and
            // Podman is not asked to start anything.
            "setup_reviewed": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let session_id = session["id"].as_str().unwrap().to_string();
    let (status, body) = call(
        &router,
        Method::DELETE,
        &format!("/api/workspaces/{workspace_id}"),
        None,
    )
    .await;
    refused(status, &body, "workspace_has_open_sessions");
    assert!(workspace_ids(&router).await.contains(&workspace_id));

    let (status, closed) = call(
        &router,
        Method::DELETE,
        &format!("/api/sessions/{session_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    let (status, removed) = call(
        &router,
        Method::DELETE,
        &format!("/api/workspaces/{workspace_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert!(!workspace_ids(&router).await.contains(&workspace_id));
    // The closed Session stays reviewable.
    let (status, kept) = call(
        &router,
        Method::GET,
        &format!("/api/sessions/{session_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{kept}");
    assert_eq!(kept["status"], "closed");
    assert!(project.join(".git").is_dir());

    // The readiness report: Podman (the fake) is ready, the configured
    // Ollama is not reachable, and /health names the version.
    let (status, report) = call(&router, Method::GET, "/api/readiness", None).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["ready"], false, "{report}");
    assert_eq!(report["sections"]["sandbox"][0]["id"], "podman");
    assert_eq!(report["sections"]["sandbox"][0]["status"], "ok", "{report}");
    assert_eq!(
        report["sections"]["models"][0]["status"], "fail",
        "{report}"
    );
    assert!(report["sessions_blocked"].is_null(), "{report}");
    let (status, health) = call(&router, Method::GET, "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["version"], report["version"]);

    state.read().await.shutdown().await.unwrap();
}

fn urlencoding(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn broad_and_protected_folders_are_refused_and_workspaces_can_be_removed() {
    if std::env::var_os(CHILD).is_some() {
        workspace_scope_routes_body().await;
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let path = fake_podman(&root_path);
    let home = root_path.join("home");
    std::fs::create_dir_all(home.join(".config").join("axocoatl")).unwrap();
    let data = root_path.join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(300),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace_scope_tests::broad_and_protected_folders_are_refused_and_workspaces_can_be_removed",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("HOME", &home)
            .env_remove("XDG_RUNTIME_DIR")
            .env("AXOCOATL_DATA_DIR", &data)
            .env("AXOCOATL_SOCKET_PATH", root_path.join("ipc/daemon.sock"))
            .env("PATH", path)
            .current_dir(&root_path)
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
