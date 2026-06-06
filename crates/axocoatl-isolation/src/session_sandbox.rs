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

/// Linux capabilities dropped from every session container. These are escape /
/// recon primitives that normal dev workflows (apk/apt/npm/pip, dev servers)
/// never need, so dropping them is safe and meaningfully shrinks the blast
/// radius — especially under rootful podman, where the container would
/// otherwise run with the full default cap set. The package-manager caps
/// (CHOWN, SETUID/SETGID, DAC_OVERRIDE, FOWNER, …) are deliberately kept.
const DROPPED_CAPS: &[&str] = &[
    "SYS_ADMIN",       // mount, namespace ops — the classic escape lever
    "SYS_PTRACE",      // inspect/inject other processes
    "SYS_MODULE",      // load kernel modules
    "SYS_RAWIO",       // raw device I/O
    "SYS_BOOT",        // reboot
    "SYS_TIME",        // set system clock
    "NET_ADMIN",       // reconfigure networking / firewall
    "NET_RAW",         // raw/packet sockets — spoofing, scanning
    "DAC_READ_SEARCH", // bypass file read/traverse permission checks
    "MKNOD",           // create device nodes
    "AUDIT_WRITE",     // write to the kernel audit log
];

/// Per-session container fork-bomb cap. Generous enough for parallel installs
/// and build tools, low enough to bound a runaway. Applied with the other
/// cgroup-backed limits (see `with_limits`).
const PIDS_LIMIT: &str = "512";

/// Network posture for a session container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxNetwork {
    /// Default bridge networking — outbound access for package installs and
    /// reachable published dev-server ports. Required for the normal flow.
    #[default]
    Bridge,
    /// No network at all (`--network none`). Cuts off exfiltration / C2 / SSRF
    /// for untrusted code, at the cost of package installs and dev servers.
    None,
}

/// Trust decisions for a session sandbox. Defaults are secure: project-author
/// setup scripts and non-default images are **not** trusted unless explicitly
/// allowed, so merely opening a hostile repository cannot run code or pull an
/// attacker-chosen image.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    /// Run a repo's `postCreateCommand` (and analogues) automatically. Off by
    /// default — otherwise a malicious repo achieves RCE just by being opened.
    pub allow_post_create: bool,
    /// Honor a repo/UI-specified base image other than [`DEFAULT_IMAGE`]. Off
    /// by default — an attacker-chosen image is attacker-controlled code.
    pub allow_untrusted_image: bool,
    /// Container network posture. [`SandboxNetwork::Bridge`] by default.
    pub network: SandboxNetwork,
    /// Refuse to start if memory/CPU/pid limits can't be applied, instead of
    /// silently continuing uncapped. Off by default because some hosts
    /// (rootless podman on WSL2) genuinely can't delegate cgroups.
    pub require_resource_limits: bool,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            allow_post_create: false,
            allow_untrusted_image: false,
            network: SandboxNetwork::Bridge,
            require_resource_limits: false,
        }
    }
}

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
    /// The session's working directory — bind-mounted at the same path inside
    /// the container, and the confinement root for the structured file tools.
    working_dir: std::path::PathBuf,
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
        policy: &SandboxPolicy,
    ) -> Result<Self, IsolationError> {
        podman::ensure_ready().await?;

        let container = format!("axo-ses-{session_id}");
        let dir = working_dir.to_string_lossy().to_string();

        // Gate non-default base images behind explicit trust. An attacker-chosen
        // image is attacker-controlled code; without consent, fall back to the
        // trusted default rather than pulling and running it.
        let image = match image {
            Some(img) if img != DEFAULT_IMAGE && !policy.allow_untrusted_image => {
                tracing::warn!(
                    "session requested non-default image '{img}', but \
                     sandbox.allow_untrusted_images is off — using the trusted \
                     default ({DEFAULT_IMAGE}). Enable it to opt in."
                );
                DEFAULT_IMAGE
            }
            Some(img) => img,
            None => DEFAULT_IMAGE,
        };

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
            match Self::run_container(
                &container,
                &mount,
                &dir,
                image,
                with_limits,
                &publish,
                policy,
            )
            .await
            {
                Ok(()) => break,
                Err(e) if e.contains("cgroup") && with_limits && policy.require_resource_limits => {
                    // Fail closed: the operator asked for guaranteed caps and we
                    // can't provide them. Surface the error instead of silently
                    // running an uncapped (fork-bomb / OOM-prone) container.
                    return Err(IsolationError::OciContainerFailed(format!(
                        "resource limits required but unavailable on this host \
                         (cgroup delegation missing): {e}. Set \
                         sandbox.require_resource_limits = false to allow an \
                         uncapped sandbox."
                    )));
                }
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
        //
        // SECURITY: these scripts come from the *opened repository*. Running
        // them automatically means a hostile repo gets code execution just by
        // being opened. They run only with explicit consent; otherwise we skip
        // them and tell the user how to opt in.
        if !post_create_commands.is_empty() && !policy.allow_post_create {
            tracing::warn!(
                "skipping {} project setup script(s) (postCreateCommand) for \
                 session container ({container}): these come from the opened \
                 repository and are not run automatically. Set \
                 sandbox.allow_post_create_command = true to enable.",
                post_create_commands.len()
            );
        }
        for script in post_create_commands
            .iter()
            .filter(|_| policy.allow_post_create)
        {
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
            working_dir: working_dir.to_path_buf(),
            tasks: std::sync::Mutex::new(Vec::new()),
            terminals: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// The session's working directory — the confinement root for file tools.
    pub fn root(&self) -> &Path {
        &self.working_dir
    }

    /// The container this sandbox runs in (`axo-ses-{session_id}`).
    pub fn container(&self) -> &str {
        &self.container
    }

    /// Build a handle that **reuses an existing container** but roots the
    /// structured file tools at `working_dir` — a subtree of the original
    /// session mount, e.g. a `git worktree`. Does NOT start or stop a
    /// container (the owning [`SessionSandbox`] controls that lifecycle); this
    /// only re-points the confinement root. Used to run a "variant" agent
    /// jailed to its own worktree inside the shared session container.
    pub fn attach(container: &str, working_dir: &Path) -> Self {
        Self {
            container: container.to_string(),
            working_dir: working_dir.to_path_buf(),
            tasks: std::sync::Mutex::new(Vec::new()),
            terminals: std::sync::Mutex::new(Vec::new()),
        }
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
            // Rootless podman's port-forwarding helper reports a port held by
            // another container's proxy as "proxy already running" — no port
            // number in the message, so `extract_conflicting_port` returns None
            // and the caller drops all publishing and retries (the session
            // still opens, just without dev-server port forwarding).
            || lc.contains("proxy already running")
    }

    /// Remove orphaned session sandbox containers left by a prior run — any
    /// `axo-ses-*` container whose session id is not in `known_ids`. These
    /// accumulate when the daemon exits without cleanly closing sessions (a
    /// crash, a `kill`, or a fresh data dir), and a lingering *running*
    /// container holds its published host ports, blocking new sessions from
    /// starting their port-forwarding proxy ("proxy already running").
    ///
    /// Best-effort and cheap: it does NOT start the podman VM (`ensure_ready`).
    /// If podman is absent or its machine is stopped, the listing fails and
    /// this is a silent no-op (a stopped VM holds no host ports anyway).
    pub async fn reap_orphans(known_ids: &[String]) {
        let out = match Command::new(PODMAN)
            .args([
                "ps",
                "-a",
                "--filter",
                "name=axo-ses-",
                "--format",
                "{{.Names}}",
            ])
            .output()
            .await
        {
            Ok(o) if o.status.success() => o,
            _ => return,
        };
        let names = String::from_utf8_lossy(&out.stdout);
        for name in names.lines().map(str::trim).filter(|n| !n.is_empty()) {
            let Some(sid) = name.strip_prefix("axo-ses-") else {
                continue;
            };
            if known_ids.iter().any(|k| k == sid) {
                // Belongs to a known session — leave it. `start()` reuses or
                // replaces it by name when that session is next opened.
                continue;
            }
            tracing::info!(
                container = name,
                "reaping orphaned session sandbox container (no matching session)"
            );
            let _ = Command::new(PODMAN).args(["rm", "-f", name]).output().await;
        }
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

    /// Build the `podman run` argument vector (pure — no I/O, so it's unit
    /// tested). Carries the always-on hardening (no-new-privileges, capability
    /// drops), the policy-driven network posture, and the optional resource
    /// caps.
    fn build_run_args(
        container: &str,
        mount: &str,
        dir: &str,
        image: &str,
        with_limits: bool,
        ports: &[u16],
        policy: &SandboxPolicy,
    ) -> Vec<String> {
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

        // Always-on hardening — safe for normal dev workflows:
        //   * no-new-privileges: setuid binaries can't escalate beyond the
        //     starting cap set.
        //   * drop escape/recon capabilities the container never needs.
        args.push("--security-opt=no-new-privileges".into());
        for cap in DROPPED_CAPS {
            args.push("--cap-drop".into());
            args.push((*cap).into());
        }

        // Network posture. Bridge is podman's default (no flag needed); `none`
        // cuts off all networking for untrusted code. Publishing ports requires
        // a network, so drop port mapping when networking is off.
        let ports: &[u16] = match policy.network {
            SandboxNetwork::None => {
                args.push("--network".into());
                args.push("none".into());
                &[]
            }
            SandboxNetwork::Bridge => ports,
        };

        if with_limits {
            args.extend([
                "--memory".into(),
                "2g".into(),
                "--cpus".into(),
                "2".into(),
                "--pids-limit".into(),
                PIDS_LIMIT.into(),
            ]);
        }
        for p in ports {
            args.push("-p".into());
            args.push(format!("{p}:{p}"));
        }
        args.push(image.into());
        args.push("sleep".into());
        args.push("infinity".into());
        args
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
        policy: &SandboxPolicy,
    ) -> Result<(), String> {
        let args = Self::build_run_args(container, mount, dir, image, with_limits, ports, policy);

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
    fn port_conflict_detection_covers_proxy_already_running() {
        // The variants podman emits for a held host port — all must route to
        // the graceful "drop ports and retry" path, never a hard failure.
        assert!(SessionSandbox::is_port_conflict(
            "Error: something went wrong with the request: \"proxy already running\\n\""
        ));
        assert!(SessionSandbox::is_port_conflict(
            "rootlessport listen tcp 0.0.0.0:3000: bind: address already in use"
        ));
        assert!(SessionSandbox::is_port_conflict(
            "port is already allocated"
        ));
        // A non-port error must NOT be misread as a port conflict.
        assert!(!SessionSandbox::is_port_conflict("no such image"));
        // "proxy already running" carries no port number → caller drops all.
        assert_eq!(
            SessionSandbox::extract_conflicting_port("proxy already running"),
            None
        );
    }

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

    #[test]
    fn default_policy_is_secure() {
        let p = SandboxPolicy::default();
        assert!(!p.allow_post_create, "post-create must be off by default");
        assert!(
            !p.allow_untrusted_image,
            "untrusted images must be off by default"
        );
        assert_eq!(p.network, SandboxNetwork::Bridge);
        assert!(!p.require_resource_limits);
    }

    #[test]
    fn run_args_always_apply_hardening() {
        let args = SessionSandbox::build_run_args(
            "axo-ses-x",
            "/w:/w:rw",
            "/w",
            DEFAULT_IMAGE,
            true,
            &[3000],
            &SandboxPolicy::default(),
        );
        // no-new-privileges and every dropped capability are present.
        assert!(args.iter().any(|a| a == "--security-opt=no-new-privileges"));
        for cap in DROPPED_CAPS {
            assert!(
                args.windows(2)
                    .any(|w| w[0] == "--cap-drop" && w[1] == *cap),
                "missing --cap-drop {cap}"
            );
        }
        // with_limits adds the fork-bomb / memory / cpu caps.
        assert!(args.windows(2).any(|w| w[0] == "--pids-limit"));
        assert!(args.iter().any(|a| a == "--memory"));
        // Bridge network publishes the port and adds no `--network none`.
        assert!(args.windows(2).any(|w| w[0] == "-p" && w[1] == "3000:3000"));
        assert!(!args
            .windows(2)
            .any(|w| w[0] == "--network" && w[1] == "none"));
    }

    #[test]
    fn run_args_network_none_cuts_off_publishing() {
        let policy = SandboxPolicy {
            network: SandboxNetwork::None,
            ..SandboxPolicy::default()
        };
        let args = SessionSandbox::build_run_args(
            "axo-ses-x",
            "/w:/w:rw",
            "/w",
            DEFAULT_IMAGE,
            false,
            &[3000, 5173],
            &policy,
        );
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--network" && w[1] == "none"));
        // No ports may be published when the network is off.
        assert!(!args.iter().any(|a| a == "-p"));
        // with_limits=false → no caps.
        assert!(!args.iter().any(|a| a == "--pids-limit"));
    }

    /// End-to-end: needs podman installed. Run with `--ignored`.
    #[tokio::test]
    #[ignore = "requires podman; run with: cargo test -p axocoatl-isolation -- --ignored"]
    async fn sandbox_runs_commands_and_jails_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sb = SessionSandbox::start(
            "test",
            dir.path(),
            None,
            &[],
            &[],
            &SandboxPolicy::default(),
        )
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
