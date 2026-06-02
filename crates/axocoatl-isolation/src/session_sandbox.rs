//! Per-session OCI container sandbox.
//!
//! Each directory session runs inside its own long-lived **podman** container
//! with the session's working directory bind-mounted at the same path. Every
//! session tool (file ops and shell) runs as a command *inside* this container
//! via `exec`, so the container is the security boundary: tools cannot reach
//! the host filesystem outside the mounted directory, and run under memory/CPU
//! caps.
//!
//! Podman is rootless, daemonless, and cross-platform (native on Linux/WSL, a
//! managed VM on macOS/Windows) — see [`crate::podman`]. Docker is not used.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::error::IsolationError;
use crate::podman;

/// The container runtime executable — always podman.
const PODMAN: &str = "podman";

/// Default base image for session containers — small, with a POSIX shell and
/// the busybox coreutils/grep/find the file + shell tools rely on.
pub const DEFAULT_IMAGE: &str = "docker.io/library/alpine:3.20";

/// The outcome of running a command inside the sandbox.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl ExecResult {
    /// True iff the command exited 0.
    pub fn ok(&self) -> bool {
        self.exit_code == 0
    }
}

/// A long-running background task inside a session container.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BgTask {
    pub id: String,
    pub command: String,
    /// "running" | "exited (N)" | "failed: …".
    pub status: String,
    /// Captured output, tail-trimmed.
    pub log: String,
}

/// Internal handle to a background task — the spawned reader updates `status`
/// and `log` in place.
struct BgTaskHandle {
    id: String,
    command: String,
    status: std::sync::Arc<std::sync::Mutex<String>>,
    log: std::sync::Arc<std::sync::Mutex<String>>,
}

/// A live per-session container. Dropping it does **not** stop the container —
/// call [`SessionSandbox::stop`] explicitly so the daemon controls lifecycle.
pub struct SessionSandbox {
    /// Container name — `axo-ses-{session_id}`.
    container: String,
    /// Background tasks started in this container.
    tasks: std::sync::Mutex<Vec<BgTaskHandle>>,
    /// Interactive PTY-backed terminals.
    terminals: std::sync::Mutex<Vec<std::sync::Arc<crate::pty::PtyTerminal>>>,
}

impl SessionSandbox {
    /// Start a sandbox container for `session_id` with `working_dir`
    /// bind-mounted read-write at the same path inside the container.
    ///
    /// Ensures podman is ready first (installing / starting its VM as needed),
    /// and removes any stale container of the same name, so this is safe to
    /// call after a daemon restart.
    pub async fn start(
        session_id: &str,
        working_dir: &Path,
        image: Option<&str>,
        exposed_ports: &[u16],
        post_create_commands: &[String],
    ) -> Result<Self, IsolationError> {
        podman::ensure_ready().await?;

        let container = format!("axo-ses-{session_id}");
        let dir = working_dir.to_string_lossy().to_string();
        let image = image.unwrap_or(DEFAULT_IMAGE);

        // Best-effort: clear a stale container with the same name.
        let _ = Command::new(PODMAN)
            .args(["rm", "-f", &container])
            .output()
            .await;

        let mount = format!("{dir}:{dir}:rw");

        // Start the long-lived idle container. Two independent best-effort
        // toggles can each fail and trigger a retry without that feature:
        //   - resource caps (cgroup delegation not always available — rootless
        //     podman on WSL2 can't apply them)
        //   - port publishing (host port already bound by another process)
        // Loop until something boots or we exhaust the fallbacks.
        let mut with_limits = true;
        // Owned copy so we can drop individual conflicting ports across
        // retries without losing the original list.
        let mut publish: Vec<u16> = exposed_ports.to_vec();
        loop {
            match Self::run_container(&container, &mount, &dir, image, with_limits, &publish).await
            {
                Ok(()) => break,
                Err(e) if e.contains("cgroup") && with_limits => {
                    tracing::warn!(
                        "this host cannot apply container resource limits \
                         (rootless podman / no cgroup delegation) — starting \
                         the sandbox without memory/CPU caps"
                    );
                    with_limits = false;
                }
                Err(e) if Self::is_port_conflict(&e) && !publish.is_empty() => {
                    // Parse the conflicting port out of the error and drop
                    // just that one; the rest of the dev ports stay published.
                    match Self::extract_conflicting_port(&e) {
                        Some(bad) if publish.contains(&bad) => {
                            tracing::warn!(
                                "host port {bad} already in use — dropping it \
                                 from this session's published ports (other \
                                 ports stay mapped). Free the port and \
                                 recreate the session to get it back."
                            );
                            publish.retain(|p| *p != bad);
                        }
                        _ => {
                            tracing::warn!(
                                "port conflict but couldn't identify which \
                                 port ({e}) — dropping all port forwarding \
                                 for this session"
                            );
                            publish.clear();
                        }
                    }
                }
                Err(e) => return Err(IsolationError::OciContainerFailed(e)),
            }
            let _ = Command::new(PODMAN)
                .args(["rm", "-f", &container])
                .output()
                .await;
        }

        // Install common dev essentials so the Terminals pane is useful out
        // of the box. Alpine ships only busybox — `cd`/`ls`/`cat` work, but
        // `bash`, `vim`, `nano`, `python3`, `node` don't. Best-effort: on
        // failure we leave a tracing warning and continue (the user can still
        // use `sh`, and Alpine's `apk add` later if they want).
        //
        // On non-Alpine images (python:slim, node:slim, etc.) this is a no-op
        // because `apk` doesn't exist — those images are assumed to already
        // ship the tools their users expect.
        Self::install_dev_essentials(&container).await;

        // Honour devcontainer.json's `postCreateCommand` (and any analogue we
        // collect later). These are project-author setup scripts — `npm ci`,
        // `pip install -r requirements.txt`, etc. Run once, best-effort: a
        // failure logs but doesn't kill the session.
        for script in post_create_commands {
            tracing::info!(
                "running post-create script in session container ({container}): {script}"
            );
            let out = Command::new(PODMAN)
                .args(["exec", &container, "sh", "-c", script])
                .output()
                .await;
            match out {
                Ok(o) if !o.status.success() => tracing::warn!(
                    "post-create script failed (exit {:?}): {}",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                Err(e) => tracing::warn!("post-create script could not run: {e}"),
                _ => {}
            }
        }

        Ok(Self {
            container,
            tasks: std::sync::Mutex::new(Vec::new()),
            terminals: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Run `apk add` for the toolchain users expect when they pop open a
    /// terminal in a session: shell, editors, scripting languages, git.
    /// Specific to Alpine-based images; a no-op on other distros (the apk
    /// command just won't exist and we log + carry on).
    async fn install_dev_essentials(container: &str) {
        let packages = "bash vim nano less git curl wget \
                        python3 py3-pip nodejs npm coreutils";
        tracing::info!("provisioning session container ({container}): installing dev essentials");
        let script = format!("command -v apk >/dev/null 2>&1 && apk add --no-cache {packages} >/dev/null 2>&1 || true");
        let _ = Command::new(PODMAN)
            .args(["exec", container, "sh", "-c", &script])
            .output()
            .await;
    }

    fn is_port_conflict(stderr: &str) -> bool {
        let lc = stderr.to_lowercase();
        lc.contains("port is already allocated")
            || lc.contains("address already in use")
            || lc.contains("bind: address")
            || lc.contains("rootlessport")
    }

    /// Pull the offending host port out of a podman port-conflict message.
    /// Matches both `0.0.0.0:3000:` and `tcp:3000` style fragments; first hit
    /// wins. Returns `None` if no port number appears in the error.
    fn extract_conflicting_port(stderr: &str) -> Option<u16> {
        // Look for a token shaped like `:NNNN` or `:NNNN:` — that's how podman
        // formats the offending address in its "bind: address already in use"
        // and "port is already allocated" errors.
        let bytes = stderr.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b':' {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > i + 1 && j - i - 1 <= 5 {
                    if let Ok(n) = std::str::from_utf8(&bytes[i + 1..j])
                        .unwrap()
                        .parse::<u16>()
                    {
                        // Skip obvious non-port numbers (line:column, etc.)
                        if n >= 1024 {
                            return Some(n);
                        }
                    }
                }
                i = j;
            } else {
                i += 1;
            }
        }
        None
    }

    /// `podman run -d` the idle session container. `with_limits` adds
    /// memory/CPU caps. `ports` are published 1:1 to the host. On failure
    /// returns podman's stderr so the caller can decide how to recover.
    async fn run_container(
        container: &str,
        mount: &str,
        dir: &str,
        image: &str,
        with_limits: bool,
        ports: &[u16],
    ) -> Result<(), String> {
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            container.into(),
            "-v".into(),
            mount.into(),
            "-w".into(),
            dir.into(),
        ];
        if with_limits {
            args.extend(["--memory".into(), "2g".into(), "--cpus".into(), "2".into()]);
        }
        for p in ports {
            args.push("-p".into());
            args.push(format!("{p}:{p}"));
        }
        args.push(image.into());
        args.push("sleep".into());
        args.push("infinity".into());

        let out = Command::new(PODMAN)
            .args(&args)
            .output()
            .await
            .map_err(|e| format!("spawning podman: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "starting session container: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// Run a command inside the session container.
    pub async fn exec(
        &self,
        argv: &[&str],
        timeout: Duration,
    ) -> Result<ExecResult, IsolationError> {
        let mut cmd = Command::new(PODMAN);
        cmd.arg("exec").arg(&self.container).args(argv);
        let out = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| IsolationError::Timeout(timeout))?
            .map_err(|e| IsolationError::OciContainerFailed(e.to_string()))?;
        Ok(ExecResult {
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            exit_code: out.status.code().unwrap_or(-1),
        })
    }

    /// Run a command inside the container with `stdin` piped in — used to
    /// write file contents (`exec_stdin(&["sh","-c","cat > path"], content)`).
    pub async fn exec_stdin(
        &self,
        argv: &[&str],
        stdin: &str,
        timeout: Duration,
    ) -> Result<ExecResult, IsolationError> {
        let mut child = Command::new(PODMAN)
            .arg("exec")
            .arg("-i")
            .arg(&self.container)
            .args(argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| IsolationError::OciContainerFailed(e.to_string()))?;

        if let Some(mut sink) = child.stdin.take() {
            sink.write_all(stdin.as_bytes())
                .await
                .map_err(IsolationError::Io)?;
            // Drop closes stdin so the inner command sees EOF.
            drop(sink);
        }

        let out = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| IsolationError::Timeout(timeout))?
            .map_err(|e| IsolationError::OciContainerFailed(e.to_string()))?;
        Ok(ExecResult {
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            exit_code: out.status.code().unwrap_or(-1),
        })
    }

    /// Start a long-running command in the background inside the container
    /// (a dev server, a build watch, …). Returns a task id immediately; the
    /// command keeps running and its output is captured. Killed for free when
    /// the container is removed by [`SessionSandbox::stop`].
    pub fn spawn_background(&self, command: &str) -> String {
        let id = format!(
            "task-{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        let status = std::sync::Arc::new(std::sync::Mutex::new("running".to_string()));
        let log = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.push(BgTaskHandle {
                id: id.clone(),
                command: command.to_string(),
                status: status.clone(),
                log: log.clone(),
            });
        }

        let container = self.container.clone();
        let script = format!("{command} 2>&1");
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut child = match Command::new(PODMAN)
                .args(["exec", &container, "sh", "-c", &script])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    if let Ok(mut s) = status.lock() {
                        *s = format!("failed: {e}");
                    }
                    return;
                }
            };
            if let Some(mut out) = child.stdout.take() {
                let mut buf = [0u8; 4096];
                loop {
                    match out.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut l) = log.lock() {
                                l.push_str(&String::from_utf8_lossy(&buf[..n]));
                                // Keep only the tail — long-running tasks log a lot.
                                if l.len() > 64 * 1024 {
                                    let cut = l.len() - 64 * 1024;
                                    l.drain(..cut);
                                }
                            }
                        }
                    }
                }
            }
            let st = child.wait().await;
            if let Ok(mut s) = status.lock() {
                *s = match st {
                    Ok(code) => format!("exited ({})", code.code().unwrap_or(-1)),
                    Err(e) => format!("error: {e}"),
                };
            }
        });
        id
    }

    /// Spawn an interactive PTY-backed terminal inside this session's
    /// container. The returned handle owns the read/write channels; callers
    /// (the WebSocket bridge) subscribe to `output_tx` and push into
    /// `input_tx`. The terminal is tracked here so `list_terminals` /
    /// `get_terminal` can find it later.
    pub fn spawn_pty(
        &self,
        command: &str,
        rows: u16,
        cols: u16,
    ) -> Result<std::sync::Arc<crate::pty::PtyTerminal>, String> {
        let id = format!(
            "term-{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        let term = crate::pty::PtyTerminal::spawn(id, &self.container, command, rows, cols)?;
        let arc = std::sync::Arc::new(term);
        if let Ok(mut t) = self.terminals.lock() {
            t.push(arc.clone());
        }
        Ok(arc)
    }

    /// Find a live terminal by id.
    pub fn get_terminal(&self, id: &str) -> Option<std::sync::Arc<crate::pty::PtyTerminal>> {
        self.terminals
            .lock()
            .ok()?
            .iter()
            .find(|t| t.id == id)
            .cloned()
    }

    /// Drop our reference to a PTY terminal so the underlying child PTY
    /// can be reaped. Any active WebSocket bridge sees its broadcast end
    /// closed; the next `list_terminals()` won't include this id.
    /// Returns `true` if a terminal with this id was present.
    pub fn kill_terminal(&self, id: &str) -> bool {
        let Ok(mut ts) = self.terminals.lock() else {
            return false;
        };
        let before = ts.len();
        ts.retain(|t| t.id != id);
        ts.len() < before
    }

    /// Snapshot of every PTY terminal — id, command, alive flag — for the
    /// session-tasks list. Output isn't included (the WS owns that).
    pub fn list_terminals(&self) -> Vec<(String, String, bool)> {
        self.terminals
            .lock()
            .map(|ts| {
                ts.iter()
                    .map(|t| (t.id.clone(), t.command.clone(), t.is_alive()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Snapshot of this session's background tasks.
    pub fn list_tasks(&self) -> Vec<BgTask> {
        self.tasks
            .lock()
            .map(|tasks| {
                tasks
                    .iter()
                    .map(|h| BgTask {
                        id: h.id.clone(),
                        command: h.command.clone(),
                        status: h.status.lock().map(|s| s.clone()).unwrap_or_default(),
                        log: h.log.lock().map(|l| l.clone()).unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Stop and remove the session container. Best-effort. Removing the
    /// container also kills every background task running inside it.
    pub async fn stop(&self) {
        let _ = Command::new(PODMAN)
            .args(["rm", "-f", &self.container])
            .output()
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_result_ok() {
        let r = ExecResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        };
        assert!(r.ok());
        let r = ExecResult { exit_code: 1, ..r };
        assert!(!r.ok());
    }

    /// End-to-end: needs podman installed. Run with `--ignored`.
    #[tokio::test]
    #[ignore = "requires podman; run with: cargo test -p axocoatl-isolation -- --ignored"]
    async fn sandbox_runs_commands_and_jails_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sb = SessionSandbox::start("test", dir.path(), None, &[], &[])
            .await
            .expect("sandbox should start");

        // A command runs inside the container.
        let r = sb
            .exec(&["echo", "hello-sandbox"], Duration::from_secs(20))
            .await
            .unwrap();
        assert!(r.ok());
        assert!(r.stdout.contains("hello-sandbox"));

        // Writes land in the mounted directory and are visible on the host.
        sb.exec_stdin(
            &["sh", "-c", "cat > \"$1\"", "sh", "probe.txt"],
            "from-inside",
            Duration::from_secs(20),
        )
        .await
        .unwrap();
        let host = std::fs::read_to_string(dir.path().join("probe.txt")).unwrap();
        assert_eq!(host, "from-inside");

        sb.stop().await;
    }
}
