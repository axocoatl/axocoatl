//! A daemon a test starts in a child process of the test binary, because
//! bootstrap reads the process environment (`AXOCOATL_DATA_DIR`,
//! `AXOCOATL_SOCKET_PATH`): the child runs the same test again, which sees
//! its phase in an environment variable. It finds a fake rootless Podman
//! that starts no container, and talks to a stand-in for the audited
//! Ollama. A Session that must run a turn without a container gets an
//! [`AdmissionOnlySandbox`] as its Ready runtime.

use axocoatl_isolation::session_sandbox::{ExecResult, Sandbox};
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// In the child: what the daemon warns about goes to stderr, which the
/// parent reads.
pub(crate) fn log_warnings_to_stderr() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

/// A `PATH` whose `podman` (`root/bin/podman`, written once) is a fake
/// rootless Podman: it answers what bootstrap and shutdown ask and refuses
/// everything else, so no container starts.
pub(crate) fn fake_podman_path(root: &Path) -> String {
    let bin = root.join("bin");
    let podman = bin.join("podman");
    if !podman.exists() {
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            &podman,
            r#"#!/bin/sh
case "$*" in
  --version) printf 'podman version 5.0.0\n' ;;
  'machine list --format json') printf '[{"Running":true}]\n' ;;
  'info --format json') printf '{}\n' ;;
  'info --format {{.Host.Security.Rootless}}') printf 'true\n' ;;
  'ps '*) ;;
  'volume ls '*) ;;
  'rm '*|'volume rm '*|'network rm '*) ;;
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    format!("{}:/usr/bin:/bin", bin.display())
}

/// The audited Ollama ([`axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION`])
/// serving `models` (`name:tag`), each with its own digest. It answers the
/// contract probes, and a load acknowledges the model it was asked for.
/// `/api/chat` is the caller's to mount.
pub(crate) async fn audited_ollama(models: &[&str]) -> MockServer {
    let server = MockServer::start().await;
    serve_audited_ollama(&server, models, 0).await;
    server
}

/// What [`audited_ollama`] serves, on `server`, after `pulls` pulls of every
/// model: each pull gives the models other digests, as pulling a model again
/// after it changed in the registry does.
pub(crate) async fn serve_audited_ollama(server: &MockServer, models: &[&str], pulls: usize) {
    crate::ollama_contract_test_support::mount_contract_probes(server).await;
    let digest = |index: usize| format!("{pulls:02x}{:062x}", index + 0x40_06);
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
            serde_json::json!({"models": models.iter().enumerate().map(|(index, name)| {
                serde_json::json!({"name": name, "model": name, "digest": digest(index)})
            }).collect::<Vec<_>>()}),
        ),
        (
            "GET",
            "/api/ps",
            serde_json::json!({"models": models.iter().enumerate().map(|(index, name)| {
                serde_json::json!({"name": name, "model": name, "digest": digest(index),
                    "details": {"format": "gguf"}, "context_length": 32768, "runner": "llamacpp"})
            }).collect::<Vec<_>>()}),
        ),
    ] {
        Mock::given(method(verb))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = request.body_json().unwrap_or_default();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": body["model"], "created_at": "2026-10-23T00:00:00Z",
                "response": "", "done": true, "done_reason": "load"
            }))
        })
        .mount(server)
        .await;
}

/// `server`'s `/api/chat` answers every request with `content`, as a model
/// that stops there.
pub(crate) async fn answer_chats(server: &MockServer, content: &'static str) {
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(move |request: &wiremock::Request| {
            let body: serde_json::Value = request.body_json().unwrap_or_default();
            let record = serde_json::json!({"model": body["model"],
                "created_at": "2026-10-23T00:00:00Z",
                "message": {"role": "assistant", "content": content}, "done": true,
                "done_reason": "stop", "prompt_eval_count": 20, "eval_count": 2});
            ResponseTemplate::new(200).set_body_raw(format!("{record}\n"), "application/x-ndjson")
        })
        .mount(server)
        .await;
}

/// Run `test` (its path in this crate, which `--exact` matches) again in a
/// child process, with `child` set to `phase`, data root `data`, the IPC
/// socket under `root`, the fake Podman of [`fake_podman_path`] and `envs`.
pub(crate) async fn run_child(
    test: &str,
    child: &str,
    phase: &str,
    root: &Path,
    data: &Path,
    envs: &[(&str, &OsStr)],
) -> std::process::Output {
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture"])
        .env(child, phase)
        .env("AXOCOATL_DATA_DIR", data)
        .env("AXOCOATL_SOCKET_PATH", root.join("ipc/daemon.sock"))
        .env("PATH", fake_podman_path(root))
        .current_dir(root)
        .kill_on_drop(true);
    for (name, value) in envs {
        command.env(name, value);
    }
    tokio::time::timeout(Duration::from_secs(300), command.output())
        .await
        .unwrap()
        .unwrap()
}

/// What a child that succeeded printed, its stdout then its stderr.
pub(crate) fn succeeded(output: &std::process::Output) -> String {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{text}");
    text
}

/// A Ready runtime that admits ownership and Begin and runs nothing: no
/// command, no terminal, no egress decision point.
pub(crate) struct AdmissionOnlySandbox(pub(crate) PathBuf);

#[async_trait::async_trait]
impl Sandbox for AdmissionOnlySandbox {
    fn root(&self) -> &Path {
        &self.0
    }
    fn execution_identity(&self) -> Option<&str> {
        Some("admission-only-runtime")
    }
    async fn exec(
        &self,
        _argv: &[&str],
        _timeout: Duration,
    ) -> Result<ExecResult, axocoatl_isolation::IsolationError> {
        Err(axocoatl_isolation::IsolationError::Io(
            std::io::Error::other("the admission test runtime runs no command"),
        ))
    }
    async fn exec_stdin(
        &self,
        _argv: &[&str],
        _stdin: &str,
        _timeout: Duration,
    ) -> Result<ExecResult, axocoatl_isolation::IsolationError> {
        Err(axocoatl_isolation::IsolationError::Io(
            std::io::Error::other("the admission test runtime runs no command"),
        ))
    }
    fn spawn_background(&self, _command: &str) -> String {
        unreachable!("the admission test runtime runs no background task")
    }
    fn spawn_pty(
        &self,
        _command: &str,
        _rows: u16,
        _cols: u16,
    ) -> Result<Arc<axocoatl_isolation::pty::PtyTerminal>, String> {
        Err("the admission test runtime has no terminal".into())
    }
    fn get_terminal(&self, _id: &str) -> Option<Arc<axocoatl_isolation::pty::PtyTerminal>> {
        None
    }
    fn kill_terminal(&self, _id: &str) -> bool {
        false
    }
    fn list_terminals(&self) -> Vec<(String, String, bool)> {
        Vec::new()
    }
    fn list_tasks(&self) -> Vec<axocoatl_isolation::session_sandbox::BgTask> {
        Vec::new()
    }
    fn with_root(&self, root: &Path) -> Arc<dyn Sandbox> {
        Arc::new(Self(root.to_path_buf()))
    }
    async fn stop(&self) {}
}
