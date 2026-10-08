//! Session lifecycle and run admission through `axocoatl run` and the HTTP
//! API, against a real daemon and real Podman, as the 1.3 smoke runs used
//! them:
//! - a data directory made (and given a stored secret) before the daemon
//!   first started runs loadouts with no `axocoatl session upgrade`;
//! - while one run's turn holds the repository's Workspace, another run
//!   exits 7 (busy) at once and still writes its `--junit` file (the
//!   verdict an `<error type="busy">`, exit code 7) and no `--record` file;
//!   closing the idle Session of an earlier run, and creating a Session on
//!   the repository (`POST /api/sessions` and `POST
//!   /api/workspaces/{id}/sessions`), are each a `409` with the code
//!   `workspace_busy` at once, naming the run that holds it, not a `500`
//!   after 60 seconds or an answer once that turn ends;
//! - `GET /api/sessions/{id}/export` serves a run Session's versioned
//!   History as JSON and Markdown, and an unknown Session is a `404`;
//! - once the holding run is stopped, Close succeeds and removes the
//!   Session's runtime volumes from Podman;
//! - a run's Session left open across a daemon restart loads again with no
//!   quarantine (its loadout binding intact), exports and closes.
//!
//! ```text
//! CONTAINER_CONNECTION=<machine> cargo test -p axocoatl-cli \
//!   --test run_lifecycle_podman -- --ignored --nocapture
//! ```
//!
//! Set AXOCOATL_E2E_MODEL_CACHE to a directory with the all-MiniLM-L6-v2
//! files to avoid downloading the embedding model.
#![cfg(unix)]

mod support;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use support::{axocoatl, client, free_port, Daemon};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const MODEL: &str = "lifecycle-model:latest";
const STALL: &str = "Hold the Workspace until you are stopped.";
const RUN_TIMEOUT: Duration = Duration::from_secs(600);
const MODEL_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

/// One writer that may only read files, no checks and no review.
const LOADOUT: &str = r#"schema: axocoatl.loadout/1
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

/// A local Ollama the native provider admits. `/api/chat` answers "Done."
/// at once, except for [`STALL`], which it holds for five minutes.
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
        .respond_with(|request: &Request| {
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
        .respond_with(|request: &Request| {
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

/// The configured Agent a workbench Session is created with.
const HELPER: &str = "helper";

fn write_config(root: &Path, port: u16, model_url: &str) -> PathBuf {
    let config = root.join("axocoatl.yaml");
    std::fs::write(
        &config,
        format!(
            r#"agents:
  - id: {HELPER}
    name: Helper
    provider: ollama
    model: {MODEL}
    role: autonomous
    tools: [read_file]
providers:
  ollama:
    base_url: "{model_url}"
server:
  host: 127.0.0.1
  port: {port}
sandbox:
  backend: podman
  network: bridge
  require_resource_limits: false
consolidation:
  enabled: false
"#
        ),
    )
    .unwrap();
    std::fs::create_dir_all(root.join("loadouts")).unwrap();
    std::fs::write(root.join("loadouts").join("lifecycle.yaml"), LOADOUT).unwrap();
    config
}

fn copy_model_cache(data: &Path) {
    let Some(cache) = std::env::var_os("AXOCOATL_E2E_MODEL_CACHE").map(PathBuf::from) else {
        return;
    };
    if !MODEL_FILES.iter().all(|name| cache.join(name).is_file()) {
        return;
    }
    let destination = data.join("models").join("all-MiniLM-L6-v2");
    std::fs::create_dir_all(&destination).unwrap();
    for name in MODEL_FILES {
        std::fs::copy(cache.join(name), destination.join(name)).unwrap();
    }
}

fn run_args<'a>(
    repo: &'a Path,
    task: &'a str,
    writer: &'a str,
    base: &'a str,
    config: &'a Path,
) -> Vec<&'a std::ffi::OsStr> {
    vec![
        "run".as_ref(),
        "lifecycle".as_ref(),
        "--task".as_ref(),
        task.as_ref(),
        "--repo".as_ref(),
        repo.as_os_str(),
        "--model".as_ref(),
        writer.as_ref(),
        "--url".as_ref(),
        base.as_ref(),
        "-c".as_ref(),
        config.as_os_str(),
    ]
}

fn run_cli(root: &Path, args: &[&std::ffi::OsStr]) -> tokio::process::Child {
    let mut command = tokio::process::Command::from(axocoatl(root));
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.spawn().unwrap()
}

async fn finished(child: tokio::process::Child) -> Output {
    tokio::time::timeout(RUN_TIMEOUT, child.wait_with_output())
        .await
        .expect("the run ended")
        .unwrap()
}

fn podman_exists(kind: &str, name: &str) -> bool {
    let status = Command::new("podman")
        .args([kind, "exists", name])
        .status()
        .unwrap();
    match status.code() {
        Some(0) => true,
        Some(1) => false,
        other => panic!("podman {kind} exists {name}: {other:?}"),
    }
}

/// The volumes a Session's runtime fills again when it starts.
fn runtime_volumes(session_id: &str) -> [String; 4] {
    ["axo-egr-", "axo-egi-", "axo-svc-", "axo-ca-"].map(|prefix| format!("{prefix}{session_id}"))
}

/// Removes what the daemon created for these Sessions if it did not, by
/// exact name.
struct SessionGuard(std::sync::Mutex<Vec<String>>);

impl Drop for SessionGuard {
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
            let _ = Command::new("podman")
                .args(["rm", "--force", "--ignore"])
                .args(&containers)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            for volume in runtime_volumes(id)
                .into_iter()
                .chain([format!("axo-ses-{id}-node-modules")])
            {
                let _ = Command::new("podman")
                    .args(["volume", "rm", "--force", &volume])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

/// The run on `session`'s `GET /api/runs` row whose Session is not
/// `except`, once there is one.
async fn other_run(base: &str, port: u16, token: &str, except: &str) -> (String, String) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let runs: serde_json::Value = client(port)
            .get(format!("{base}/api/runs"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(run) = runs
            .as_array()
            .into_iter()
            .flatten()
            .find(|run| run["session_id"].as_str() != Some(except))
        {
            return (
                run["run_id"].as_str().unwrap().to_string(),
                run["session_id"].as_str().unwrap().to_string(),
            );
        }
        assert!(Instant::now() < deadline, "the second run was not admitted");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Podman; run this test explicitly"]
async fn a_busy_workspace_exits_seven_and_close_refuses_at_once_then_removes_runtime_volumes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# Lifecycle\n").unwrap();

    let model = model_server().await;
    let port = free_port();
    let config = write_config(&root, port, &model.uri());
    // The data directory exists before the daemon first starts, with a
    // stored secret in it, as in the smoke runs. No `session upgrade`.
    let data = root.join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    copy_model_cache(&data);
    let mut secret = axocoatl(&root)
        .args(["secret", "set", "lifecycle-dummy", "-c"])
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    secret
        .stdin
        .take()
        .unwrap()
        .write_all(b"not-a-real-value\n")
        .unwrap();
    let stored = secret.wait_with_output().unwrap();
    assert!(
        stored.status.success(),
        "{}",
        String::from_utf8_lossy(&stored.stderr)
    );
    let mut daemon = Daemon::start(&root, &config, port, 0).await;
    let token = std::fs::read_to_string(data.join("local-api-token"))
        .unwrap()
        .trim()
        .to_string();
    let base = format!("http://127.0.0.1:{port}");
    let writer = format!("writer=ollama:{MODEL}");
    let guard = SessionGuard(std::sync::Mutex::new(Vec::new()));

    // Run A passes; its Session stays open and idle.
    let ran = finished(run_cli(
        &root,
        &run_args(&repo, "Say done.", &writer, &base, &config),
    ))
    .await;
    let out = String::from_utf8_lossy(&ran.stdout).to_string();
    let err = String::from_utf8_lossy(&ran.stderr).to_string();
    assert_eq!(
        ran.status.code(),
        Some(0),
        "stdout:\n{out}\nstderr:\n{err}\ndaemon:\n{}",
        daemon.logs()
    );
    let a_session = err
        .lines()
        .find_map(|line| line.split(" in Session ").nth(1))
        .unwrap_or_else(|| panic!("no Session in\n{err}"))
        .trim()
        .to_string();
    guard.0.lock().unwrap().push(a_session.clone());

    // Run B's turn holds the Workspace until it is stopped.
    let b = run_cli(&root, &run_args(&repo, STALL, &writer, &base, &config));
    let (b_run, b_session) = other_run(&base, port, &token, &a_session).await;
    guard.0.lock().unwrap().push(b_session.clone());
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let calls = model.received_requests().await.unwrap_or_default();
        if calls.iter().any(|request| {
            request.url.path() == "/api/chat"
                && String::from_utf8_lossy(&request.body).contains(STALL)
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "run B's provider call never started\n{}",
            daemon.logs()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // A third run on the repository exits 7 at once and names B's Session.
    // Its JUnit file says so; there is no run to record.
    let asked = Instant::now();
    let junit = root.join("refused-junit.xml");
    let record = root.join("refused.axorecord.jsonl");
    let mut refused_args = run_args(&repo, "Say done.", &writer, &base, &config);
    refused_args.extend([
        "--junit".as_ref(),
        junit.as_os_str(),
        "--record".as_ref(),
        record.as_os_str(),
    ]);
    let refused = finished(run_cli(&root, &refused_args)).await;
    let refused_err = String::from_utf8_lossy(&refused.stderr).to_string();
    assert_eq!(refused.status.code(), Some(7), "{refused_err}");
    assert!(
        asked.elapsed() < Duration::from_secs(30),
        "{:?}",
        asked.elapsed()
    );
    assert!(refused_err.contains("Workspace busy: "), "{refused_err}");
    assert!(refused_err.contains(&b_session), "{refused_err}");
    assert!(!refused_err.contains("· run "), "{refused_err}");
    let xml = std::fs::read_to_string(&junit)
        .unwrap_or_else(|error| panic!("no JUnit file ({error})\n{refused_err}"));
    assert!(
        xml.contains("<property name=\"axocoatl.exit_code\" value=\"7\"/>"),
        "{xml}"
    );
    assert!(xml.contains("<error type=\"busy\" message=\""), "{xml}");
    assert!(xml.contains(&b_session), "{xml}");
    assert!(!record.exists());
    assert!(
        refused_err.contains("no run was admitted, so there is nothing to record"),
        "{refused_err}"
    );

    // Creating a Session on the repository is a 409 with the code at once,
    // by path and by Workspace, naming B; no Session is created.
    let sessions = |token: String| {
        let base = base.clone();
        async move {
            let listed: serde_json::Value = client(port)
                .get(format!("{base}/api/sessions"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            listed.as_array().unwrap().len()
        }
    };
    let before = sessions(token.clone()).await;
    let workspaces: serde_json::Value = client(port)
        .get(format!("{base}/api/workspaces"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let workspace_id = workspaces
        .as_array()
        .unwrap()
        .iter()
        .find(|workspace| {
            workspace["canonical_path"].as_str() == Some(repo.to_str().unwrap())
                || workspace["path"].as_str() == Some(repo.to_str().unwrap())
        })
        .unwrap_or_else(|| panic!("no Workspace for {} in {workspaces}", repo.display()))["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mode = serde_json::json!({"kind": "single_agent", "agent_id": HELPER});
    for (path, body) in [
        (
            "/api/sessions".to_string(),
            serde_json::json!({"name": "probe", "working_dir": repo, "mode": mode}),
        ),
        (
            format!("/api/workspaces/{workspace_id}/sessions"),
            serde_json::json!({"name": "probe", "mode": mode, "setup_reviewed": true}),
        ),
    ] {
        let asked = Instant::now();
        let created = client(port)
            .post(format!("{base}{path}"))
            .bearer_auth(&token)
            .json(&body)
            .timeout(Duration::from_secs(50))
            .send()
            .await
            .unwrap_or_else(|error| panic!("{path} waited for B's turn: {error}"));
        assert_eq!(created.status().as_u16(), 409, "{path}");
        assert!(
            asked.elapsed() < Duration::from_secs(10),
            "{path}: {:?}",
            asked.elapsed()
        );
        let body: serde_json::Value = created.json().await.unwrap();
        assert_eq!(body["code"], "workspace_busy", "{path}: {body}");
        let message = body["error"].as_str().unwrap();
        assert!(message.contains("No Session was created"), "{message}");
        assert!(message.contains(&b_session), "{message}");
        assert!(message.contains(&b_run), "{message}");
    }
    assert_eq!(sessions(token.clone()).await, before);

    // Closing idle A is a 409 with the code at once, naming B.
    let asked = Instant::now();
    let close = client(port)
        .delete(format!("{base}/api/sessions/{a_session}?force=false"))
        .bearer_auth(&token)
        .timeout(Duration::from_secs(50))
        .send()
        .await
        .expect("Close answers before the Workspace is free");
    assert_eq!(close.status().as_u16(), 409);
    assert!(
        asked.elapsed() < Duration::from_secs(10),
        "{:?}",
        asked.elapsed()
    );
    let body: serde_json::Value = close.json().await.unwrap();
    assert_eq!(body["code"], "workspace_busy", "{body}");
    let message = body["error"].as_str().unwrap();
    assert!(message.contains(&b_session), "{message}");
    assert!(
        message.contains(&format!("Session {a_session} was not closed")),
        "{message}"
    );

    // A's History exports in the versioned form, as JSON and as Markdown.
    let exported = client(port)
        .get(format!(
            "{base}/api/sessions/{a_session}/export?format=json"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(exported.status().as_u16(), 200);
    let entries: serde_json::Value = exported.json().await.unwrap();
    let entries = entries.as_array().expect("the export is a list of entries");
    assert!(!entries.is_empty());
    assert!(
        entries
            .iter()
            .all(|entry| entry["history_version"] == "execution_v2"),
        "{entries:?}"
    );
    let markdown = client(port)
        .get(format!("{base}/api/sessions/{a_session}/export"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(markdown.status().as_u16(), 200);
    let markdown = markdown.text().await.unwrap();
    assert!(markdown.contains("Say done."), "{markdown}");
    // A Session the daemon does not know is not found.
    let unknown = client(port)
        .get(format!(
            "{base}/api/sessions/ses-00000000-0000-4000-8000-000000000000/export"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status().as_u16(), 404);

    // Stop B: its command exits 6, and A then closes.
    let stopped = client(port)
        .post(format!("{base}/api/runs/{b_run}/stop"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(stopped.status().as_u16(), 202);
    let b = finished(b).await;
    assert_eq!(
        b.status.code(),
        Some(6),
        "{}",
        String::from_utf8_lossy(&b.stderr)
    );
    let close = |session: String, token: String| {
        let base = base.clone();
        async move {
            let closed = client(port)
                .delete(format!("{base}/api/sessions/{session}?force=false"))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap();
            assert_eq!(
                closed.status().as_u16(),
                200,
                "{}",
                closed.text().await.unwrap_or_default()
            );
            // Close leaves no container and no runtime volume of the Session.
            assert!(!podman_exists("container", &format!("axo-ses-{session}")));
            for volume in runtime_volumes(&session) {
                assert!(!podman_exists("volume", &volume), "{volume} survived Close");
            }
        }
    };
    assert!(podman_exists("volume", &format!("axo-egr-{b_session}")));
    close(b_session.clone(), token.clone()).await;

    // Restart the daemon with A still open: its loadout Session loads
    // again, with no quarantine, and can still be exported and closed.
    daemon.stop();
    let mut daemon = Daemon::start(&root, &config, port, 1).await;
    let restarted = std::fs::read_to_string(root.join("stderr-1.log")).unwrap_or_default()
        + &std::fs::read_to_string(root.join("stdout-1.log")).unwrap_or_default();
    assert!(!restarted.contains("quarantined"), "{restarted}");
    assert!(
        !restarted
            .lines()
            .any(|line| line.contains("ERROR") && line.contains(&a_session)),
        "{restarted}"
    );
    let token = std::fs::read_to_string(data.join("local-api-token"))
        .unwrap()
        .trim()
        .to_string();
    let loaded: serde_json::Value = client(port)
        .get(format!("{base}/api/sessions/{a_session}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(loaded["status"], "active", "{loaded}");
    assert!(loaded["loadout"]["run_id"].is_string(), "{loaded}");
    let exported = client(port)
        .get(format!(
            "{base}/api/sessions/{a_session}/export?format=json"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(exported.status().as_u16(), 200);
    close(a_session.clone(), token.clone()).await;

    daemon.stop();
    drop(guard);
}
