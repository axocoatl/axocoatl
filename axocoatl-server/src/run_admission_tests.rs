//! `POST /api/runs` when its client goes away while admission still runs
//! after the run's Session was made (Ctrl-C while `axocoatl run` waits for
//! the Session's environment, its setup or its check probe). Before 1.5 the
//! handler's admission was dropped with the connection: the Session stayed
//! open with whatever it had started, and the request id stayed "being
//! admitted" until the daemon restarted. The daemon runs in a child process
//! with its own data root and a fake Podman that holds the Session's
//! preparation until the test lets it go, then fails it.
use crate::auth::AuthConfig;
use crate::middleware::{RateLimitConfig, RateLimiter};
use crate::missing_workspace_tests::{call, ollama, run, LOADOUT};
use crate::{build_router, AppState};
use axocoatl_daemon::AxocoatlDaemon;
use axum::http::{Method, StatusCode};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const CHILD: &str = "AXOCOATL_RUN_ADMISSION_ROUTE_CHILD";
/// The folder where the fake Podman says it is holding a command
/// (`held`) and waits until the test lets it go (`release`).
const GATE: &str = "AXOCOATL_RUN_ADMISSION_GATE";

/// A fake rootless Podman that answers admission's checks and holds the
/// first other command (the Session's preparation) until `release` exists
/// in `gate`, then fails it, as every other command after it.
fn fake_podman(root: &Path, gate: &Path) -> String {
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let podman = bin.join("podman");
    std::fs::write(
        &podman,
        format!(
            r#"#!/bin/sh
gate='{gate}'
case "$*" in
  --version) printf 'podman version 5.0.0\n' ;;
  'machine list --format json') printf '[{{"Running":true}}]\n' ;;
  'info --format json') printf '{{}}\n' ;;
  'info --format {{{{.Host.Security.Rootless}}}}') printf 'true\n' ;;
  'ps '*) ;;
  'volume ls '*) printf '[]\n' ;;
  'rm '*|'volume rm '*|'network rm '*) ;;
  *)
    if [ ! -e "$gate/held" ]; then
      printf '%s\n' "$*" > "$gate/held"
      while [ ! -e "$gate/release" ]; do sleep 0.1; done
    fi
    printf 'unexpected Podman command: %s\n' "$*" >&2
    exit 1
    ;;
esac
"#,
            gate = gate.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
    format!("{}:/usr/bin:/bin", bin.display())
}

async fn listed(router: &axum::Router, uri: &str) -> Vec<serde_json::Value> {
    let (status, listed) = call(router, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    listed.as_array().unwrap().clone()
}

async fn interrupted_admission_body() {
    let gate = std::path::PathBuf::from(std::env::var_os(GATE).unwrap());
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
    assert!(
        !gate.join("held").exists(),
        "daemon startup consumed the Session preparation barrier"
    );
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
    let repo = outside.path().canonicalize().unwrap().join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# repo\n").unwrap();

    // The client asks for the run and goes away once the run's Session
    // exists and Podman holds its preparation: its request is dropped, as
    // the server drops it when the connection closes.
    let request = tokio::spawn({
        let router = router.clone();
        let body = run(&repo, "gone");
        async move { call(&router, Method::POST, "/api/runs", Some(body)).await }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let session_id = loop {
        if gate.join("held").exists() {
            let sessions = listed(&router, "/api/sessions").await;
            if let [session] = sessions.as_slice() {
                break session["id"].as_str().unwrap().to_string();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Podman never held the Session's preparation"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        !request.is_finished(),
        "admission completed before the client disconnected"
    );
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());

    // The admission goes on without its client: the same request is still
    // being admitted, and no run is recorded yet.
    let (status, busy) = call(&router, Method::POST, "/api/runs", Some(run(&repo, "gone"))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{busy}");
    assert!(
        busy["error"]
            .as_str()
            .unwrap()
            .contains("request gone is being admitted"),
        "{busy}"
    );
    assert!(listed(&router, "/api/runs").await.is_empty());

    // Podman lets the preparation go and fails it: the admission ends on its
    // own, and the run is recorded as ended at its environment.
    std::fs::write(gate.join("release"), "").unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let ended = loop {
        let runs = listed(&router, "/api/runs").await;
        if let [run] = runs.as_slice() {
            if run["state"] != "preparing" && run["state"] != "running" {
                break run.clone();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the admission never ended without its client"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(ended["session_id"], session_id.as_str(), "{ended}");
    assert_eq!(ended["outcome"]["verdict"], "error", "{ended}");

    // The request id is no longer being admitted: a repeat answers with
    // the same run, and no other Session was made.
    let (status, again) = call(&router, Method::POST, "/api/runs", Some(run(&repo, "gone"))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{again}");
    assert_eq!(again["run_id"], ended["run_id"], "{again}");
    let sessions = listed(&router, "/api/sessions").await;
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    // A run that ended at its environment keeps its Session, whose
    // environment failed and which runs nothing.
    assert_ne!(sessions[0]["status"], "closed", "{sessions:?}");
    assert_eq!(
        sessions[0]["environment"]["state"], "failed",
        "{sessions:?}"
    );

    state.read().await.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_admission_whose_client_went_away_runs_to_its_end() {
    if std::env::var_os(CHILD).is_some() {
        interrupted_admission_body().await;
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let gate = root.path().join("gate");
    std::fs::create_dir(&gate).unwrap();
    let path = fake_podman(root.path(), &gate);
    let data = root.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(300),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "run_admission_tests::an_admission_whose_client_went_away_runs_to_its_end",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env(GATE, &gate)
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
    // Whatever happened, the fake Podman is let go.
    let _ = std::fs::write(gate.join("release"), "");
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
