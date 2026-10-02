//! Code in a Session container cannot use the local API without the token.
//!
//! Starts `axocoatl serve` on loopback with bridge networking, creates a real
//! Podman-backed Session, and sends requests to the host from inside its
//! container. Run explicitly:
//!
//! ```text
//! CONTAINER_CONNECTION=<machine> cargo test -p axocoatl-cli \
//!   --test local_api_token_podman -- --ignored --nocapture
//! ```
//!
//! Set AXOCOATL_E2E_MODEL_CACHE to a directory with the all-MiniLM-L6-v2
//! files to avoid downloading the embedding model.
#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use support::{client, free_port, Daemon};

const SESSION_READY_TIMEOUT: Duration = Duration::from_secs(300);
const MODEL_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

fn write_config(root: &Path, port: u16) -> PathBuf {
    let config = root.join("axocoatl.yaml");
    std::fs::write(
        &config,
        format!(
            r#"agents:
  - id: token-probe
    name: Token Probe
    provider: ollama
    model: token-probe-model
    system_prompt: Local API token fixture. No turns are executed.
    depends_on: []
    tools: []
providers:
  ollama:
    base_url: "http://127.0.0.1:9"
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

fn git(directory: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=Axocoatl Test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(args)
        .current_dir(directory)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// Removes the Session container this test created if the daemon did not.
struct ContainerGuard(String);

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["rm", "--force", "--ignore", &self.0])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn container_exists(name: &str) -> bool {
    Command::new("podman")
        .args(["container", "exists", name])
        .status()
        .unwrap()
        .success()
}

/// Run `wget` inside the container. Returns the HTTP status the host
/// answered with, or `None` when no HTTP response arrived at all.
fn wget_in_container(
    container: &str,
    url: &str,
    headers: &[String],
    post: bool,
    stdin_token: Option<&str>,
) -> (Option<u16>, String) {
    let mut script = String::from("read -r token || true; wget -S -T 5 -O /dev/null");
    for header in headers {
        script.push_str(&format!(" --header \"{header}\""));
    }
    if post {
        script.push_str(" --post-data '{}'");
    }
    script.push_str(&format!(" '{url}' 2>&1"));
    let mut child = Command::new("podman")
        .args(["exec", "-i", container, "sh", "-c", &script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        // The secret travels on stdin so it never appears in an argv.
        let _ = writeln!(stdin, "{}", stdin_token.unwrap_or_default());
    }
    let output: Output = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let status = text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("HTTP/1.")?;
        rest.get(2..5)?.parse::<u16>().ok()
    });
    (status, text)
}

#[tokio::test]
#[ignore = "requires Podman; run this test explicitly"]
async fn session_container_cannot_use_the_local_api_without_the_token() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    copy_model_cache(&data);
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("README.md"), "token probe\n").unwrap();
    git(&workspace, &["init", "-q"]);
    git(&workspace, &["add", "README.md"]);
    git(&workspace, &["commit", "-q", "-m", "init"]);

    let port = free_port();
    let config = write_config(&root, port);
    let mut daemon = Daemon::start(&root, &config, port, 0).await;
    let token = std::fs::read_to_string(data.join("local-api-token")).unwrap();
    let client = client(port);
    let base = format!("http://127.0.0.1:{port}");

    let workspace_record: serde_json::Value = client
        .post(format!("{base}/api/workspaces"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "path": workspace, "name": "Token Probe" }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let workspace_id = workspace_record["id"].as_str().unwrap().to_string();
    let session: serde_json::Value = client
        .post(format!("{base}/api/workspaces/{workspace_id}/sessions"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "name": "Token Probe Session",
            "mode": { "kind": "single_agent", "agent_id": "token-probe" },
            "enabled_skills": [],
            "exposed_ports": [],
            "setup_approved": false,
            "setup_reviewed": true,
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let session_id = session["id"].as_str().unwrap().to_string();
    let container = format!("axo-ses-{session_id}");
    let _guard = ContainerGuard(container.clone());

    let deadline = Instant::now() + SESSION_READY_TIMEOUT;
    loop {
        let sessions: serde_json::Value = client
            .get(format!("{base}/api/sessions"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let state = sessions
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["id"] == session_id.as_str())
            .map(|candidate| candidate["environment"]["state"].clone())
            .unwrap_or_default();
        if state == "ready" && container_exists(&container) {
            break;
        }
        assert!(
            state != "failed" && Instant::now() < deadline,
            "Session environment did not become ready (state {state})\n{}",
            daemon.logs()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let host_url = |path: &str| format!("http://host.containers.internal:{port}{path}");
    let local_host = format!("Host: localhost:{port}");
    let cookie_name = format!("axocoatl-token-{port}");

    // The real token, sent from inside the container, proves the probe can
    // reach the daemon at all. Without that the refusals below say nothing.
    let (reachable, transcript) = wget_in_container(
        &container,
        &host_url("/api/sessions"),
        &[
            local_host.clone(),
            "Authorization: Bearer $token".to_string(),
        ],
        false,
        Some(&token),
    );
    let Some(reachable) = reachable else {
        // Rootless pasta on Linux usually maps host.containers.internal to a
        // non-loopback host address, so a loopback-bound daemon is
        // unreachable. Nothing can succeed then; record it and stop.
        eprintln!(
            "host.containers.internal did not reach the loopback daemon on {}; skipping the refusal matrix\n{transcript}",
            std::env::consts::OS
        );
        assert!(!transcript.contains(" 200 "), "{transcript}");
        daemon.stop();
        return;
    };
    assert_eq!(reachable, 200, "{transcript}");

    // The container's own Host name is refused before credentials matter.
    let (status, transcript) =
        wget_in_container(&container, &host_url("/api/sessions"), &[], false, None);
    assert_eq!(status, Some(421), "{transcript}");

    // A forged loopback Host still needs the token.
    for (path, post) in [("/api/sessions", false), ("/a2a/tasks", true), ("/", false)] {
        let (status, transcript) = wget_in_container(
            &container,
            &host_url(path),
            std::slice::from_ref(&local_host),
            post,
            None,
        );
        assert_eq!(status, Some(401), "{path}\n{transcript}");
    }
    for credential in [
        "Authorization: Bearer wrong-token".to_string(),
        "x-api-key: wrong-token".to_string(),
        format!("Cookie: {cookie_name}=wrong-token"),
    ] {
        let (status, transcript) = wget_in_container(
            &container,
            &host_url("/api/sessions"),
            &[local_host.clone(), credential.clone()],
            false,
            None,
        );
        assert_eq!(status, Some(401), "{credential}\n{transcript}");
    }

    // The data root, and with it the token file, is not visible in the
    // container.
    let token_path = data.join("local-api-token");
    let read = Command::new("podman")
        .args(["exec", &container, "cat"])
        .arg(&token_path)
        .output()
        .unwrap();
    assert!(!read.status.success());
    assert!(!String::from_utf8_lossy(&read.stdout).contains(&token));

    daemon.stop();
    let deadline = Instant::now() + Duration::from_secs(30);
    while container_exists(&container) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        !container_exists(&container),
        "the daemon left its Session container running"
    );
    assert!(!daemon.logs().contains(&token));
}
