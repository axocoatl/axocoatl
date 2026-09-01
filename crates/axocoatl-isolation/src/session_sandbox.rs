//! Per-session isolation instance.
//!
//! Each directory session runs inside its own long-lived **isolation instance**,
//! and every session tool (file ops and shell) runs as a command *inside* it via
//! `exec` — so the instance is the security boundary: tools reach only the
//! session's working tree, not the surrounding environment. The backend is
//! pluggable behind the [`Sandbox`] trait:
//!
//! - **Local (default) — a rootless podman container** ([`SessionSandbox`], this
//!   module): the working directory is bind-mounted at the same path and tools
//!   run under memory/CPU caps. Repository tool execution stays local, subject
//!   to the configured container network policy. Podman is rootless, daemonless,
//!   and cross-platform (native on Linux/WSL, a managed VM on macOS/Windows) —
//!   see [`crate::podman`]. Docker is not used.
//! - **Remote (opt-in) — an E2B Cloud microVM** ([`crate::e2b`]): the repository
//!   is reproduced git-natively inside the selected E2B account. Local-first by
//!   default; the remote backend and template are daemon-global choices, never
//!   per-Session defaults — see `sandbox.backend` in config. Third-party E2B API
//!   implementations are outside Axocoatl 1.0's support claim.
//!
//! Both expose the same repository-tool abstraction, while Preview, network
//! policy, and lifecycle capabilities remain backend-specific. The tools take
//! `Arc<dyn Sandbox>` and never need to know the concrete backend.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use axocoatl_core::SecureDir;

use crate::error::IsolationError;
use crate::podman;

/// The container runtime executable — always podman.
const PODMAN: &str = "podman";

/// A named-container removal is allowed to block for this long before its
/// Podman client is killed. Callers that own a tighter product deadline can
/// pass it to [`SessionSandbox::remove_named_many`].
const NAMED_REMOVE_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// Killing the local Podman client must also reap it before cleanup continues.
const COMMAND_REAP_TIMEOUT: Duration = Duration::from_secs(2);
/// Podman on macOS proxies removal into a VM. The remote removal can finish
/// just after its local client hits a deadline, so reconcile exact names before
/// deciding that cleanup failed or retrying it.
const NAMED_REMOVE_RECONCILE_TIMEOUT: Duration = Duration::from_secs(5);
const NAMED_REMOVE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const NAMED_REMOVE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const SANDBOX_READINESS_TIMEOUT: Duration = Duration::from_secs(30);
const SANDBOX_PROVISION_TIMEOUT: Duration = Duration::from_secs(300);
const SANDBOX_SETUP_COMMAND_TIMEOUT: Duration = Duration::from_secs(900);
const SANDBOX_START_COMMAND_TIMEOUT: Duration = Duration::from_secs(600);
/// Locally-derived, setup-free image used only for passive Ways recovery.
/// The image is provisioned from [`DEFAULT_IMAGE`] before any Workspace is
/// mounted, then every recovery container runs it with networking disabled.
const PASSIVE_RECOVERY_IMAGE: &str = "localhost/axocoatl-recovery-tools:alpine-3.20-v1";
const PASSIVE_RECOVERY_IMAGE_LABEL: &str = "io.axocoatl.recovery-tools";
const PASSIVE_RECOVERY_IMAGE_VERSION: &str = "alpine-3.20-v1";
const SETUP_OUTPUT_MAX_BYTES: usize = 16 * 1024;
/// Maximum retained bytes for each stdout/stderr stream of a foreground
/// command. Readers continue draining after this limit so a chatty child
/// cannot block on a full pipe or grow the daemon heap without bound.
pub(crate) const COMMAND_OUTPUT_MAX_BYTES: usize = 1024 * 1024;
const OUTPUT_TRUNCATION_MARKER_PREFIX: &str = "\n… ";
/// Dynamic host-port allocation should almost never collide, but Podman's
/// rootless proxy can race while it is reconciling a just-removed container.
/// Retry the complete mapping briefly; never make Preview silently disappear.
const DYNAMIC_PORT_START_RETRIES: usize = 3;
/// Durable ownership boundary for local Session containers. The daemon supplies
/// a stable value derived from its canonical data root; orphan cleanup filters
/// on this label before considering a container name.
const RUNTIME_AUTHORITY_LABEL: &str = "io.axocoatl.runtime-authority";

/// Commands Axocoatl itself needs in every local repository sandbox. These are
/// product infrastructure, not a project's language toolchain: Source Control,
/// Ways snapshots, file tools, and cleanup all depend on them being present.
pub const REQUIRED_REPOSITORY_COMMANDS: &[&str] = &[
    "sh", "git", "env", "grep", "tee", "rm", "mkdir", "mv", "cp", "cat", "head", "wc", "find",
    "realpath", "ls", "rmdir", "test",
];

pub(crate) struct BoundedCommandOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
    pub(crate) timed_out: bool,
}

struct BoundedCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

#[derive(Clone, Copy)]
enum SandboxLifecycleDisposition {
    Keep,
    Remove,
}

/// Owns fail-closed cleanup independently from the request/task that started
/// sandbox preparation. If that caller is cancelled, dropping the decision
/// sender wakes the detached supervisor and removes the exact container.
struct SandboxLifecycleSupervisor {
    decision: Option<tokio::sync::oneshot::Sender<SandboxLifecycleDisposition>>,
    task: tokio::task::JoinHandle<Result<(), IsolationError>>,
}

/// Cancellation signal for one passive recovery exec. Dropping the caller's
/// future poisons the shared handle before waking the owned supervisor, so no
/// later request can reuse the container while exact removal is in flight.
struct PassiveExecCancellation {
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    usable: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl PassiveExecCancellation {
    fn disarm(&mut self) {
        self.cancel.take();
    }
}

impl Drop for PassiveExecCancellation {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            self.usable
                .store(false, std::sync::atomic::Ordering::Release);
            let _ = cancel.send(());
        }
    }
}

impl SandboxLifecycleSupervisor {
    fn spawn<F>(cleanup: F) -> Self
    where
        F: std::future::Future<Output = Result<(), IsolationError>> + Send + 'static,
    {
        let (decision, receiver) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            match receiver.await {
                Ok(SandboxLifecycleDisposition::Keep) => Ok(()),
                Ok(SandboxLifecycleDisposition::Remove) | Err(_) => cleanup.await,
            }
        });
        Self {
            decision: Some(decision),
            task,
        }
    }

    async fn finish(
        mut self,
        disposition: SandboxLifecycleDisposition,
    ) -> Result<(), IsolationError> {
        if let Some(decision) = self.decision.take() {
            let _ = decision.send(disposition);
        }
        self.task.await.map_err(|error| {
            IsolationError::OciContainerFailed(format!(
                "sandbox lifecycle supervisor failed: {error}"
            ))
        })?
    }
}

/// Default base image for session containers — small, with a POSIX shell and
/// the busybox coreutils/grep/find the file + shell tools rely on.
pub const DEFAULT_IMAGE: &str = "docker.io/library/alpine:3.20";

/// Axocoatl-curated runtime presets exposed by the Session UI. These exact
/// references are part of the product's trusted surface; arbitrary repository
/// or custom image references still require `allow_untrusted_image`.
pub const CURATED_IMAGES: &[&str] = &[
    DEFAULT_IMAGE,
    "docker.io/library/debian:bookworm-slim",
    "docker.io/library/ubuntu:24.04",
    "docker.io/library/python:3.12-slim",
    "docker.io/library/node:20-slim",
    "docker.io/library/rust:bookworm",
];

fn curated_image_reference(image: &str) -> Option<&'static str> {
    CURATED_IMAGES.iter().copied().find(|canonical| {
        if image == *canonical {
            return true;
        }
        let Some(short) = canonical.strip_prefix("docker.io/library/") else {
            return false;
        };
        image == short
            || image.strip_prefix("library/") == Some(short)
            || image.strip_prefix("docker.io/") == Some(short)
    })
}

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
/// setup scripts and non-curated images are **not** trusted unless explicitly
/// allowed, so merely opening a hostile repository cannot run code or pull an
/// attacker-chosen image.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    /// Run a repo's `postCreateCommand` (and analogues) automatically. Off by
    /// default — otherwise a malicious repo achieves RCE just by being opened.
    pub allow_post_create: bool,
    /// Honor a repo/UI-specified base image outside [`CURATED_IMAGES`]. Off by
    /// default — an attacker-chosen image is attacker-controlled code.
    pub allow_untrusted_image: bool,
    /// Container network posture. [`SandboxNetwork::Bridge`] by default.
    pub network: SandboxNetwork,
    /// Refuse to start if memory/CPU/pid limits can't be applied, instead of
    /// silently continuing uncapped. Off by default because some hosts
    /// (rootless podman on WSL2) genuinely can't delegate cgroups.
    pub require_resource_limits: bool,
    /// Start the container with Axocoatl's inert shell command instead of the
    /// image's configured entrypoint. Recovery-only sandboxes set this so
    /// mounting a stopped Workspace cannot execute image startup code before
    /// the daemon validates and uses its repository metadata.
    pub passive_start: bool,
    /// Stable owner of containers created under this policy. Daemons set this
    /// from their canonical data root so another concurrently-running data
    /// root can never classify these containers as its own orphans. `None`
    /// leaves non-daemon/example containers unmanaged by orphan cleanup.
    pub runtime_authority: Option<String>,
    /// Canonical host directories containing Axocoatl control-plane state.
    /// When one sits below the repository bind mount (including the
    /// v0.1-compatible `./data` default and the daemon's external lease root),
    /// a nested tmpfs hides it from sandboxed code. A Workspace at or below a
    /// control-plane directory is rejected instead.
    pub control_plane_dirs: Vec<std::path::PathBuf>,
    /// Retained capabilities for control-plane directories owned by the
    /// daemon. These are verified immediately before `podman run` and again
    /// after the immutable container id exists, before readiness or repository
    /// setup can execute. `control_plane_dirs` remains the public compatibility
    /// input for examples and callers that do not own daemon state.
    pub control_plane_roots: Vec<SecureDir>,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            allow_post_create: false,
            allow_untrusted_image: false,
            network: SandboxNetwork::Bridge,
            require_resource_limits: false,
            passive_start: false,
            runtime_authority: None,
            control_plane_dirs: Vec::new(),
            control_plane_roots: Vec::new(),
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

/// One explicitly-requested project setup command and its bounded outcome.
///
/// A non-zero exit is represented as data rather than an infrastructure
/// error, allowing the daemon to persist an honest per-Session setup status.
/// Commands run sequentially and stop at the first non-zero exit.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SandboxSetupResult {
    pub command: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl SandboxSetupResult {
    pub fn ok(&self) -> bool {
        self.exit_code == 0
    }
}

impl ExecResult {
    /// True iff the command exited 0.
    pub fn ok(&self) -> bool {
        self.exit_code == 0
    }

    /// Whether stdout was capped by the sandbox execution boundary.
    pub fn stdout_truncated(&self) -> bool {
        output_has_truncation_marker(&self.stdout, "stdout")
    }

    /// Whether stderr was capped by the sandbox execution boundary.
    pub fn stderr_truncated(&self) -> bool {
        output_has_truncation_marker(&self.stderr, "stderr")
    }
}

fn output_has_truncation_marker(value: &str, stream: &str) -> bool {
    value.ends_with(&format!(
        "{OUTPUT_TRUNCATION_MARKER_PREFIX}{stream} truncated after {COMMAND_OUTPUT_MAX_BYTES} bytes …"
    ))
}

pub(crate) fn captured_output_text(bytes: &[u8], truncated: bool, stream: &str) -> String {
    let bytes = if truncated {
        // If a valid UTF-8 stream was clipped in the middle of its final code
        // point, omit only that incomplete suffix. Invalid bytes elsewhere
        // retain the established from_utf8_lossy replacement behavior.
        &bytes[..utf8_prefix_without_incomplete_suffix(bytes)]
    } else {
        bytes
    };
    let mut value = String::from_utf8_lossy(bytes).into_owned();
    if truncated {
        value.push_str(&format!(
            "{OUTPUT_TRUNCATION_MARKER_PREFIX}{stream} truncated after {COMMAND_OUTPUT_MAX_BYTES} bytes …"
        ));
    }
    value
}

fn utf8_prefix_without_incomplete_suffix(bytes: &[u8]) -> usize {
    let mut start = bytes.len();
    while start > 0 && bytes.len() - start < 3 && bytes[start - 1] & 0b1100_0000 == 0b1000_0000 {
        start -= 1;
    }
    if start == 0 {
        return bytes.len();
    }
    let lead = bytes[start - 1];
    let expected = match lead {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return bytes.len(),
    };
    let sequence_start = start - 1;
    if bytes.len() - sequence_start < expected {
        sequence_start
    } else {
        bytes.len()
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
    /// Immutable Podman identity for this exact container incarnation. Cleanup
    /// uses this instead of the reusable name, so a late cancelled task can
    /// never remove a replacement started for the same Session.
    container_id: Option<String>,
    /// The session's working directory — bind-mounted at the same path inside
    /// the container, and the confinement root for the structured file tools.
    working_dir: std::path::PathBuf,
    /// User/config ports remain container-local identities (3000 means the
    /// app's port inside this Session). Podman assigns a unique loopback host
    /// port per Session and the browser proxy resolves through this map.
    published_ports: HashMap<u16, u16>,
    /// The image that actually backs this handle. `None` is reserved for a
    /// non-owning handle attached to an already-running named container.
    effective_image: Option<String>,
    /// Deterministic Podman volume mounted over the repository's root
    /// `node_modules`, when this sandbox was started for a Node project.
    node_dependency_volume: Option<String>,
    /// Recovery containers are deliberately setup-free and execute only
    /// daemon-authored Git plumbing. Their execs are serialized, followed by
    /// an idle-process proof, and fail closed by removing the exact container.
    passive_start: bool,
    passive_execution_usable: std::sync::Arc<std::sync::atomic::AtomicBool>,
    passive_exec_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// Background tasks started in this container.
    tasks: std::sync::Mutex<Vec<BgTaskHandle>>,
    /// Interactive PTY-backed terminals.
    terminals: std::sync::Mutex<Vec<std::sync::Arc<crate::pty::PtyTerminal>>>,
}

impl SessionSandbox {
    fn container_name(session_id: &str) -> String {
        format!("axo-ses-{session_id}")
    }

    fn start_lock(container: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        static START_LOCKS: std::sync::OnceLock<
            std::sync::Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
        > = std::sync::OnceLock::new();
        let locks = START_LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
        let mut locks = locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(container).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(container.to_string(), std::sync::Arc::downgrade(&lock));
        lock
    }

    /// Stable Node dependency-volume identity for one sandbox id.
    ///
    /// Session ids and attempt ids already form deterministic Podman container
    /// names. Keeping the volume derived from that exact identity lets a daemon
    /// restart reuse dependencies, while sibling Sessions and Ways never share
    /// native packages accidentally.
    pub fn dependency_volume_name(sandbox_id: &str) -> String {
        format!("{}-node-modules", Self::container_name(sandbox_id))
    }

    /// Resolve and validate the exact image that a local Session may start.
    ///
    /// This is deliberately pure so callers can reject an untrusted or
    /// malformed image before starting Podman or performing host cleanup. The
    /// start path repeats the same check as a defense-in-depth boundary.
    pub fn resolve_effective_image(
        requested: Option<&str>,
        allow_untrusted_image: bool,
    ) -> Result<String, IsolationError> {
        match requested.map(str::trim).filter(|image| !image.is_empty()) {
            Some(image) => {
                if image.starts_with('-') || image.chars().any(char::is_control) {
                    return Err(IsolationError::OciContainerFailed(format!(
                    "Session image reference {image:?} is invalid: image references cannot begin with '-' or contain control characters"
                    )));
                }
                if let Some(canonical) = curated_image_reference(image) {
                    return Ok(canonical.to_string());
                }
                if !allow_untrusted_image {
                    return Err(IsolationError::OciContainerFailed(format!(
                        "Session image '{image}' requires explicit trust; set \
                     sandbox.allow_untrusted_images = true or choose an Axocoatl-curated \
                     runtime preset. Axocoatl did not silently substitute another image."
                    )));
                }
                Ok(image.to_string())
            }
            None => Ok(DEFAULT_IMAGE.to_string()),
        }
    }

    fn node_dependency_volume(session_id: &str, working_dir: &Path) -> Option<String> {
        working_dir
            .join("package.json")
            .is_file()
            .then(|| Self::dependency_volume_name(session_id))
    }

    fn supervise_container_lifecycle(container_id: String) -> SandboxLifecycleSupervisor {
        SandboxLifecycleSupervisor::spawn(async move {
            Self::remove_exact_container_identity(&container_id, NAMED_REMOVE_COMMAND_TIMEOUT).await
        })
    }

    async fn failed_start_with_name_cleanup(
        container: &str,
        error: IsolationError,
    ) -> IsolationError {
        match Self::remove_exact_container_names(
            &[container.to_string()],
            NAMED_REMOVE_COMMAND_TIMEOUT,
        )
        .await
        {
            Ok(()) => error,
            Err(cleanup_error) => IsolationError::OciContainerFailed(format!(
                "{error}; removing the failed sandbox also failed: {cleanup_error}"
            )),
        }
    }

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
        let working_dir = SecureDir::open(working_dir).map_err(|error| {
            IsolationError::OciSetupFailed(format!(
                "opening retained Session Workspace '{}': {error}",
                working_dir.display()
            ))
        })?;
        Self::start_in(
            session_id,
            &working_dir,
            image,
            exposed_ports,
            post_create_commands,
            policy,
        )
        .await
    }

    /// Start from a Workspace directory capability retained by the caller.
    /// This is the daemon/product entrypoint; callers must open the capability
    /// only after every prior sandbox writer for that Workspace is quiesced.
    pub async fn start_in(
        session_id: &str,
        working_dir: &SecureDir,
        image: Option<&str>,
        exposed_ports: &[u16],
        post_create_commands: &[String],
        policy: &SandboxPolicy,
    ) -> Result<Self, IsolationError> {
        let session_id = session_id.to_string();
        let start_lock = Self::start_lock(&Self::container_name(&session_id));
        let working_dir = working_dir.clone();
        let image = image.map(str::to_string);
        let exposed_ports = exposed_ports.to_vec();
        let post_create_commands = post_create_commands.to_vec();
        let policy = policy.clone();
        let (finished, finished_rx) = tokio::sync::oneshot::channel();
        let start_task = tokio::spawn(async move {
            // This guard lives in the owned task, not the caller. Cancelling a
            // daemon request therefore cannot release same-name singleflight
            // while an earlier Podman create/readiness operation still runs.
            let _start = start_lock.lock_owned().await;
            let result = Self::start_owned(
                &session_id,
                &working_dir,
                image.as_deref(),
                &exposed_ports,
                &post_create_commands,
                &policy,
            )
            .await;
            let container_id = result
                .as_ref()
                .ok()
                .and_then(|sandbox| sandbox.container_id.clone());
            let _ = finished.send(container_id);
            result
        });

        // If this caller disappears while Podman is pulling or creating the
        // container, wait for the owned start task to reach a terminal state
        // and then remove that immutable container id. This closes the otherwise unavoidable
        // race where cleanup observes absence just before `podman run` creates
        // the abandoned container.
        let lifecycle = SandboxLifecycleSupervisor::spawn(async move {
            match finished_rx.await {
                Ok(Some(container_id)) => {
                    Self::remove_exact_container_identity(
                        &container_id,
                        NAMED_REMOVE_COMMAND_TIMEOUT,
                    )
                    .await
                }
                Ok(None) | Err(_) => Ok(()),
            }
        });

        let result = start_task.await.map_err(|error| {
            IsolationError::OciContainerFailed(format!("sandbox start supervisor failed: {error}"))
        });
        match result {
            Ok(Ok(sandbox)) => {
                lifecycle.finish(SandboxLifecycleDisposition::Keep).await?;
                Ok(sandbox)
            }
            Ok(Err(error)) => {
                let cleanup = lifecycle.finish(SandboxLifecycleDisposition::Remove).await;
                Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup_error) => IsolationError::OciContainerFailed(format!(
                        "{error}; removing the failed sandbox also failed: {cleanup_error}"
                    )),
                })
            }
            Err(error) => {
                let cleanup = lifecycle.finish(SandboxLifecycleDisposition::Remove).await;
                Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup_error) => IsolationError::OciContainerFailed(format!(
                        "{error}; removing the failed sandbox also failed: {cleanup_error}"
                    )),
                })
            }
        }
    }

    /// Resolve the nested mounts needed to keep daemon state outside the
    /// repository sandbox. A control-plane directory may be a child of the
    /// Workspace, in which case Podman masks that exact child. A Workspace
    /// equal to or below any control-plane directory cannot be made safe with
    /// a nested mount and is rejected.
    fn control_plane_paths(
        working_dir: &Path,
        control_plane_dirs: &[std::path::PathBuf],
    ) -> Result<(std::path::PathBuf, Vec<std::path::PathBuf>), IsolationError> {
        let working_dir = std::fs::canonicalize(working_dir).map_err(|error| {
            IsolationError::OciSetupFailed(format!(
                "resolving Session Workspace '{}': {error}",
                working_dir.display()
            ))
        })?;
        let mut masks = Vec::new();
        for control_plane_dir in control_plane_dirs {
            let control_plane_dir = std::fs::canonicalize(control_plane_dir).map_err(|error| {
                IsolationError::OciSetupFailed(format!(
                    "resolving Axocoatl control-plane directory '{}': {error}",
                    control_plane_dir.display()
                ))
            })?;

            if working_dir == control_plane_dir || working_dir.starts_with(&control_plane_dir) {
                return Err(IsolationError::OciSetupFailed(format!(
                    "Session Workspace '{}' overlaps Axocoatl control-plane directory '{}'; choose a repository outside that directory",
                    working_dir.display(),
                    control_plane_dir.display()
                )));
            }
            if control_plane_dir.starts_with(&working_dir) {
                masks.push(control_plane_dir);
            }
        }
        masks.sort();
        masks.dedup();
        Ok((working_dir, masks))
    }

    fn verify_start_authorities(
        working_dir: &SecureDir,
        control_plane_roots: &[SecureDir],
    ) -> Result<(), IsolationError> {
        working_dir.verify_ambient_identity().map_err(|error| {
            IsolationError::OciSetupFailed(format!(
                "Session Workspace '{}' no longer resolves to its retained directory: {error}",
                working_dir.path().display()
            ))
        })?;
        for root in control_plane_roots {
            root.verify_ambient_identity().map_err(|error| {
                IsolationError::OciSetupFailed(format!(
                    "Axocoatl control-plane directory '{}' no longer resolves to its retained directory: {error}",
                    root.path().display()
                ))
            })?;
        }
        Ok(())
    }

    async fn start_owned(
        session_id: &str,
        working_dir: &SecureDir,
        image: Option<&str>,
        exposed_ports: &[u16],
        post_create_commands: &[String],
        policy: &SandboxPolicy,
    ) -> Result<Self, IsolationError> {
        // A configured image and the image actually executing repository code
        // must never disagree. Reject an untrusted request with an actionable
        // error instead of persisting one image while silently running Alpine.
        let mut image = Self::resolve_effective_image(image, policy.allow_untrusted_image)?;
        if policy.passive_start && (!exposed_ports.is_empty() || !post_create_commands.is_empty()) {
            return Err(IsolationError::OciSetupFailed(
                "passive recovery sandboxes cannot publish ports or run project setup commands"
                    .to_string(),
            ));
        }
        Self::verify_start_authorities(working_dir, &policy.control_plane_roots)?;
        let mut control_plane_dirs = policy.control_plane_dirs.clone();
        control_plane_dirs.extend(
            policy
                .control_plane_roots
                .iter()
                .map(|root| root.path().to_path_buf()),
        );
        control_plane_dirs.sort();
        control_plane_dirs.dedup();
        let (working_dir_path, control_plane_masks) =
            Self::control_plane_paths(working_dir.path(), &control_plane_dirs)?;
        let mut effective_policy = policy.clone();
        effective_policy.control_plane_dirs = control_plane_masks;
        if policy.passive_start {
            // This is an invariant of the recovery boundary, not an operator
            // preference: no command running with the Workspace mounted can
            // install packages or reach the network.
            effective_policy.network = SandboxNetwork::None;
        }
        podman::ensure_ready().await?;

        if policy.passive_start {
            image = Self::ensure_passive_recovery_image().await?;
        }

        // A macOS/Windows Podman machine may have been stopped during daemon
        // bootstrap. Once it is running, repeat the legacy exposure sweep
        // before any new repository container starts; dormant v0.1 containers
        // could otherwise resume with the old unmasked data-root bind.
        if !control_plane_dirs.is_empty() {
            let runtime_authority = policy.runtime_authority.as_deref().ok_or_else(|| {
                IsolationError::OciSetupFailed(
                    "control-plane masks require a retained local runtime authority".to_string(),
                )
            })?;
            for protected_root in &control_plane_dirs {
                Self::remove_data_root_exposing_containers(protected_root, runtime_authority)
                    .await?;
            }
        }

        let container = Self::container_name(session_id);
        let dir = working_dir_path.to_string_lossy().to_string();

        // Clear a stale exact container under the same-name start lock. This
        // is required, not best-effort: after a dormant machine is started the
        // old container must be proven absent before replacement can execute
        // repository setup.
        Self::remove_exact_container_names(
            std::slice::from_ref(&container),
            NAMED_REMOVE_COMMAND_TIMEOUT,
        )
        .await?;

        // Host dependency directories may contain native artifacts for macOS
        // or Windows. A deterministic nested volume masks those bytes inside
        // Linux without mutating the developer's host install. The volume is
        // intentionally empty until an explicitly-approved setup command runs.
        let node_dependency_volume = (!policy.passive_start)
            .then(|| Self::node_dependency_volume(session_id, &working_dir_path))
            .flatten();

        // Start the long-lived idle container. Resource caps remain a
        // best-effort compatibility toggle. Ports are different: every
        // Session receives its own Podman-assigned loopback host ports, and a
        // startup that cannot preserve those mappings fails honestly instead
        // of opening a container whose Preview is known to be unreachable.
        let mut with_limits = true;
        let mut seen_ports = HashSet::new();
        let publish: Vec<u16> = exposed_ports
            .iter()
            .copied()
            .filter(|port| seen_ports.insert(*port))
            .collect();
        if publish.contains(&0) {
            return Err(IsolationError::OciContainerFailed(
                "container port 0 cannot be exposed".to_string(),
            ));
        }
        let mut dynamic_port_retries = 0_usize;
        let container_id = loop {
            Self::verify_start_authorities(working_dir, &policy.control_plane_roots)?;
            match Self::run_container(
                &container,
                &dir,
                &image,
                node_dependency_volume.as_deref(),
                with_limits,
                &publish,
                &effective_policy,
            )
            .await
            {
                Ok(container_id) => break container_id,
                Err(e) if e.contains("cgroup") && with_limits && policy.require_resource_limits => {
                    // Fail closed: the operator asked for guaranteed caps and we
                    // can't provide them. Surface the error instead of silently
                    // running an uncapped (fork-bomb / OOM-prone) container.
                    let error = IsolationError::OciContainerFailed(format!(
                        "resource limits required but unavailable on this host \
                         (cgroup delegation missing): {e}. Set \
                         sandbox.require_resource_limits = false to allow an \
                         uncapped sandbox."
                    ));
                    return Err(Self::failed_start_with_name_cleanup(&container, error).await);
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
                    if dynamic_port_retries >= DYNAMIC_PORT_START_RETRIES {
                        let error = IsolationError::OciContainerFailed(format!(
                            "Podman could not allocate this Session's dynamic Preview ports after {DYNAMIC_PORT_START_RETRIES} retries: {e}"
                        ));
                        return Err(Self::failed_start_with_name_cleanup(&container, error).await);
                    }
                    dynamic_port_retries += 1;
                    tracing::warn!(
                        retry = dynamic_port_retries,
                        "Podman's dynamic port proxy conflicted while starting; retrying without dropping Preview ports"
                    );
                }
                Err(e) => {
                    let error = IsolationError::OciContainerFailed(e);
                    return Err(Self::failed_start_with_name_cleanup(&container, error).await);
                }
            }
            let _ = Self::remove_exact_container_names(
                std::slice::from_ref(&container),
                NAMED_REMOVE_COMMAND_TIMEOUT,
            )
            .await;
        };

        // From this point onward a live container exists. Keep cleanup owned
        // by an independent task until every readiness/setup step succeeds, so
        // cancellation of `start` cannot leak an environment that is still
        // provisioning or running project code.
        let lifecycle = Self::supervise_container_lifecycle(container_id.clone());

        if let Err(error) = Self::verify_start_authorities(working_dir, &policy.control_plane_roots)
        {
            let cleanup = lifecycle.finish(SandboxLifecycleDisposition::Remove).await;
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup_error) => IsolationError::OciContainerFailed(format!(
                    "{error}; removing the sandbox created from a replaced host path also failed: {cleanup_error}"
                )),
            });
        }

        let expected_mappings: &[u16] = match effective_policy.network {
            SandboxNetwork::Bridge => &publish,
            SandboxNetwork::None => &[],
        };
        let published_ports =
            match Self::discover_published_ports(&container_id, expected_mappings).await {
                Ok(mappings) => mappings,
                Err(error) => {
                    let cleanup = lifecycle.finish(SandboxLifecycleDisposition::Remove).await;
                    let error = IsolationError::OciContainerFailed(error);
                    return Err(match cleanup {
                        Ok(()) => error,
                        Err(cleanup_error) => IsolationError::OciContainerFailed(format!(
                            "{error}; removing the unmapped sandbox also failed: {cleanup_error}"
                        )),
                    });
                }
            };

        // Normal Sessions can provision their chosen runtime after mounting
        // the Workspace. Passive recovery may only *probe*: its derived tool
        // image was provisioned before this mount existed and the container is
        // already network-isolated.
        let readiness = if policy.passive_start {
            Self::ensure_passive_repository_readiness(&container_id, &image).await
        } else {
            Self::ensure_repository_readiness(&container_id, &image).await
        };
        if let Err(error) = readiness {
            let cleanup = lifecycle.finish(SandboxLifecycleDisposition::Remove).await;
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup_error) => IsolationError::OciContainerFailed(format!(
                    "{error}; removing the unready sandbox also failed: {cleanup_error}"
                )),
            });
        }
        if policy.passive_start {
            if let Err(error) = Self::ensure_passive_idle_process(&container_id).await {
                let cleanup = lifecycle.finish(SandboxLifecycleDisposition::Remove).await;
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup_error) => IsolationError::OciContainerFailed(format!(
                        "{error}; removing the non-passive recovery sandbox also failed: {cleanup_error}"
                    )),
                });
            }
        }

        // Honour devcontainer.json's `postCreateCommand` (and any analogue we
        // collect later). These are project-author setup scripts — `npm ci`,
        // `pip install -r requirements.txt`, etc. A setup failure is terminal
        // for this container; the caller must rebuild or change the runtime.
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
        let sandbox = Self {
            container,
            container_id: Some(container_id),
            working_dir: working_dir_path,
            published_ports,
            effective_image: Some(image),
            node_dependency_volume,
            passive_start: policy.passive_start,
            passive_execution_usable: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            passive_exec_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            tasks: std::sync::Mutex::new(Vec::new()),
            terminals: std::sync::Mutex::new(Vec::new()),
        };
        if policy.allow_post_create && !post_create_commands.is_empty() {
            let setup = sandbox.run_setup_commands(post_create_commands).await?;
            if let Some(failed) = setup.iter().find(|result| !result.ok()) {
                let error = IsolationError::OciContainerFailed(format!(
                    "project setup command failed with exit {}: {}{}",
                    failed.exit_code,
                    failed.command,
                    if failed.stderr.trim().is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", failed.stderr.lines().next().unwrap_or_default())
                    }
                ));
                let cleanup = lifecycle.finish(SandboxLifecycleDisposition::Remove).await;
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup_error) => IsolationError::OciContainerFailed(format!(
                        "{error}; removing the failed sandbox also failed: {cleanup_error}"
                    )),
                });
            }
        }

        lifecycle.finish(SandboxLifecycleDisposition::Keep).await?;
        Ok(sandbox)
    }

    /// The session's working directory — the confinement root for file tools.
    pub fn root(&self) -> &Path {
        &self.working_dir
    }

    /// The container this sandbox runs in (`axo-ses-{session_id}`).
    pub fn container(&self) -> &str {
        &self.container
    }

    fn runtime_target(&self) -> &str {
        self.container_id.as_deref().unwrap_or(&self.container)
    }

    /// Host loopback port assigned to one logical container port.
    pub fn published_host_port(&self, container_port: u16) -> Option<u16> {
        self.published_ports.get(&container_port).copied()
    }

    /// OCI image that was actually accepted for this owned sandbox.
    /// Attached recovery handles return `None` because they do not start or
    /// inspect the existing container.
    pub fn effective_image(&self) -> Option<&str> {
        self.effective_image.as_deref()
    }

    /// Whether this handle owns a container with a root `node_modules` volume.
    pub fn uses_node_dependency_volume(&self) -> bool {
        self.node_dependency_volume.is_some()
    }

    fn bounded_setup_output(mut value: String) -> String {
        if value.len() <= SETUP_OUTPUT_MAX_BYTES {
            return value;
        }
        let mut end = SETUP_OUTPUT_MAX_BYTES;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
        value.push_str("\n… setup output truncated …");
        value
    }

    /// Run project setup only after the product has obtained explicit consent.
    ///
    /// This method never infers or injects `npm install`, `npm ci`, or another
    /// project command. It executes exactly the supplied commands, in order,
    /// inside the sandbox and stops after the first non-zero exit. Transport or
    /// timeout failures are returned as [`IsolationError`]; command failures are
    /// returned as [`SandboxSetupResult`] so the caller can persist them.
    pub async fn run_setup_commands(
        &self,
        commands: &[String],
    ) -> Result<Vec<SandboxSetupResult>, IsolationError> {
        if commands.is_empty() {
            return Ok(Vec::new());
        }
        if self.passive_start {
            return Err(IsolationError::OciContainerFailed(
                "project setup is disabled in passive recovery sandboxes".to_string(),
            ));
        }

        let container_id = self.container_id.clone().ok_or_else(|| {
            IsolationError::OciContainerFailed(
                "project setup requires an owned sandbox with a verified container identity"
                    .to_string(),
            )
        })?;
        let lifecycle = Self::supervise_container_lifecycle(container_id.clone());
        let mut results = Vec::with_capacity(commands.len());
        for command in commands {
            tracing::info!(
                container = %self.container,
                command,
                "running explicitly-approved project setup command"
            );
            let mut process = Command::new(PODMAN);
            process
                .arg("exec")
                .arg("-w")
                .arg(&self.working_dir)
                .arg(&container_id)
                .args(["sh", "-c", command]);
            let output = match Self::run_bounded_command(process, SANDBOX_SETUP_COMMAND_TIMEOUT)
                .await
            {
                Ok(output) => output,
                Err(error) => {
                    let cleanup = lifecycle.finish(SandboxLifecycleDisposition::Remove).await;
                    return Err(match cleanup {
                        Ok(()) => error,
                        Err(cleanup_error) => IsolationError::OciContainerFailed(format!(
                            "{error}; removing the failed setup sandbox also failed: {cleanup_error}"
                        )),
                    });
                }
            };
            if output.timed_out {
                let error = IsolationError::Timeout(SANDBOX_SETUP_COMMAND_TIMEOUT);
                let cleanup = lifecycle.finish(SandboxLifecycleDisposition::Remove).await;
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup_error) => IsolationError::OciContainerFailed(format!(
                        "{error}; removing the timed-out setup sandbox also failed: {cleanup_error}"
                    )),
                });
            }
            let outcome = SandboxSetupResult {
                command: command.clone(),
                exit_code: output.status.code().unwrap_or(-1),
                stdout: Self::bounded_setup_output(
                    String::from_utf8_lossy(&output.stdout).into_owned(),
                ),
                stderr: Self::bounded_setup_output(
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                ),
            };
            let ok = outcome.ok();
            results.push(outcome);
            if !ok {
                lifecycle
                    .finish(SandboxLifecycleDisposition::Remove)
                    .await?;
                return Ok(results);
            }
        }
        lifecycle.finish(SandboxLifecycleDisposition::Keep).await?;
        Ok(results)
    }

    /// Build a handle that **reuses an existing container** but roots the
    /// structured file tools at `working_dir`. Does NOT start or stop a
    /// container (the owning [`SessionSandbox`] controls that lifecycle); this
    /// only re-points the confinement root.
    pub fn attach(container: &str, working_dir: &Path) -> Self {
        Self {
            container: container.to_string(),
            container_id: None,
            working_dir: working_dir.to_path_buf(),
            published_ports: HashMap::new(),
            effective_image: None,
            node_dependency_volume: None,
            passive_start: false,
            passive_execution_usable: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            passive_exec_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            tasks: std::sync::Mutex::new(Vec::new()),
            terminals: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Attach to the deterministic container owned by `session_id` without
    /// exposing the container-name encoding to callers.
    pub fn attach_named(session_id: &str, working_dir: &Path) -> Self {
        Self::attach(&Self::container_name(session_id), working_dir)
    }

    /// Whether a deterministically named sandbox exists and is running.
    /// Missing containers are `false`; Podman/runtime failures are errors.
    pub async fn named_running(session_id: &str) -> Result<bool, IsolationError> {
        let container = Self::container_name(session_id);
        let mut exists = Command::new(PODMAN);
        exists.args(["container", "exists", &container]);
        let exists = Self::run_bounded_command(exists, NAMED_REMOVE_PROBE_TIMEOUT).await?;
        if exists.timed_out {
            return Err(IsolationError::Timeout(NAMED_REMOVE_PROBE_TIMEOUT));
        }
        match exists.status.code() {
            Some(0) => {}
            Some(1) => return Ok(false),
            code => {
                return Err(IsolationError::OciContainerFailed(format!(
                    "checking sandbox {container}: podman container exists exited {}",
                    code.map_or_else(|| "without a status".to_string(), |code| code.to_string())
                )));
            }
        }
        let mut inspect = Command::new(PODMAN);
        inspect.args(["inspect", "--format", "{{.State.Running}}", &container]);
        let inspect = Self::run_bounded_command(inspect, NAMED_REMOVE_PROBE_TIMEOUT).await?;
        if inspect.timed_out {
            return Err(IsolationError::Timeout(NAMED_REMOVE_PROBE_TIMEOUT));
        }
        if !inspect.status.success() {
            return Err(IsolationError::OciContainerFailed(format!(
                "inspecting sandbox {container}: {}",
                String::from_utf8_lossy(&inspect.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&inspect.stdout).trim() == "true")
    }

    fn required_commands_probe() -> String {
        let commands = REQUIRED_REPOSITORY_COMMANDS.join(" ");
        format!(
            "for command in {commands}; do command -v \"$command\" >/dev/null 2>&1 || printf '%s\\n' \"$command\"; done"
        )
    }

    fn required_commands_assertion() -> String {
        let commands = REQUIRED_REPOSITORY_COMMANDS.join(" ");
        format!(
            "for command in {commands}; do command -v \"$command\" >/dev/null 2>&1 || {{ printf '%s\\n' \"missing required recovery command: $command\" >&2; exit 127; }}; done"
        )
    }

    fn passive_recovery_image_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    async fn passive_recovery_image_is_valid() -> Result<bool, IsolationError> {
        let mut inspect = Command::new(PODMAN);
        inspect.args([
            "image",
            "inspect",
            "--format",
            &format!("{{{{ index .Labels {PASSIVE_RECOVERY_IMAGE_LABEL:?} }}}}"),
            PASSIVE_RECOVERY_IMAGE,
        ]);
        let inspect = Self::run_bounded_command(inspect, SANDBOX_READINESS_TIMEOUT).await?;
        if inspect.timed_out {
            return Err(IsolationError::OciContainerFailed(
                "inspecting the passive recovery tool image timed out".to_string(),
            ));
        }
        if !inspect.status.success()
            || String::from_utf8_lossy(&inspect.stdout).trim() != PASSIVE_RECOVERY_IMAGE_VERSION
        {
            return Ok(false);
        }

        // Probe in a throwaway, networkless container with no bind mounts.
        // Exact-name cleanup is required even if the Podman client times out.
        let probe_name = format!("axo-recovery-probe-{}", uuid::Uuid::new_v4().simple());
        let mut probe = Command::new(PODMAN);
        probe.args([
            "run",
            "--name",
            &probe_name,
            "--network",
            "none",
            "--entrypoint",
            "/bin/sh",
            PASSIVE_RECOVERY_IMAGE,
            "-c",
            &Self::required_commands_assertion(),
        ]);
        let outcome = Self::run_bounded_command(probe, SANDBOX_READINESS_TIMEOUT).await;
        let cleanup = Self::remove_exact_container_names(
            std::slice::from_ref(&probe_name),
            NAMED_REMOVE_COMMAND_TIMEOUT,
        )
        .await;
        match (outcome, cleanup) {
            (Ok(output), Ok(())) => Ok(!output.timed_out && output.status.success()),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(cleanup_error)) => Err(IsolationError::OciContainerFailed(format!(
                "probing the passive recovery tool image left a container behind: {cleanup_error}"
            ))),
            (Err(error), Err(cleanup_error)) => Err(IsolationError::OciContainerFailed(format!(
                "{error}; cleaning up the recovery image probe also failed: {cleanup_error}"
            ))),
        }
    }

    fn passive_recovery_provision_args(container: &str) -> Vec<String> {
        let mut args = vec!["run".into(), "--name".into(), container.into()];
        args.push("--security-opt=no-new-privileges".into());
        for cap in DROPPED_CAPS {
            args.push("--cap-drop".into());
            args.push((*cap).into());
        }
        args.extend([
            "--entrypoint".into(),
            "/bin/sh".into(),
            DEFAULT_IMAGE.into(),
            "-c".into(),
            "apk add --no-cache coreutils findutils git grep".into(),
        ]);
        args
    }

    async fn provision_passive_recovery_image() -> Result<(), IsolationError> {
        let container = format!("axo-recovery-image-{}", uuid::Uuid::new_v4().simple());
        let outcome = async {
            let mut provision = Command::new(PODMAN);
            provision.args(Self::passive_recovery_provision_args(&container));
            let provision = Self::run_bounded_command(provision, SANDBOX_PROVISION_TIMEOUT).await?;
            if provision.timed_out || !provision.status.success() {
                let detail = String::from_utf8_lossy(&provision.stderr);
                return Err(IsolationError::OciContainerFailed(format!(
                    "preparing the setup-free recovery image failed{}",
                    detail
                        .lines()
                        .find(|line| !line.trim().is_empty())
                        .map_or_else(String::new, |line| format!(": {}", line.trim()))
                )));
            }

            let mut commit = Command::new(PODMAN);
            commit.args([
                "commit",
                "--quiet",
                "--change",
                &format!("LABEL {PASSIVE_RECOVERY_IMAGE_LABEL}={PASSIVE_RECOVERY_IMAGE_VERSION}"),
                &container,
                PASSIVE_RECOVERY_IMAGE,
            ]);
            let commit = Self::run_bounded_command(commit, SANDBOX_PROVISION_TIMEOUT).await?;
            if commit.timed_out || !commit.status.success() {
                return Err(IsolationError::OciContainerFailed(format!(
                    "committing the setup-free recovery image failed: {}",
                    String::from_utf8_lossy(&commit.stderr).trim()
                )));
            }
            Ok(())
        }
        .await;
        let cleanup = Self::remove_exact_container_names(
            std::slice::from_ref(&container),
            NAMED_REMOVE_COMMAND_TIMEOUT,
        )
        .await;
        match (outcome, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Err(cleanup_error)) => Err(IsolationError::OciContainerFailed(format!(
                "the recovery tool image was prepared, but its setup container could not be removed: {cleanup_error}"
            ))),
            (Err(error), Err(cleanup_error)) => Err(IsolationError::OciContainerFailed(format!(
                "{error}; cleaning up its setup container also failed: {cleanup_error}"
            ))),
        }
    }

    async fn ensure_passive_recovery_image() -> Result<String, IsolationError> {
        let _image = Self::passive_recovery_image_lock().lock().await;
        if Self::passive_recovery_image_is_valid().await? {
            return Ok(PASSIVE_RECOVERY_IMAGE.to_string());
        }
        // Provision only a throwaway container created before `run_container`
        // binds the Workspace. The resulting local image is then independently
        // probed with no network and no mounts.
        Self::provision_passive_recovery_image().await?;
        if !Self::passive_recovery_image_is_valid().await? {
            return Err(IsolationError::OciContainerFailed(
                "the prepared passive recovery image failed its offline command probe".to_string(),
            ));
        }
        Ok(PASSIVE_RECOVERY_IMAGE.to_string())
    }

    fn repository_provision_script() -> &'static str {
        // The Alpine branch preserves the useful language tools the trusted
        // default historically installed. Other images receive only Axocoatl's
        // required repository utilities; their language runtime remains the
        // image author's responsibility.
        "if command -v apk >/dev/null 2>&1; then \
             apk add --no-cache bash coreutils curl findutils git grep less nano nodejs npm \
                 python3 py3-pip vim wget; \
         elif command -v apt-get >/dev/null 2>&1; then \
             export DEBIAN_FRONTEND=noninteractive; \
             apt-get update -qq && apt-get install -y --no-install-recommends \
                 ca-certificates coreutils findutils git grep; \
         elif command -v microdnf >/dev/null 2>&1; then \
             microdnf install -y coreutils findutils git grep; \
         elif command -v dnf >/dev/null 2>&1; then \
             dnf install -y coreutils findutils git grep; \
         elif command -v yum >/dev/null 2>&1; then \
             yum install -y coreutils findutils git grep; \
         elif command -v zypper >/dev/null 2>&1; then \
             zypper --non-interactive install coreutils findutils git grep; \
         else \
             printf '%s\\n' 'no supported package manager (apk, apt-get, microdnf, dnf, yum, or zypper)' >&2; \
             exit 127; \
         fi"
    }

    async fn podman_exec_shell(
        container: &str,
        script: &str,
        timeout: Duration,
    ) -> Result<BoundedCommandOutput, IsolationError> {
        let mut command = Command::new(PODMAN);
        command.args(["exec", container, "sh", "-c", script]);
        Self::run_bounded_command(command, timeout).await
    }

    async fn podman_exec_shell_as_root(
        container: &str,
        script: &str,
        timeout: Duration,
    ) -> Result<BoundedCommandOutput, IsolationError> {
        let mut command = Command::new(PODMAN);
        command.args(["exec", "--user", "0", container, "sh", "-c", script]);
        Self::run_bounded_command(command, timeout).await
    }

    async fn missing_repository_commands(container: &str) -> Result<Vec<String>, IsolationError> {
        let output = Self::podman_exec_shell(
            container,
            &Self::required_commands_probe(),
            SANDBOX_READINESS_TIMEOUT,
        )
        .await?;
        if output.timed_out {
            return Err(IsolationError::OciContainerFailed(format!(
                "probing required repository commands timed out after {} seconds",
                SANDBOX_READINESS_TIMEOUT.as_secs()
            )));
        }
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            return Err(IsolationError::OciContainerFailed(format!(
                "the image could not run Axocoatl's POSIX shell readiness probe{}",
                if detail.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", detail.lines().next().unwrap_or_default())
                }
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_string)
            .collect())
    }

    async fn ensure_repository_readiness(
        container: &str,
        image: &str,
    ) -> Result<(), IsolationError> {
        let missing = Self::missing_repository_commands(container).await?;
        if missing.is_empty() {
            return Ok(());
        }

        tracing::info!(
            container,
            image,
            missing = %missing.join(", "),
            "provisioning required Axocoatl repository commands"
        );
        let provision = Self::podman_exec_shell_as_root(
            container,
            Self::repository_provision_script(),
            SANDBOX_PROVISION_TIMEOUT,
        )
        .await?;
        if provision.timed_out {
            return Err(IsolationError::OciContainerFailed(format!(
                "Session image '{image}' did not become ready: distro-aware provisioning timed out after {} seconds; the sandbox will be removed.",
                SANDBOX_PROVISION_TIMEOUT.as_secs()
            )));
        }
        let still_missing = Self::missing_repository_commands(container).await?;
        if still_missing.is_empty() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&provision.stderr);
        let stdout = String::from_utf8_lossy(&provision.stdout);
        let provision_detail = stderr
            .lines()
            .chain(stdout.lines())
            .find(|line| !line.trim().is_empty())
            .map(|line| line.trim().to_string())
            .unwrap_or_else(|| format!("provisioner exited with {}", provision.status));
        Err(IsolationError::OciContainerFailed(format!(
            "Session image '{image}' is not ready for Axocoatl repository work; \
             missing required commands after distro-aware provisioning: {}. \
             Use an image that provides these commands or make its package manager \
             available ({provision_detail}).",
            still_missing.join(", ")
        )))
    }

    async fn ensure_passive_repository_readiness(
        container: &str,
        image: &str,
    ) -> Result<(), IsolationError> {
        let missing = Self::missing_repository_commands(container).await?;
        if missing.is_empty() {
            return Ok(());
        }
        Err(IsolationError::OciContainerFailed(format!(
            "passive recovery image '{image}' is missing required commands after its offline probe: {}; Axocoatl will not install packages while the Workspace is mounted",
            missing.join(", ")
        )))
    }

    fn is_port_conflict(stderr: &str) -> bool {
        let lc = stderr.to_lowercase();
        lc.contains("port is already allocated")
            || lc.contains("address already in use")
            || lc.contains("bind: address")
            || lc.contains("rootlessport")
            // Rootless Podman's helper can race a recently removed proxy. The
            // caller retries dynamic allocation; it never drops Preview ports.
            || lc.contains("proxy already running")
    }

    fn orphan_list_args(runtime_authority: &str) -> Vec<String> {
        vec![
            "ps".into(),
            "-a".into(),
            "--filter".into(),
            "name=axo-ses-".into(),
            "--filter".into(),
            format!("label={RUNTIME_AUTHORITY_LABEL}={runtime_authority}"),
            "--format".into(),
            "{{.Names}}".into(),
        ]
    }

    fn owned_container_list_args(runtime_authority: &str) -> Vec<String> {
        vec![
            "ps".into(),
            "-a".into(),
            "--no-trunc".into(),
            "--filter".into(),
            "name=axo-ses-".into(),
            "--filter".into(),
            format!("label={RUNTIME_AUTHORITY_LABEL}={runtime_authority}"),
            "--format".into(),
            "{{.ID}}\t{{.Names}}".into(),
        ]
    }

    fn parse_owned_container_candidates(
        stdout: &[u8],
    ) -> Result<Vec<(String, String)>, IsolationError> {
        let stdout = std::str::from_utf8(stdout).map_err(|error| {
            IsolationError::OciContainerFailed(format!(
                "listing owned Session containers returned non-UTF-8 output: {error}"
            ))
        })?;
        let mut candidates = Vec::new();
        for line in stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            let (id, name) = line.split_once('\t').ok_or_else(|| {
                IsolationError::OciContainerFailed(format!(
                    "listing owned Session containers returned an invalid row: {line:?}"
                ))
            })?;
            let name = name.trim().trim_start_matches('/');
            if !name.starts_with("axo-ses-") {
                return Err(IsolationError::OciContainerFailed(format!(
                    "Podman returned non-Session container {name:?} for the owned-Session filter"
                )));
            }
            if !(12..=64).contains(&id.len()) || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(IsolationError::OciContainerFailed(format!(
                    "Podman returned an invalid immutable identity for Session container {name}: {id:?}"
                )));
            }
            if !candidates.iter().any(|(candidate, _)| candidate == id) {
                candidates.push((id.to_string(), name.to_string()));
            }
        }
        Ok(candidates)
    }

    fn data_root_exposure_list_args() -> Vec<String> {
        vec![
            "ps".into(),
            "-a".into(),
            "--no-trunc".into(),
            "--filter".into(),
            "name=axo-ses-".into(),
            "--format".into(),
            "{{.ID}}\t{{.Names}}".into(),
        ]
    }

    fn parse_data_root_exposure_candidates(stdout: &[u8]) -> Result<Vec<String>, IsolationError> {
        let stdout = std::str::from_utf8(stdout).map_err(|error| {
            IsolationError::OciContainerFailed(format!(
                "listing legacy Axocoatl containers returned non-UTF-8 output: {error}"
            ))
        })?;
        let mut ids = Vec::new();
        for line in stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            let (id, name) = line.split_once('\t').ok_or_else(|| {
                IsolationError::OciContainerFailed(format!(
                    "listing legacy Axocoatl containers returned an invalid row: {line:?}"
                ))
            })?;
            let name = name.trim().trim_start_matches('/');
            if !name.starts_with("axo-ses-") {
                continue;
            }
            if !(12..=64).contains(&id.len()) || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(IsolationError::OciContainerFailed(format!(
                    "Podman returned an invalid container identity for {name}: {id:?}"
                )));
            }
            if !ids.iter().any(|candidate| candidate == id) {
                ids.push(id.to_string());
            }
        }
        Ok(ids)
    }

    fn mount_source_overlaps_data_root(source: &str, data_root: &Path) -> bool {
        let source = Path::new(source);
        source.is_absolute() && (data_root.starts_with(source) || source.starts_with(data_root))
    }

    fn data_root_exposing_ids(
        listed_ids: &[String],
        data_root: &Path,
        trusted_runtime_authority: &str,
        inspect_json: &[u8],
    ) -> Result<Vec<String>, IsolationError> {
        let records: Vec<serde_json::Value> =
            serde_json::from_slice(inspect_json).map_err(|error| {
                IsolationError::OciContainerFailed(format!(
                    "inspecting legacy Axocoatl containers returned invalid JSON: {error}"
                ))
            })?;
        let mut observed = HashSet::new();
        let mut exposing = Vec::new();
        for record in records {
            let id = record
                .get("Id")
                .or_else(|| record.get("ID"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    IsolationError::OciContainerFailed(
                        "Podman inspect omitted a container identity".to_string(),
                    )
                })?;
            if !listed_ids.iter().any(|listed| listed == id) {
                return Err(IsolationError::OciContainerFailed(format!(
                    "Podman inspect returned unrequested container identity {id}"
                )));
            }
            observed.insert(id.to_string());
            let is_current_runtime = record
                .get("Config")
                .and_then(|config| config.get("Labels"))
                .and_then(|labels| labels.get(RUNTIME_AUTHORITY_LABEL))
                .and_then(serde_json::Value::as_str)
                == Some(trusted_runtime_authority);
            let mounts = record
                .get("Mounts")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    IsolationError::OciContainerFailed(format!(
                        "Podman inspect omitted mounts for container {id}"
                    ))
                })?;
            let exposes_data = mounts.iter().any(|mount| {
                mount.get("Type").and_then(serde_json::Value::as_str) == Some("bind")
                    && mount
                        .get("Source")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|source| {
                            Self::mount_source_overlaps_data_root(source, data_root)
                        })
            });
            if exposes_data && !is_current_runtime {
                exposing.push(id.to_string());
            }
        }
        if let Some(missing) = listed_ids.iter().find(|id| !observed.contains(*id)) {
            return Err(IsolationError::OciContainerFailed(format!(
                "Podman inspect omitted listed container identity {missing}"
            )));
        }
        exposing.sort();
        exposing.dedup();
        Ok(exposing)
    }

    /// Remove any Axocoatl-named local container whose inspected host bind
    /// overlaps the canonical control-plane data root.
    ///
    /// This is a fail-closed v0.1 upgrade preflight. Legacy containers had no
    /// runtime-authority label, and repository code could have corrupted or
    /// deleted the Session record that named them. Selection therefore uses
    /// both the reserved `axo-ses-*` name and Podman's immutable inspect data.
    /// Containers carrying this daemon root's exact 1.0 runtime-authority label
    /// are already protected by nested control-plane masks and are retained;
    /// other or unlabeled containers with an exposing bind are removed by
    /// immutable ID while dependency volumes are preserved.
    pub async fn remove_data_root_exposing_containers(
        data_root: &Path,
        trusted_runtime_authority: &str,
    ) -> Result<Vec<String>, IsolationError> {
        // The caller supplies the absolute, canonical spelling retained by an
        // already-open SecureDir. Do not resolve it here: a surviving v0.1
        // container may have renamed or replaced the ambient path, and cleanup
        // must still select that container before the caller verifies the
        // retained directory identity and fails closed.
        if !data_root.is_absolute()
            || data_root.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )
            })
        {
            return Err(IsolationError::OciSetupFailed(format!(
                "Axocoatl data directory '{}' is not an absolute normalized path",
                data_root.display()
            )));
        }
        if trusted_runtime_authority.len() != 64
            || !trusted_runtime_authority
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(IsolationError::OciSetupFailed(
                "Axocoatl runtime authority is not a full SHA-256 identity".to_string(),
            ));
        }
        match podman::detect().await {
            podman::PodmanReadiness::Ready => {}
            podman::PodmanReadiness::NotInstalled
            | podman::PodmanReadiness::MachineMissing
            | podman::PodmanReadiness::MachineStopped => {
                // A missing or stopped Podman VM cannot have a running process
                // mutating the host-shared Workspace. Axocoatl containers are
                // created without a restart policy, so a dormant legacy
                // container remains stopped if an existing VM is later started;
                // the exact-name start path removes it before any reuse.
                return Ok(Vec::new());
            }
            readiness => {
                return Err(IsolationError::OciSetupFailed(format!(
                    "cannot prove that no legacy Session container exposes Axocoatl's data directory: {}",
                    readiness.summary()
                )));
            }
        }

        let mut list = Command::new(PODMAN);
        list.args(Self::data_root_exposure_list_args());
        let listed = Self::run_bounded_command(list, NAMED_REMOVE_COMMAND_TIMEOUT).await?;
        if listed.timed_out {
            return Err(IsolationError::Timeout(NAMED_REMOVE_COMMAND_TIMEOUT));
        }
        if !listed.status.success() {
            return Err(IsolationError::OciContainerFailed(format!(
                "listing legacy Axocoatl containers: {}",
                String::from_utf8_lossy(&listed.stderr).trim()
            )));
        }
        let listed_ids = Self::parse_data_root_exposure_candidates(&listed.stdout)?;
        if listed_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut inspect = Command::new(PODMAN);
        inspect.args(["inspect", "--type", "container", "--format", "json"]);
        inspect.args(&listed_ids);
        let inspected = Self::run_bounded_command(inspect, NAMED_REMOVE_COMMAND_TIMEOUT).await?;
        if inspected.timed_out {
            return Err(IsolationError::Timeout(NAMED_REMOVE_COMMAND_TIMEOUT));
        }
        if !inspected.status.success() {
            return Err(IsolationError::OciContainerFailed(format!(
                "inspecting legacy Axocoatl containers: {}",
                String::from_utf8_lossy(&inspected.stderr).trim()
            )));
        }
        let exposing = Self::data_root_exposing_ids(
            &listed_ids,
            data_root,
            trusted_runtime_authority,
            &inspected.stdout,
        )?;
        for container_id in &exposing {
            Self::remove_exact_container_identity(container_id, NAMED_REMOVE_COMMAND_TIMEOUT)
                .await?;
        }
        Ok(exposing)
    }

    /// Remove every Session or Way container carrying this exact daemon's
    /// durable local runtime authority. No local runtime survives daemon
    /// restart as execution authority; Session records and attempt ledgers are
    /// the durable recovery state. Selection uses immutable Podman ids so a
    /// same-name replacement cannot be removed between discovery and cleanup.
    pub async fn remove_owned_containers(
        runtime_authority: &str,
    ) -> Result<Vec<String>, IsolationError> {
        if runtime_authority.len() != 64
            || !runtime_authority
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(IsolationError::OciSetupFailed(
                "Axocoatl runtime authority is not a full SHA-256 identity".to_string(),
            ));
        }
        match podman::detect().await {
            podman::PodmanReadiness::Ready => {}
            podman::PodmanReadiness::NotInstalled
            | podman::PodmanReadiness::MachineMissing
            | podman::PodmanReadiness::MachineStopped => return Ok(Vec::new()),
            readiness => {
                return Err(IsolationError::OciSetupFailed(format!(
                    "cannot prove owned Session containers absent: {}",
                    readiness.summary()
                )));
            }
        }

        let mut list = Command::new(PODMAN);
        list.args(Self::owned_container_list_args(runtime_authority));
        let listed = Self::run_bounded_command(list, NAMED_REMOVE_COMMAND_TIMEOUT).await?;
        if listed.timed_out {
            return Err(IsolationError::Timeout(NAMED_REMOVE_COMMAND_TIMEOUT));
        }
        if !listed.status.success() {
            return Err(IsolationError::OciContainerFailed(format!(
                "listing owned Session containers: {}",
                String::from_utf8_lossy(&listed.stderr).trim()
            )));
        }
        let candidates = Self::parse_owned_container_candidates(&listed.stdout)?;
        for (container_id, _) in &candidates {
            Self::remove_exact_container_identity(container_id, NAMED_REMOVE_COMMAND_TIMEOUT)
                .await?;
        }
        Ok(candidates.into_iter().map(|(_, name)| name).collect())
    }

    /// Remove orphaned session sandbox containers left by a prior run — any
    /// owned `axo-ses-*` container whose session id is not in `known_ids`.
    /// Ownership is the daemon's durable data-root authority label; containers
    /// belonging to another data root, and legacy/unmanaged containers without
    /// a label, are never included in the cleanup set. Owned containers can
    /// accumulate when the daemon exits without cleanly closing sessions (a
    /// crash or a `kill`), and a lingering *running* container holds its
    /// published host ports, blocking new sessions from starting their
    /// port-forwarding proxy ("proxy already running").
    ///
    /// Best-effort and cheap: it does NOT start the podman VM (`ensure_ready`).
    /// If podman is absent or its machine is stopped, the listing fails and
    /// this is a silent no-op (a stopped VM holds no host ports anyway).
    pub async fn reap_orphans(runtime_authority: &str, known_ids: &[String]) {
        if runtime_authority.is_empty() {
            tracing::warn!("skipping orphan cleanup without a runtime ownership authority");
            return;
        }
        let mut list = Command::new(PODMAN);
        list.args(Self::orphan_list_args(runtime_authority));
        let out = match Self::run_bounded_command(list, NAMED_REMOVE_COMMAND_TIMEOUT).await {
            Ok(output) if output.status.success() && !output.timed_out => output,
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
            if let Err(error) = Self::remove_named_with_dependencies(sid).await {
                tracing::warn!(container = name, error = %error, "orphan cleanup was incomplete");
            }
        }
    }

    /// Parse `podman port <container>` output, for example:
    /// `3000/tcp -> 127.0.0.1:43117`.
    fn parse_published_ports(stdout: &str) -> HashMap<u16, u16> {
        stdout
            .lines()
            .filter_map(|line| {
                let (container, host) = line.trim().split_once(" -> ")?;
                let container_port = container.split('/').next()?.parse::<u16>().ok()?;
                let host_port = host.rsplit(':').next()?.parse::<u16>().ok()?;
                (container_port > 0 && host_port > 0).then_some((container_port, host_port))
            })
            .collect()
    }

    /// Read back Podman's authoritative dynamic allocations. A successful
    /// container start without every requested mapping is not a successful
    /// Session start because Preview would route to the wrong process.
    async fn discover_published_ports(
        container: &str,
        expected: &[u16],
    ) -> Result<HashMap<u16, u16>, String> {
        if expected.is_empty() {
            return Ok(HashMap::new());
        }
        let mut last_detail = String::new();
        for _ in 0..10 {
            let mut command = Command::new(PODMAN);
            command.args(["port", container]);
            match Self::run_bounded_command(command, NAMED_REMOVE_PROBE_TIMEOUT).await {
                Ok(output) if !output.timed_out && output.status.success() => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let mappings = Self::parse_published_ports(&stdout);
                    if expected.iter().all(|port| mappings.contains_key(port)) {
                        return Ok(mappings);
                    }
                    last_detail = format!("reported mappings: {}", stdout.trim());
                }
                Ok(output) => {
                    last_detail = if output.timed_out {
                        "port discovery timed out".to_string()
                    } else {
                        String::from_utf8_lossy(&output.stderr).trim().to_string()
                    };
                }
                Err(error) => last_detail = error.to_string(),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(format!(
            "Podman started sandbox {container} but did not report every dynamic Preview mapping ({last_detail})"
        ))
    }

    /// Build the `podman run` argument vector (pure — no I/O, so it's unit
    /// tested). Carries the always-on hardening (no-new-privileges, capability
    /// drops), the policy-driven network posture, and the optional resource
    /// caps.
    fn build_run_args(
        container: &str,
        dir: &str,
        image: &str,
        node_dependency_volume: Option<&str>,
        with_limits: bool,
        ports: &[u16],
        policy: &SandboxPolicy,
    ) -> Vec<String> {
        let mount = format!("{dir}:{dir}:rw");
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            container.into(),
            "-v".into(),
            mount,
            "-w".into(),
            dir.into(),
        ];
        for mask in &policy.control_plane_dirs {
            args.push("--tmpfs".into());
            // Podman enables `tmpcopyup` by default. Without the explicit
            // inverse, files already present below the bind-mounted Workspace
            // are copied into this tmpfs and remain visible to the container,
            // defeating the control-plane mask.
            args.push(format!(
                "{}:rw,noexec,nosuid,nodev,notmpcopyup",
                mask.to_string_lossy()
            ));
        }
        if let Some(runtime_authority) = policy.runtime_authority.as_deref() {
            args.push("--label".into());
            args.push(format!("{RUNTIME_AUTHORITY_LABEL}={runtime_authority}"));
        }
        if let Some(volume) = node_dependency_volume {
            args.push("--mount".into());
            // Podman named volumes do not copy destination content unless its
            // explicit `copy` mount option is present. Omit that option so the
            // empty nested volume masks host `node_modules` instead of importing
            // native packages from the bind-mounted checkout.
            args.push(format!(
                "type=volume,source={volume},destination={dir}/node_modules"
            ));
        }

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
        let network = if policy.passive_start {
            SandboxNetwork::None
        } else {
            policy.network
        };
        let ports: &[u16] = match network {
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
            // Keep configured URLs container-local and stable while Podman
            // assigns an independent host port for every Session. Loopback
            // binding also avoids exposing dev servers on the LAN.
            args.push(format!("127.0.0.1::{p}"));
        }
        if policy.passive_start {
            args.push("--entrypoint".into());
            args.push("/bin/sh".into());
        }
        args.push(image.into());
        if policy.passive_start {
            args.push("-c".into());
            args.push("exec sleep infinity".into());
        } else {
            args.push("sleep".into());
            args.push("infinity".into());
        }
        args
    }

    /// `podman run -d` the idle session container. `with_limits` adds
    /// memory/CPU caps. `ports` receive dynamic loopback host mappings. On
    /// failure returns Podman's stderr so the caller can decide how to recover.
    async fn run_container(
        container: &str,
        dir: &str,
        image: &str,
        node_dependency_volume: Option<&str>,
        with_limits: bool,
        ports: &[u16],
        policy: &SandboxPolicy,
    ) -> Result<String, String> {
        let args = Self::build_run_args(
            container,
            dir,
            image,
            node_dependency_volume,
            with_limits,
            ports,
            policy,
        );

        let mut command = Command::new(PODMAN);
        command.args(&args);
        let out = Self::run_bounded_command(command, SANDBOX_START_COMMAND_TIMEOUT)
            .await
            .map_err(|error| format!("spawning Podman: {error}"))?;
        if out.timed_out {
            return Err(format!(
                "starting session container timed out after {} seconds",
                SANDBOX_START_COMMAND_TIMEOUT.as_secs()
            ));
        }
        if out.status.success() {
            let container_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if container_id.is_empty() {
                Err(
                    "starting session container: Podman reported success without a container id"
                        .to_string(),
                )
            } else {
                Ok(container_id)
            }
        } else {
            Err(format!(
                "starting session container: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    async fn execute_podman_exec_owned(
        container: String,
        working_dir: std::path::PathBuf,
        argv: Vec<String>,
        stdin: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<ExecResult, IsolationError> {
        let mut command = Command::new(PODMAN);
        // `-w` per exec, not just at container creation: a handle produced by
        // `attach`/`with_root` (a variant lane) shares the container but works in
        // its own worktree. Without this every lane would run in the container's
        // default directory — the session root — and a relative path would land
        // in the shared checkout instead of that lane's tree.
        command.arg("exec");
        if stdin.is_some() {
            command.arg("-i");
        }
        command.arg("-w").arg(working_dir).arg(container).args(argv);
        let out = Self::run_bounded_command_with_input_owned(command, stdin, timeout).await?;
        if out.timed_out {
            return Err(IsolationError::Timeout(timeout));
        }
        Ok(ExecResult {
            stdout: captured_output_text(&out.stdout, out.stdout_truncated, "stdout"),
            stderr: captured_output_text(&out.stderr, out.stderr_truncated, "stderr"),
            exit_code: out.status.code().unwrap_or(-1),
        })
    }

    async fn passive_exec(
        &self,
        argv: &[&str],
        stdin: Option<&str>,
        timeout: Duration,
    ) -> Result<ExecResult, IsolationError> {
        if !self
            .passive_execution_usable
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(IsolationError::OciContainerFailed(
                "the passive recovery sandbox was invalidated by an interrupted command"
                    .to_string(),
            ));
        }
        let container_id = self.container_id.clone().ok_or_else(|| {
            IsolationError::OciContainerFailed(
                "passive recovery requires an immutable container identity".to_string(),
            )
        })?;
        let working_dir = self.working_dir.clone();
        let argv = argv.iter().map(|value| (*value).to_string()).collect();
        let stdin = stdin.map(|value| value.as_bytes().to_vec());
        let lock = self.passive_exec_lock.clone();
        let usable = self.passive_execution_usable.clone();
        let usable_for_task = usable.clone();
        let cleanup_id = container_id.clone();
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let operation = tokio::select! {
                biased;
                _ = cancelled => Err(IsolationError::OciContainerFailed(
                    "passive recovery command was cancelled".to_string()
                )),
                result = async {
                    let _exec = lock.lock_owned().await;
                    if !usable_for_task.load(std::sync::atomic::Ordering::Acquire) {
                        return Err(IsolationError::OciContainerFailed(
                            "passive recovery command was cancelled before execution".to_string()
                        ));
                    }
                    let result = Self::execute_podman_exec_owned(
                        container_id.clone(),
                        working_dir,
                        argv,
                        stdin,
                        timeout,
                    ).await?;
                    // Even a non-zero Git exit must prove it did not leave a
                    // daemonized helper behind before topology is revalidated.
                    Self::ensure_passive_idle_process(&container_id).await?;
                    Ok(result)
                } => result,
            };
            match operation {
                Ok(result) => Ok(result),
                Err(error) => {
                    usable_for_task.store(false, std::sync::atomic::Ordering::Release);
                    let cleanup = Self::remove_exact_container_identity(
                        &cleanup_id,
                        NAMED_REMOVE_COMMAND_TIMEOUT,
                    )
                    .await;
                    Err(match cleanup {
                        Ok(()) => error,
                        Err(cleanup_error) => IsolationError::OciContainerFailed(format!(
                            "{error}; removing the interrupted recovery container also failed: {cleanup_error}"
                        )),
                    })
                }
            }
        });
        let mut cancellation = PassiveExecCancellation {
            cancel: Some(cancel),
            usable: usable.clone(),
        };
        let joined = task.await;
        cancellation.disarm();
        match joined {
            Ok(result) => result,
            Err(error) => {
                usable.store(false, std::sync::atomic::Ordering::Release);
                let cleanup = Self::remove_exact_container_identity(
                    self.runtime_target(),
                    NAMED_REMOVE_COMMAND_TIMEOUT,
                )
                .await;
                Err(match cleanup {
                    Ok(()) => IsolationError::OciContainerFailed(format!(
                        "passive recovery exec supervisor failed: {error}"
                    )),
                    Err(cleanup_error) => IsolationError::OciContainerFailed(format!(
                        "passive recovery exec supervisor failed: {error}; exact cleanup also failed: {cleanup_error}"
                    )),
                })
            }
        }
    }

    /// Run a command inside the session container.
    pub async fn exec(
        &self,
        argv: &[&str],
        timeout: Duration,
    ) -> Result<ExecResult, IsolationError> {
        if self.passive_start {
            return self.passive_exec(argv, None, timeout).await;
        }
        let container = self.runtime_target().to_string();
        let working_dir = self.working_dir.clone();
        let argv = argv.iter().map(|value| (*value).to_string()).collect();
        tokio::spawn(async move {
            Self::execute_podman_exec_owned(container, working_dir, argv, None, timeout).await
        })
        .await
        .map_err(|error| {
            IsolationError::OciContainerFailed(format!("sandbox exec supervisor failed: {error}"))
        })?
    }

    /// Run a command inside the container with `stdin` piped in — used to
    /// write file contents (`exec_stdin(&["sh","-c","cat > path"], content)`).
    pub async fn exec_stdin(
        &self,
        argv: &[&str],
        stdin: &str,
        timeout: Duration,
    ) -> Result<ExecResult, IsolationError> {
        if self.passive_start {
            return self.passive_exec(argv, Some(stdin), timeout).await;
        }
        let container = self.runtime_target().to_string();
        let working_dir = self.working_dir.clone();
        let argv = argv.iter().map(|value| (*value).to_string()).collect();
        let stdin = stdin.as_bytes().to_vec();
        tokio::spawn(async move {
            Self::execute_podman_exec_owned(container, working_dir, argv, Some(stdin), timeout)
                .await
        })
        .await
        .map_err(|error| {
            IsolationError::OciContainerFailed(format!(
                "sandbox stdin exec supervisor failed: {error}"
            ))
        })?
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
        if self.passive_start {
            if let Ok(mut state) = status.lock() {
                *state = "blocked (passive recovery sandbox)".to_string();
            }
            if let Ok(mut output) = log.lock() {
                *output =
                    "background commands are disabled in passive recovery sandboxes".to_string();
            }
            return id;
        }

        let container = self.runtime_target().to_string();
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
        if self.passive_start {
            return Err("interactive terminals are disabled in passive recovery sandboxes".into());
        }
        let id = format!(
            "term-{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        let term = crate::pty::PtyTerminal::spawn_podman(
            id,
            self.runtime_target(),
            &self.working_dir,
            command,
            rows,
            cols,
        )?;
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
        if self.passive_start {
            self.passive_execution_usable
                .store(false, std::sync::atomic::Ordering::Release);
        }
        if let Some(container_id) = &self.container_id {
            let _ =
                Self::remove_exact_container_identity(container_id, NAMED_REMOVE_COMMAND_TIMEOUT)
                    .await;
        } else {
            let mut remove = Command::new(PODMAN);
            remove.args(["rm", "-f", &self.container]);
            let _ = Self::run_bounded_command(remove, NAMED_REMOVE_COMMAND_TIMEOUT).await;
        }
    }

    /// Whether a cached passive-recovery handle can accept another command.
    /// An infrastructure error, timeout, or cancelled caller makes this false
    /// before exact container cleanup begins.
    pub fn execution_boundary_usable(&self) -> bool {
        !self.passive_start
            || self
                .passive_execution_usable
                .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Checked exact cleanup used before an invalidated recovery handle can be
    /// replaced. Unlike [`SessionSandbox::stop`], failures are not discarded.
    pub async fn stop_checked(&self) -> Result<(), IsolationError> {
        if self.passive_start {
            self.passive_execution_usable
                .store(false, std::sync::atomic::Ordering::Release);
        }
        if let Some(container_id) = &self.container_id {
            Self::remove_exact_container_identity(container_id, NAMED_REMOVE_COMMAND_TIMEOUT).await
        } else {
            Self::remove_exact_container_names(
                std::slice::from_ref(&self.container),
                NAMED_REMOVE_COMMAND_TIMEOUT,
            )
            .await
        }
    }

    fn spawn_output_reader<R>(
        mut reader: R,
    ) -> tokio::task::JoinHandle<std::io::Result<BoundedCapture>>
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        tokio::spawn(async move {
            let mut bytes = Vec::with_capacity(16 * 1024);
            let mut truncated = false;
            let mut chunk = [0_u8; 16 * 1024];
            loop {
                let read = reader.read(&mut chunk).await?;
                if read == 0 {
                    break;
                }
                let remaining = COMMAND_OUTPUT_MAX_BYTES.saturating_sub(bytes.len());
                let retained = remaining.min(read);
                bytes.extend_from_slice(&chunk[..retained]);
                truncated |= retained < read;
            }
            Ok(BoundedCapture { bytes, truncated })
        })
    }

    async fn collect_output_reader(
        mut reader: tokio::task::JoinHandle<std::io::Result<BoundedCapture>>,
    ) -> Result<BoundedCapture, IsolationError> {
        match tokio::time::timeout(COMMAND_REAP_TIMEOUT, &mut reader).await {
            Ok(Ok(Ok(capture))) => Ok(capture),
            Ok(Ok(Err(error))) => Err(IsolationError::Io(error)),
            Ok(Err(error)) => Err(IsolationError::OciContainerFailed(format!(
                "collecting Podman output: {error}"
            ))),
            Err(_) => {
                reader.abort();
                let _ = reader.await;
                Err(IsolationError::OciContainerFailed(
                    "collecting Podman output timed out after its process exited".to_string(),
                ))
            }
        }
    }

    /// Spawn an owned subprocess, bound its running time, and reap it after a
    /// forced kill. `kill_on_drop` is the cancellation fallback; the explicit
    /// kill + wait is the normal deadline path so a timed-out Podman client
    /// cannot continue mutating container state behind its caller's result.
    async fn run_bounded_command_owned(
        command: Command,
        timeout: Duration,
    ) -> Result<BoundedCommandOutput, IsolationError> {
        Self::run_bounded_command_with_input_owned(command, None, timeout).await
    }

    async fn run_bounded_command_with_input_owned(
        mut command: Command,
        input: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<BoundedCommandOutput, IsolationError> {
        command
            .kill_on_drop(true)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| IsolationError::OciContainerFailed(error.to_string()))?;
        let stdout_reader = Self::spawn_output_reader(child.stdout.take().ok_or_else(|| {
            IsolationError::OciContainerFailed("Podman stdout was not captured".to_string())
        })?);
        let stderr_reader = Self::spawn_output_reader(child.stderr.take().ok_or_else(|| {
            IsolationError::OciContainerFailed("Podman stderr was not captured".to_string())
        })?);

        let mut child_stdin = child.stdin.take();
        let waited = tokio::time::timeout(timeout, async {
            if let Some(bytes) = input {
                let sink = child_stdin.as_mut().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "Podman stdin was not captured",
                    )
                })?;
                sink.write_all(&bytes).await?;
                sink.shutdown().await?;
            }
            drop(child_stdin);
            child.wait().await
        })
        .await;
        let (status, timed_out) = match waited {
            Ok(Ok(status)) => (status, false),
            Ok(Err(error)) => {
                let kill_error = child.start_kill().err();
                let reap_error =
                    match tokio::time::timeout(COMMAND_REAP_TIMEOUT, child.wait()).await {
                        Ok(Ok(_)) => None,
                        Ok(Err(reap)) => Some(reap.to_string()),
                        Err(_) => Some(format!(
                            "process was not reaped within {} seconds",
                            COMMAND_REAP_TIMEOUT.as_secs()
                        )),
                    };
                stdout_reader.abort();
                stderr_reader.abort();
                return Err(IsolationError::OciContainerFailed(format!(
                    "writing to or waiting for Podman: {error}{}{}",
                    kill_error.map_or_else(String::new, |kill| format!(
                        "; initiating kill also failed: {kill}"
                    )),
                    reap_error
                        .map_or_else(String::new, |reap| format!("; reaping also failed: {reap}"))
                )));
            }
            Err(_) => {
                let kill_error = child.start_kill().err();
                match tokio::time::timeout(COMMAND_REAP_TIMEOUT, child.wait()).await {
                    Ok(Ok(status)) => (status, true),
                    Ok(Err(error)) => {
                        stdout_reader.abort();
                        stderr_reader.abort();
                        return Err(IsolationError::OciContainerFailed(format!(
                            "reaping timed-out Podman process: {error}{}",
                            kill_error.map_or_else(String::new, |kill| format!(
                                "; initiating kill also failed: {kill}"
                            ))
                        )));
                    }
                    Err(_) => {
                        stdout_reader.abort();
                        stderr_reader.abort();
                        return Err(IsolationError::OciContainerFailed(format!(
                            "timed-out Podman process could not be reaped within {} seconds{}",
                            COMMAND_REAP_TIMEOUT.as_secs(),
                            kill_error.map_or_else(String::new, |kill| format!(
                                "; initiating kill failed: {kill}"
                            ))
                        )));
                    }
                }
            }
        };
        let (stdout, stderr) = tokio::join!(
            Self::collect_output_reader(stdout_reader),
            Self::collect_output_reader(stderr_reader)
        );
        let stdout = stdout?;
        let stderr = stderr?;
        Ok(BoundedCommandOutput {
            status,
            stdout: stdout.bytes,
            stderr: stderr.bytes,
            stdout_truncated: stdout.truncated,
            stderr_truncated: stderr.truncated,
            timed_out,
        })
    }

    /// Keep subprocess ownership in a supervisor task. Dropping an HTTP
    /// handler or another caller only detaches the join handle; the supervisor
    /// still reaches its deadline, kills, and reaps the child.
    pub(crate) async fn run_bounded_command(
        command: Command,
        timeout: Duration,
    ) -> Result<BoundedCommandOutput, IsolationError> {
        tokio::spawn(async move { Self::run_bounded_command_owned(command, timeout).await })
            .await
            .map_err(|error| {
                IsolationError::OciContainerFailed(format!(
                    "Podman command supervisor failed: {error}"
                ))
            })?
    }

    async fn remove_container_names_once(
        containers: &[String],
        timeout: Duration,
    ) -> Result<BoundedCommandOutput, IsolationError> {
        let mut command = Command::new(PODMAN);
        command.args(Self::remove_container_args(containers));
        Self::run_bounded_command(command, timeout).await
    }

    fn remove_container_args(containers: &[String]) -> Vec<String> {
        // `--ignore` makes absence atomically successful. An exists-then-rm
        // sequence races concurrent cancellation and teardown. `--force`
        // still inherits Podman's ten-second graceful-stop delay unless time
        // is explicit; that exactly matched the old product deadline and made
        // a healthy `sleep infinity` lane look stuck at 10.2 seconds on macOS.
        let mut args = vec![
            "rm".to_string(),
            "--force".to_string(),
            "--time".to_string(),
            "0".to_string(),
            "--ignore".to_string(),
        ];
        args.extend(containers.iter().cloned());
        args
    }

    fn remove_volume_args(volumes: &[String]) -> Vec<String> {
        // Do not use `--force`: callers prove the exact owning containers are
        // absent first. A volume unexpectedly used elsewhere is an integrity
        // error, not permission to destroy that other container's storage.
        let mut args = vec!["volume".to_string(), "rm".to_string()];
        args.extend(volumes.iter().cloned());
        args
    }

    async fn remove_volume_names_once(
        volumes: &[String],
        timeout: Duration,
    ) -> Result<BoundedCommandOutput, IsolationError> {
        let mut command = Command::new(PODMAN);
        command.args(Self::remove_volume_args(volumes));
        Self::run_bounded_command(command, timeout).await
    }

    fn exact_names_present(containers: &[String], output: &[u8]) -> Vec<String> {
        let output = String::from_utf8_lossy(output);
        let existing = output.lines().map(str::trim).collect::<HashSet<_>>();
        containers
            .iter()
            .filter(|container| existing.contains(container.as_str()))
            .cloned()
            .collect()
    }

    async fn named_containers_present(
        containers: &[String],
        timeout: Duration,
    ) -> Result<Vec<String>, IsolationError> {
        let mut command = Command::new(PODMAN);
        command.args(["ps", "-a", "--format", "{{.Names}}"]);
        let output = Self::run_bounded_command(command, timeout).await?;
        if output.timed_out {
            return Err(IsolationError::OciContainerFailed(format!(
                "listing Podman containers timed out after {} seconds",
                timeout.as_secs_f64()
            )));
        }
        if !output.status.success() {
            return Err(IsolationError::OciContainerFailed(format!(
                "listing Podman containers: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(Self::exact_names_present(containers, &output.stdout))
    }

    async fn named_volumes_present(
        volumes: &[String],
        timeout: Duration,
    ) -> Result<Vec<String>, IsolationError> {
        let mut command = Command::new(PODMAN);
        command.args(["volume", "ls", "--format", "{{.Name}}"]);
        let output = Self::run_bounded_command(command, timeout).await?;
        if output.timed_out {
            return Err(IsolationError::OciContainerFailed(format!(
                "listing Podman volumes timed out after {} seconds",
                timeout.as_secs_f64()
            )));
        }
        if !output.status.success() {
            return Err(IsolationError::OciContainerFailed(format!(
                "listing Podman volumes: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(Self::exact_names_present(volumes, &output.stdout))
    }

    async fn reconcile_named_absence(
        containers: &[String],
        timeout: Duration,
    ) -> Result<Vec<String>, IsolationError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last_observation = None;
        let mut last_error = None;
        loop {
            let remaining_budget = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining_budget.is_zero() {
                break;
            }
            let probe_timeout = NAMED_REMOVE_PROBE_TIMEOUT.min(remaining_budget);
            match Self::named_containers_present(containers, probe_timeout).await {
                Ok(remaining) if remaining.is_empty() => return Ok(remaining),
                Ok(remaining) => last_observation = Some(remaining),
                Err(error) => last_error = Some(error),
            }
            let remaining_budget = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining_budget.is_zero() {
                break;
            }
            tokio::time::sleep(NAMED_REMOVE_POLL_INTERVAL.min(remaining_budget)).await;
        }
        if let Some(remaining) = last_observation {
            Ok(remaining)
        } else {
            Err(last_error.unwrap_or_else(|| {
                IsolationError::OciContainerFailed(
                    "container removal could not be reconciled".to_string(),
                )
            }))
        }
    }

    async fn reconcile_named_volume_absence(
        volumes: &[String],
        timeout: Duration,
    ) -> Result<Vec<String>, IsolationError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last_observation = None;
        let mut last_error = None;
        loop {
            let remaining_budget = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining_budget.is_zero() {
                break;
            }
            let probe_timeout = NAMED_REMOVE_PROBE_TIMEOUT.min(remaining_budget);
            match Self::named_volumes_present(volumes, probe_timeout).await {
                Ok(remaining) if remaining.is_empty() => return Ok(remaining),
                Ok(remaining) => last_observation = Some(remaining),
                Err(error) => last_error = Some(error),
            }
            let remaining_budget = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining_budget.is_zero() {
                break;
            }
            tokio::time::sleep(NAMED_REMOVE_POLL_INTERVAL.min(remaining_budget)).await;
        }
        if let Some(remaining) = last_observation {
            Ok(remaining)
        } else {
            Err(last_error.unwrap_or_else(|| {
                IsolationError::OciContainerFailed(
                    "dependency-volume removal could not be reconciled".to_string(),
                )
            }))
        }
    }

    fn remove_failure(output: &BoundedCommandOutput, timeout: Duration) -> String {
        if output.timed_out {
            format!(
                "Podman removal timed out after {} seconds",
                timeout.as_secs_f64()
            )
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            if stderr.is_empty() {
                format!("Podman removal exited with {}", output.status)
            } else {
                format!("Podman removal exited with {}: {stderr}", output.status)
            }
        }
    }

    async fn container_identity_present(
        container_id: &str,
        timeout: Duration,
    ) -> Result<bool, IsolationError> {
        let mut command = Command::new(PODMAN);
        command.args(["container", "exists", container_id]);
        let output = Self::run_bounded_command(command, timeout).await?;
        if output.timed_out {
            return Err(IsolationError::OciContainerFailed(format!(
                "checking exact Podman container identity timed out after {} seconds",
                timeout.as_secs_f64()
            )));
        }
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            status => Err(IsolationError::OciContainerFailed(format!(
                "checking exact Podman container identity exited {}: {}",
                status.map_or_else(|| "without a status".to_string(), |code| code.to_string()),
                String::from_utf8_lossy(&output.stderr).trim()
            ))),
        }
    }

    async fn reconcile_container_identity_absence(
        container_id: &str,
        timeout: Duration,
    ) -> Result<bool, IsolationError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last_error = None;
        loop {
            let remaining_budget = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining_budget.is_zero() {
                break;
            }
            let probe_timeout = NAMED_REMOVE_PROBE_TIMEOUT.min(remaining_budget);
            match Self::container_identity_present(container_id, probe_timeout).await {
                Ok(false) => return Ok(true),
                Ok(true) => last_error = None,
                Err(error) => last_error = Some(error),
            }
            let remaining_budget = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining_budget.is_zero() {
                break;
            }
            tokio::time::sleep(NAMED_REMOVE_POLL_INTERVAL.min(remaining_budget)).await;
        }
        match last_error {
            Some(error) => Err(error),
            None => Ok(false),
        }
    }

    async fn remove_exact_container_identity(
        container_id: &str,
        timeout: Duration,
    ) -> Result<(), IsolationError> {
        let targets = [container_id.to_string()];
        let first = Self::remove_container_names_once(&targets, timeout).await?;
        if !first.timed_out && first.status.success() {
            return Ok(());
        }
        let first_failure = Self::remove_failure(&first, timeout);
        if Self::reconcile_container_identity_absence(container_id, NAMED_REMOVE_RECONCILE_TIMEOUT)
            .await?
        {
            return Ok(());
        }

        let retry = Self::remove_container_names_once(&targets, timeout).await?;
        if !retry.timed_out && retry.status.success() {
            return Ok(());
        }
        let retry_failure = Self::remove_failure(&retry, timeout);
        if Self::reconcile_container_identity_absence(container_id, NAMED_REMOVE_RECONCILE_TIMEOUT)
            .await?
        {
            Ok(())
        } else {
            Err(IsolationError::OciContainerFailed(format!(
                "removing exact sandbox container identity failed ({first_failure}; retry: \
                 {retry_failure}); container {container_id} is still present"
            )))
        }
    }

    /// Remove a deterministically named sandbox even when no in-process handle
    /// survived (for example after a daemon crash). Missing containers are a
    /// successful no-op; any other Podman failure is surfaced so callers do not
    /// delete a still-mounted workspace underneath a live process.
    pub async fn remove_named(session_id: &str) -> Result<(), IsolationError> {
        Self::remove_named_many(&[session_id.to_string()], NAMED_REMOVE_COMMAND_TIMEOUT).await
    }

    /// Remove one sandbox and its deterministic Node dependency volume.
    ///
    /// Use this for permanent Session deletion, an explicit environment
    /// rebuild/runtime change, or terminal attempt cleanup. Normal Close and
    /// daemon restart should use [`Self::remove_named`] so dependencies remain
    /// reusable and host `node_modules` stays masked on reopen.
    pub async fn remove_named_with_dependencies(session_id: &str) -> Result<(), IsolationError> {
        Self::remove_named_many_with_dependencies(
            &[session_id.to_string()],
            NAMED_REMOVE_COMMAND_TIMEOUT,
        )
        .await
    }

    /// Remove several deterministic sandboxes with one owned Podman command.
    /// A deadline kills and reaps the local client, then exact-name polling
    /// reconciles a removal that may already be completing inside Podman's VM.
    /// Only names that are proven to remain are retried.
    pub async fn remove_named_many(
        session_ids: &[String],
        timeout: Duration,
    ) -> Result<(), IsolationError> {
        let containers = Self::container_names(session_ids);
        if containers.is_empty() {
            return Ok(());
        }

        // The start task owns this same exact-name lock until its detached
        // create/readiness supervisor reaches a terminal state. Cleanup may
        // therefore never return while an older cancelled start can still
        // create the deterministic container or dependency volume afterward.
        let _starts = Self::lock_container_starts(&containers).await;
        Self::remove_exact_container_names(&containers, timeout).await
    }

    fn container_names(session_ids: &[String]) -> Vec<String> {
        let mut containers = session_ids
            .iter()
            .map(|session_id| Self::container_name(session_id))
            .collect::<Vec<_>>();
        containers.sort();
        containers.dedup();
        containers
    }

    async fn lock_container_starts(containers: &[String]) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
        let mut guards = Vec::with_capacity(containers.len());
        for container in containers {
            guards.push(Self::start_lock(container).lock_owned().await);
        }
        guards
    }

    async fn remove_exact_container_names(
        containers: &[String],
        timeout: Duration,
    ) -> Result<(), IsolationError> {
        let first = Self::remove_container_names_once(containers, timeout).await?;
        if !first.timed_out && first.status.success() {
            return Ok(());
        }
        let first_failure = Self::remove_failure(&first, timeout);
        let remaining =
            Self::reconcile_named_absence(containers, NAMED_REMOVE_RECONCILE_TIMEOUT).await?;
        if remaining.is_empty() {
            return Ok(());
        }

        let retry = Self::remove_container_names_once(&remaining, timeout).await?;
        if !retry.timed_out && retry.status.success() {
            return Ok(());
        }
        let retry_failure = Self::remove_failure(&retry, timeout);
        let remaining =
            Self::reconcile_named_absence(&remaining, NAMED_REMOVE_RECONCILE_TIMEOUT).await?;
        if remaining.is_empty() {
            Ok(())
        } else {
            Err(IsolationError::OciContainerFailed(format!(
                "removing sandboxes failed ({first_failure}; retry: {retry_failure}); exact containers still present: {}",
                remaining.join(", ")
            )))
        }
    }

    /// Remove exact named sandboxes, then their exact derived Node dependency
    /// volumes. Container absence is proven before volume removal begins; a
    /// still-in-use volume is surfaced rather than force-removed.
    pub async fn remove_named_many_with_dependencies(
        session_ids: &[String],
        timeout: Duration,
    ) -> Result<(), IsolationError> {
        let containers = Self::container_names(session_ids);
        let _starts = Self::lock_container_starts(&containers).await;
        if !containers.is_empty() {
            Self::remove_exact_container_names(&containers, timeout).await?;
        }

        let mut seen = HashSet::new();
        let volumes = session_ids
            .iter()
            .map(|session_id| Self::dependency_volume_name(session_id))
            .filter(|volume| seen.insert(volume.clone()))
            .collect::<Vec<_>>();
        if volumes.is_empty() {
            return Ok(());
        }

        let first = Self::remove_volume_names_once(&volumes, timeout).await?;
        if !first.timed_out && first.status.success() {
            return Ok(());
        }
        let first_failure = Self::remove_failure(&first, timeout);
        let remaining =
            Self::reconcile_named_volume_absence(&volumes, NAMED_REMOVE_RECONCILE_TIMEOUT).await?;
        if remaining.is_empty() {
            return Ok(());
        }

        let retry = Self::remove_volume_names_once(&remaining, timeout).await?;
        if !retry.timed_out && retry.status.success() {
            return Ok(());
        }
        let retry_failure = Self::remove_failure(&retry, timeout);
        let remaining =
            Self::reconcile_named_volume_absence(&remaining, NAMED_REMOVE_RECONCILE_TIMEOUT)
                .await?;
        if remaining.is_empty() {
            Ok(())
        } else {
            Err(IsolationError::OciContainerFailed(format!(
                "removing sandbox dependency volumes failed ({first_failure}; retry: \
                 {retry_failure}); exact volumes still present: {}",
                remaining.join(", ")
            )))
        }
    }

    fn passive_idle_probe_script() -> &'static str {
        r#"set -eu
read -r init_comm < /proc/1/comm
[ "$init_comm" = sleep ] || {
  printf '%s\n' "unexpected passive init: $init_comm" >&2
  exit 1
}
unexpected=""
for process in /proc/[0-9]*; do
  pid=${process##*/}
  case "$pid" in
    1|"$$") continue ;;
  esac
  process_comm=unknown
  read -r process_comm < "$process/comm" || true
  unexpected="$unexpected $pid:$process_comm"
done
[ -z "$unexpected" ] || {
  printf '%s\n' "passive sandbox retained background processes:$unexpected" >&2
  exit 1
}
"#
    }

    async fn ensure_passive_idle_process(container: &str) -> Result<(), IsolationError> {
        let output = Self::podman_exec_shell(
            container,
            Self::passive_idle_probe_script(),
            NAMED_REMOVE_PROBE_TIMEOUT,
        )
        .await?;
        if output.timed_out {
            return Err(IsolationError::OciContainerFailed(
                "proving passive sandbox process quiescence timed out".to_string(),
            ));
        }
        if !output.status.success() {
            return Err(IsolationError::OciContainerFailed(format!(
                "the recovery sandbox did not become an inert single-process tool container: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }
}

/// The per-session runtime surface the session tools are written against.
///
/// `read_file`, `bash`, the terminals, and the rest call this trait — not a
/// concrete runtime — so a session can be backed by the rootless Podman
/// container ([`SessionSandbox`]) or the validated E2B Cloud remote backend
/// without the tools changing. Construction is backend-specific (see
/// [`SessionSandbox::start`]); this is only the running-session API.
#[async_trait::async_trait]
pub trait Sandbox: Send + Sync {
    /// The session working directory — the confinement root for file tools.
    fn root(&self) -> &Path;

    /// Opaque backend identity that must survive a daemon restart for exact
    /// remote cleanup. Deterministically named local sandboxes return `None`.
    fn runtime_id(&self) -> Option<&str> {
        None
    }

    /// Transfer final-Drop ownership to durable Session state. Local runtimes
    /// do not need a disposition change; remote implementations use this only
    /// after the exact runtime identity has been fsynced as Ready.
    fn preserve_on_drop(&self) {}

    /// Whether this handle can safely accept another serialized recovery
    /// command. Backends that do not use a poisonable local exec boundary are
    /// always usable by default.
    fn execution_boundary_usable(&self) -> bool {
        true
    }

    /// Host loopback port for a configured container port, when this backend
    /// exposes local Preview transport. Remote backends may return `None`.
    fn published_host_port(&self, _container_port: u16) -> Option<u16> {
        None
    }

    /// Run a command in the sandbox and capture its output.
    async fn exec(&self, argv: &[&str], timeout: Duration) -> Result<ExecResult, IsolationError>;

    /// Run a command with `stdin` piped in (used to write file contents).
    async fn exec_stdin(
        &self,
        argv: &[&str],
        stdin: &str,
        timeout: Duration,
    ) -> Result<ExecResult, IsolationError>;

    /// Start a long-running command in the background; returns a task id.
    fn spawn_background(&self, command: &str) -> String;

    /// Spawn an interactive PTY-backed terminal in the session.
    fn spawn_pty(
        &self,
        command: &str,
        rows: u16,
        cols: u16,
    ) -> Result<std::sync::Arc<crate::pty::PtyTerminal>, String>;

    /// Find a live terminal by id.
    fn get_terminal(&self, id: &str) -> Option<std::sync::Arc<crate::pty::PtyTerminal>>;

    /// Drop a PTY terminal; returns `true` if one with this id was present.
    fn kill_terminal(&self, id: &str) -> bool;

    /// Snapshot of every PTY terminal — id, command, alive flag.
    fn list_terminals(&self) -> Vec<(String, String, bool)>;

    /// Snapshot of this session's background tasks.
    fn list_tasks(&self) -> Vec<BgTask>;

    /// A handle to the **same** isolation instance, re-rooted at `root` (a
    /// subtree of the session mount — e.g. a `git worktree`). Does not start or
    /// stop the instance; the owning handle controls that. Used for variant
    /// lanes: each lane is a worktree inside the one shared session sandbox, so
    /// this works identically for a local container or a remote microVM.
    fn with_root(&self, root: &Path) -> std::sync::Arc<dyn Sandbox>;

    /// Stop the sandbox and release its resources. Best-effort.
    async fn stop(&self);

    /// Stop the sandbox and report whether its owned resources were actually
    /// released. Local implementations may retain the best-effort default;
    /// remote backends override this so a user-visible Delete/Rebuild cannot
    /// claim success while the remote sandbox is still alive.
    async fn stop_checked(&self) -> Result<(), IsolationError> {
        self.stop().await;
        Ok(())
    }

    /// Suspend a durable runtime without deleting it. Backends without a
    /// resumable pause primitive retain their checked-stop behavior.
    async fn pause_checked(&self) -> Result<(), IsolationError> {
        self.stop_checked().await
    }
}

// Bodies delegate to the inherent methods — dot-syntax resolves to those, not
// back into this trait impl (inherent methods take priority), so there is no
// recursion and Podman behavior is byte-for-byte unchanged.
#[async_trait::async_trait]
impl Sandbox for SessionSandbox {
    fn root(&self) -> &Path {
        self.root()
    }
    fn published_host_port(&self, container_port: u16) -> Option<u16> {
        self.published_host_port(container_port)
    }
    fn execution_boundary_usable(&self) -> bool {
        self.execution_boundary_usable()
    }
    async fn exec(&self, argv: &[&str], timeout: Duration) -> Result<ExecResult, IsolationError> {
        self.exec(argv, timeout).await
    }
    async fn exec_stdin(
        &self,
        argv: &[&str],
        stdin: &str,
        timeout: Duration,
    ) -> Result<ExecResult, IsolationError> {
        self.exec_stdin(argv, stdin, timeout).await
    }
    fn spawn_background(&self, command: &str) -> String {
        self.spawn_background(command)
    }
    fn spawn_pty(
        &self,
        command: &str,
        rows: u16,
        cols: u16,
    ) -> Result<std::sync::Arc<crate::pty::PtyTerminal>, String> {
        self.spawn_pty(command, rows, cols)
    }
    fn get_terminal(&self, id: &str) -> Option<std::sync::Arc<crate::pty::PtyTerminal>> {
        self.get_terminal(id)
    }
    fn kill_terminal(&self, id: &str) -> bool {
        self.kill_terminal(id)
    }
    fn list_terminals(&self) -> Vec<(String, String, bool)> {
        self.list_terminals()
    }
    fn list_tasks(&self) -> Vec<BgTask> {
        self.list_tasks()
    }
    fn with_root(&self, root: &Path) -> std::sync::Arc<dyn Sandbox> {
        std::sync::Arc::new(SessionSandbox {
            container: self.container.clone(),
            container_id: self.container_id.clone(),
            working_dir: root.to_path_buf(),
            published_ports: self.published_ports.clone(),
            effective_image: self.effective_image.clone(),
            node_dependency_volume: self.node_dependency_volume.clone(),
            passive_start: self.passive_start,
            passive_execution_usable: self.passive_execution_usable.clone(),
            passive_exec_lock: self.passive_exec_lock.clone(),
            tasks: std::sync::Mutex::new(Vec::new()),
            terminals: std::sync::Mutex::new(Vec::new()),
        })
    }
    async fn stop(&self) {
        self.stop().await
    }
    async fn stop_checked(&self) -> Result<(), IsolationError> {
        self.stop_checked().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn output_reader_caps_retention_but_drains_to_eof() {
        let (reader, mut writer) = tokio::io::duplex(16 * 1024);
        let reader = SessionSandbox::spawn_output_reader(reader);
        let writer = tokio::spawn(async move {
            let chunk = vec![b'x'; 16 * 1024];
            for _ in 0..(COMMAND_OUTPUT_MAX_BYTES / chunk.len() + 4) {
                writer.write_all(&chunk).await.unwrap();
            }
            writer.shutdown().await.unwrap();
        });

        writer.await.unwrap();
        let capture = SessionSandbox::collect_output_reader(reader).await.unwrap();
        assert_eq!(capture.bytes.len(), COMMAND_OUTPUT_MAX_BYTES);
        assert!(capture.truncated);
    }

    #[test]
    fn truncated_output_drops_only_an_incomplete_utf8_suffix_and_marks_it() {
        let mut bytes = vec![b'a'; COMMAND_OUTPUT_MAX_BYTES - 1];
        bytes.push(0xf0); // first byte of a four-byte emoji clipped by the cap
        let output = captured_output_text(&bytes, true, "stdout");
        assert!(!output.contains('\u{fffd}'));
        assert!(output.starts_with(&"a".repeat(COMMAND_OUTPUT_MAX_BYTES - 1)));
        assert!(output_has_truncation_marker(&output, "stdout"));

        let invalid = captured_output_text(&[b'a', 0xff, b'b'], false, "stderr");
        assert_eq!(invalid, "a\u{fffd}b");
        let invalid_then_incomplete =
            captured_output_text(&[b'a', 0xff, b'b', 0xf0, 0x9f], true, "stderr");
        assert!(invalid_then_incomplete.starts_with("a\u{fffd}b\n… stderr truncated"));
    }

    #[cfg(unix)]
    fn process_is_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[cfg(unix)]
    async fn wait_for_pid(path: &Path) -> u32 {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(value) = tokio::fs::read_to_string(path).await {
                return value.trim().parse().expect("child should record its pid");
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "child did not record its pid"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[cfg(unix)]
    async fn assert_process_stops(pid: u32) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while process_is_alive(pid) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!process_is_alive(pid), "timed-out child {pid} survived");
    }

    #[cfg(unix)]
    fn sleeping_command(pid_file: &Path) -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", "printf %s $$ > \"$1\"; exec sleep 30", "sh"])
            .arg(pid_file);
        command
    }

    #[test]
    fn port_conflict_detection_covers_proxy_already_running() {
        // These variants route to bounded dynamic reallocation. None may
        // trigger the old behavior that silently started without Preview.
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
    }

    #[test]
    fn legacy_exposure_candidates_require_exact_names_and_immutable_ids() {
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        let stdout = format!(
            "{first}\taxo-ses-primary\n{second}\t/axo-ses-recovery\n{first}\taxo-ses-duplicate\n{}\tunrelated\n",
            "not-an-id"
        );
        assert_eq!(
            SessionSandbox::parse_data_root_exposure_candidates(stdout.as_bytes()).unwrap(),
            vec![first, second]
        );

        let invalid = b"not-an-id\taxo-ses-primary\n";
        assert!(SessionSandbox::parse_data_root_exposure_candidates(invalid).is_err());
        assert!(SessionSandbox::parse_data_root_exposure_candidates(
            b"aaaaaaaaaaaa axo-ses-primary\n"
        )
        .is_err());
    }

    #[test]
    fn checked_owned_cleanup_selects_normal_sessions_and_ways_by_immutable_id() {
        let authority = "a".repeat(64);
        let args = SessionSandbox::owned_container_list_args(&authority);
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--filter" && pair[1] == "name=axo-ses-"));
        assert!(args.windows(2).any(|pair| {
            pair[0] == "--filter"
                && pair[1] == format!("label={RUNTIME_AUTHORITY_LABEL}={authority}")
        }));
        assert!(args.iter().any(|argument| argument == "--no-trunc"));

        let primary_id = "b".repeat(64);
        let way_id = "c".repeat(64);
        let listed = format!("{primary_id}\taxo-ses-primary\n{way_id}\taxo-ses-attempt-set-0\n");
        assert_eq!(
            SessionSandbox::parse_owned_container_candidates(listed.as_bytes()).unwrap(),
            vec![
                (primary_id, "axo-ses-primary".to_string()),
                (way_id, "axo-ses-attempt-set-0".to_string()),
            ]
        );
        assert!(
            SessionSandbox::parse_owned_container_candidates(b"aaaaaaaaaaaa\tunrelated\n").is_err()
        );
    }

    #[test]
    fn legacy_exposure_inspection_selects_only_binds_overlapping_data_root() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let data_root = workspace.join("data");
        let nested_workspace = data_root.join("nested-workspace");
        let unrelated = root.path().join("unrelated");
        std::fs::create_dir_all(&nested_workspace).unwrap();
        std::fs::create_dir_all(&unrelated).unwrap();
        let data_root = std::fs::canonicalize(data_root).unwrap();
        let workspace = std::fs::canonicalize(workspace).unwrap();
        let nested_workspace = std::fs::canonicalize(nested_workspace).unwrap();
        let unrelated = std::fs::canonicalize(unrelated).unwrap();
        let exposing_id = "a".repeat(64);
        let unrelated_id = "b".repeat(64);
        let volume_id = "c".repeat(64);
        let current_id = "d".repeat(64);
        let authority = "e".repeat(64);
        let nested_id = "f".repeat(64);
        let listed = vec![
            exposing_id.clone(),
            unrelated_id.clone(),
            volume_id.clone(),
            current_id.clone(),
            nested_id.clone(),
        ];
        let inspect = serde_json::to_vec(&serde_json::json!([
            {
                "Id": exposing_id,
                "Mounts": [{"Type": "bind", "Source": workspace}]
            },
            {
                "Id": unrelated_id,
                "Mounts": [{"Type": "bind", "Source": unrelated}]
            },
            {
                "Id": volume_id,
                "Mounts": [{"Type": "volume", "Source": workspace}]
            },
            {
                "Id": current_id,
                "Config": {
                    "Labels": {RUNTIME_AUTHORITY_LABEL: authority.clone()}
                },
                "Mounts": [{"Type": "bind", "Source": workspace}]
            },
            {
                "Id": nested_id,
                "Mounts": [{"Type": "bind", "Source": nested_workspace}]
            }
        ]))
        .unwrap();
        assert_eq!(
            SessionSandbox::data_root_exposing_ids(&listed, &data_root, &authority, &inspect)
                .unwrap(),
            vec![listed[0].clone(), listed[4].clone()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_exposure_selection_survives_an_ambient_data_path_swap() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let data_path = workspace.join("data");
        let moved_data = root.path().join("moved-data");
        let replacement = root.path().join("replacement");
        std::fs::create_dir_all(&data_path).unwrap();
        std::fs::create_dir_all(&replacement).unwrap();
        let retained_data_path = std::fs::canonicalize(&data_path).unwrap();
        let mounted_workspace = std::fs::canonicalize(&workspace).unwrap();

        std::fs::rename(&data_path, &moved_data).unwrap();
        symlink(&replacement, &data_path).unwrap();

        let container_id = "a".repeat(64);
        let authority = "e".repeat(64);
        let inspect = serde_json::to_vec(&serde_json::json!([{
            "Id": container_id,
            "Mounts": [{"Type": "bind", "Source": mounted_workspace}]
        }]))
        .unwrap();
        assert_eq!(
            SessionSandbox::data_root_exposing_ids(
                std::slice::from_ref(&container_id),
                &retained_data_path,
                &authority,
                &inspect,
            )
            .unwrap(),
            vec![container_id]
        );
    }

    #[test]
    fn legacy_exposure_inspection_fails_closed_on_incomplete_or_invalid_results() {
        let root = tempfile::tempdir().unwrap();
        let data_root = std::fs::canonicalize(root.path()).unwrap();
        let listed = vec!["a".repeat(64)];
        let authority = "e".repeat(64);

        assert!(SessionSandbox::data_root_exposing_ids(
            &listed,
            &data_root,
            &authority,
            b"not-json"
        )
        .is_err());
        assert!(
            SessionSandbox::data_root_exposing_ids(&listed, &data_root, &authority, b"[]").is_err()
        );
        let missing_mounts = serde_json::to_vec(&serde_json::json!([{"Id": listed[0]}])).unwrap();
        assert!(SessionSandbox::data_root_exposing_ids(
            &listed,
            &data_root,
            &authority,
            &missing_mounts
        )
        .is_err());
        let unrequested = serde_json::to_vec(&serde_json::json!([{
            "Id": "b".repeat(64),
            "Mounts": []
        }]))
        .unwrap();
        assert!(SessionSandbox::data_root_exposing_ids(
            &listed,
            &data_root,
            &authority,
            &unrequested
        )
        .is_err());
    }

    #[test]
    fn published_port_parser_keeps_container_and_host_identity_separate() {
        let mappings = SessionSandbox::parse_published_ports(
            "3000/tcp -> 127.0.0.1:43117\n5173/tcp -> [::1]:43118\n",
        );
        assert_eq!(mappings.get(&3000), Some(&43117));
        assert_eq!(mappings.get(&5173), Some(&43118));
        assert_eq!(mappings.len(), 2);
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
    fn setup_result_is_persistable_and_reports_success() {
        let result = SandboxSetupResult {
            command: "npm ci".to_string(),
            exit_code: 0,
            stdout: "installed".to_string(),
            stderr: String::new(),
        };
        assert!(result.ok());
        let encoded = serde_json::to_string(&result).unwrap();
        assert_eq!(
            serde_json::from_str::<SandboxSetupResult>(&encoded).unwrap(),
            result
        );
    }

    #[test]
    fn container_reconciliation_uses_exact_names() {
        let targets = vec![
            "axo-ses-one".to_string(),
            "axo-ses-two".to_string(),
            "axo-ses-three".to_string(),
        ];
        let present = SessionSandbox::exact_names_present(
            &targets,
            b"axo-ses-one-more\n axo-ses-two \naxo-ses-three-old\n",
        );
        assert_eq!(present, vec!["axo-ses-two"]);
    }

    #[test]
    fn named_removal_does_not_spend_the_product_deadline_on_podman_grace() {
        let targets = vec!["axo-ses-one".to_string(), "axo-ses-two".to_string()];
        assert_eq!(
            SessionSandbox::remove_container_args(&targets),
            [
                "rm",
                "--force",
                "--time",
                "0",
                "--ignore",
                "axo-ses-one",
                "axo-ses-two",
            ]
        );
    }

    #[test]
    fn late_removal_reconciliation_accepts_absence_and_retries_exact_remainder() {
        let targets = vec![
            "axo-ses-one".to_string(),
            "axo-ses-two".to_string(),
            "axo-ses-three".to_string(),
        ];
        // Model a removal that crosses the command deadline: two names vanish
        // before the exact-name probe and only the observed remainder retries.
        let remaining = SessionSandbox::exact_names_present(&targets, b"axo-ses-two\n");
        assert_eq!(remaining, vec!["axo-ses-two"]);
        assert!(SessionSandbox::exact_names_present(&remaining, b"").is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_command_timeout_kills_and_reaps_child() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let output = SessionSandbox::run_bounded_command(
            sleeping_command(&pid_file),
            Duration::from_millis(250),
        )
        .await
        .unwrap();
        let pid = wait_for_pid(&pid_file).await;
        assert!(output.timed_out);
        assert_process_stops(pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_supervisor_reaps_child_after_caller_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let caller = tokio::spawn(SessionSandbox::run_bounded_command(
            sleeping_command(&pid_file),
            Duration::from_millis(250),
        ));
        let pid = wait_for_pid(&pid_file).await;
        assert!(process_is_alive(pid));

        caller.abort();
        let _ = caller.await;
        assert_process_stops(pid).await;
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
        assert!(p.runtime_authority.is_none());
    }

    #[test]
    fn curated_images_are_trusted_and_arbitrary_images_are_never_substituted() {
        let policy = SandboxPolicy::default();
        assert_eq!(
            SessionSandbox::resolve_effective_image(None, policy.allow_untrusted_image).unwrap(),
            DEFAULT_IMAGE
        );
        for image in CURATED_IMAGES {
            assert_eq!(
                SessionSandbox::resolve_effective_image(Some(image), policy.allow_untrusted_image)
                    .unwrap(),
                *image
            );
        }
        for alias in [
            "node:20-slim",
            "library/node:20-slim",
            "docker.io/node:20-slim",
        ] {
            assert_eq!(
                SessionSandbox::resolve_effective_image(Some(alias), policy.allow_untrusted_image)
                    .unwrap(),
                "docker.io/library/node:20-slim"
            );
        }
        let error = SessionSandbox::resolve_effective_image(
            Some("registry.example.invalid/project/runtime:latest"),
            policy.allow_untrusted_image,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("registry.example.invalid"));
        assert!(error.contains("requires explicit trust"));
        assert!(error.contains("did not silently substitute"));

        let trusted = SandboxPolicy {
            allow_untrusted_image: true,
            ..policy
        };
        assert_eq!(
            SessionSandbox::resolve_effective_image(
                Some("registry.example.invalid/project/runtime:latest"),
                trusted.allow_untrusted_image
            )
            .unwrap(),
            "registry.example.invalid/project/runtime:latest"
        );
        for invalid in ["--privileged", "node:20\n--volume=/host:/host"] {
            let error = SessionSandbox::resolve_effective_image(
                Some(invalid),
                trusted.allow_untrusted_image,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("image reference"));
            assert!(error.contains("invalid"));
        }
    }

    #[test]
    fn node_projects_receive_distinct_deterministic_dependency_volumes() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            SessionSandbox::node_dependency_volume("primary", directory.path()),
            None
        );
        std::fs::write(directory.path().join("package.json"), "{}").unwrap();
        assert_eq!(
            SessionSandbox::node_dependency_volume("primary", directory.path()).as_deref(),
            Some("axo-ses-primary-node-modules")
        );
        assert_ne!(
            SessionSandbox::dependency_volume_name("primary"),
            SessionSandbox::dependency_volume_name("primary-way-1")
        );
    }

    #[test]
    fn node_dependency_mount_masks_host_modules_without_copying_them() {
        let args = SessionSandbox::build_run_args(
            "axo-ses-primary",
            "/workspace",
            DEFAULT_IMAGE,
            Some("axo-ses-primary-node-modules"),
            true,
            &[],
            &SandboxPolicy::default(),
        );
        assert!(args.windows(2).any(|pair| {
            pair[0] == "--mount"
                && pair[1]
                    == "type=volume,source=axo-ses-primary-node-modules,destination=/workspace/node_modules"
        }));
        assert!(!args.iter().any(|argument| argument.contains("copy")));
        assert!(!args.iter().any(|argument| {
            argument.contains("source=/workspace/node_modules")
                || argument.contains("/workspace/node_modules:/workspace/node_modules")
        }));
    }

    #[test]
    fn passive_start_bypasses_the_image_entrypoint() {
        let policy = SandboxPolicy {
            passive_start: true,
            ..SandboxPolicy::default()
        };
        let args = SessionSandbox::build_run_args(
            "axo-ses-recovery",
            "/workspace",
            DEFAULT_IMAGE,
            None,
            true,
            &[],
            &policy,
        );
        let image = args
            .iter()
            .position(|argument| argument == DEFAULT_IMAGE)
            .expect("the image must remain explicit");
        assert_eq!(&args[image - 2..image], ["--entrypoint", "/bin/sh"]);
        assert_eq!(&args[image + 1..], ["-c", "exec sleep infinity"]);
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--network" && pair[1] == "none"));
        assert!(!args.iter().any(|argument| argument == "-p"));
    }

    #[test]
    fn passive_recovery_image_is_provisioned_without_a_workspace_mount() {
        let args = SessionSandbox::passive_recovery_provision_args("axo-recovery-image-test");
        assert_eq!(args[0], "run");
        assert!(args.iter().any(|argument| argument == DEFAULT_IMAGE));
        assert!(!args.iter().any(|argument| {
            argument == "--mount"
                || argument == "--volume"
                || argument == "-v"
                || argument.contains("/workspace")
        }));
    }

    #[test]
    fn control_plane_data_is_masked_or_overlap_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let data = workspace.join("data");
        let nested_workspace = data.join("repository");
        let disjoint = root.path().join("other-data");
        std::fs::create_dir_all(&nested_workspace).unwrap();
        std::fs::create_dir_all(&disjoint).unwrap();

        let (canonical_workspace, masks) =
            SessionSandbox::control_plane_paths(&workspace, std::slice::from_ref(&data)).unwrap();
        let mask = masks
            .first()
            .expect("a data root below the Workspace must be masked")
            .clone();
        assert_eq!(
            canonical_workspace,
            std::fs::canonicalize(&workspace).unwrap()
        );
        assert_eq!(mask, std::fs::canonicalize(&data).unwrap());
        assert!(SessionSandbox::control_plane_paths(&data, std::slice::from_ref(&data)).is_err());
        assert!(
            SessionSandbox::control_plane_paths(&nested_workspace, std::slice::from_ref(&data))
                .is_err(),
            "a Workspace below the data root would expose control-plane state"
        );
        assert_eq!(
            SessionSandbox::control_plane_paths(&workspace, std::slice::from_ref(&disjoint))
                .unwrap(),
            (std::fs::canonicalize(&workspace).unwrap(), Vec::new())
        );

        let policy = SandboxPolicy {
            control_plane_dirs: vec![mask.clone()],
            ..SandboxPolicy::default()
        };
        let args = SessionSandbox::build_run_args(
            "axo-ses-control-plane-mask",
            workspace.to_str().unwrap(),
            DEFAULT_IMAGE,
            None,
            false,
            &[],
            &policy,
        );
        assert!(args.windows(2).any(|pair| {
            pair[0] == "--tmpfs"
                && pair[1]
                    == format!(
                        "{}:rw,noexec,nosuid,nodev,notmpcopyup",
                        mask.to_string_lossy()
                    )
        }));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_workspace_spelling_cannot_bypass_control_plane_mask() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let data = workspace.join("data");
        let alias = root.path().join("workspace-alias");
        std::fs::create_dir_all(&data).unwrap();
        symlink(&workspace, &alias).unwrap();

        let (mounted_workspace, masks) =
            SessionSandbox::control_plane_paths(&alias, std::slice::from_ref(&data)).unwrap();
        assert_eq!(
            mounted_workspace,
            std::fs::canonicalize(&workspace).unwrap()
        );
        assert_eq!(masks, vec![std::fs::canonicalize(&data).unwrap()]);
        assert_ne!(mounted_workspace, alias);
    }

    #[test]
    fn every_control_plane_directory_is_masked_or_rejected() {
        let root = tempfile::tempdir().unwrap();
        let broad_workspace = root.path().join("workspace");
        let data = broad_workspace.join("data");
        let lease_root = broad_workspace.join("axocoatl-daemon-leases");
        let ipc_root = broad_workspace.join(".axocoatl/run");
        let lease_nested_workspace = lease_root.join("nested-repository");
        let ipc_nested_workspace = ipc_root.join("nested-repository");
        let ordinary_workspace = broad_workspace.join("repository");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&lease_nested_workspace).unwrap();
        std::fs::create_dir_all(&ipc_nested_workspace).unwrap();
        std::fs::create_dir_all(&ordinary_workspace).unwrap();

        let control_plane_dirs = vec![data.clone(), lease_root.clone(), ipc_root.clone()];
        let (_, masks) =
            SessionSandbox::control_plane_paths(&broad_workspace, &control_plane_dirs).unwrap();
        assert_eq!(
            masks,
            vec![
                std::fs::canonicalize(&ipc_root).unwrap(),
                std::fs::canonicalize(&lease_root).unwrap(),
                std::fs::canonicalize(&data).unwrap(),
            ]
        );

        let policy = SandboxPolicy {
            control_plane_dirs: masks.clone(),
            ..SandboxPolicy::default()
        };
        let args = SessionSandbox::build_run_args(
            "axo-ses-all-control-plane-masks",
            broad_workspace.to_str().unwrap(),
            DEFAULT_IMAGE,
            None,
            false,
            &[],
            &policy,
        );
        for mask in &masks {
            assert!(args.windows(2).any(|pair| {
                pair[0] == "--tmpfs"
                    && pair[1]
                        == format!(
                            "{}:rw,noexec,nosuid,nodev,notmpcopyup",
                            mask.to_string_lossy()
                        )
            }));
        }

        assert!(
            SessionSandbox::control_plane_paths(&lease_root, &control_plane_dirs).is_err(),
            "a Workspace equal to the external lease root must be rejected"
        );
        assert!(
            SessionSandbox::control_plane_paths(&lease_nested_workspace, &control_plane_dirs)
                .is_err(),
            "a Workspace below the external lease root must be rejected"
        );
        assert!(
            SessionSandbox::control_plane_paths(&ipc_nested_workspace, &control_plane_dirs)
                .is_err(),
            "a Workspace below the IPC parent must be rejected"
        );
        assert_eq!(
            SessionSandbox::control_plane_paths(&ordinary_workspace, &control_plane_dirs).unwrap(),
            (
                std::fs::canonicalize(&ordinary_workspace).unwrap(),
                Vec::new()
            )
        );
    }

    #[test]
    fn readiness_probe_and_provisioner_cover_product_commands_and_common_distros() {
        let probe = SessionSandbox::required_commands_probe();
        for command in REQUIRED_REPOSITORY_COMMANDS {
            assert!(probe.contains(command), "probe omitted {command}");
        }

        let provisioner = SessionSandbox::repository_provision_script();
        for package_manager in ["apk", "apt-get", "microdnf", "dnf", "yum", "zypper"] {
            assert!(
                provisioner.contains(package_manager),
                "provisioner omitted {package_manager}"
            );
        }
        assert!(provisioner.contains("git"));
        assert!(provisioner.contains("coreutils"));
        assert!(provisioner.contains("findutils"));
    }

    #[test]
    fn dependency_volume_cleanup_is_exact_and_never_forced() {
        let volumes = vec![
            SessionSandbox::dependency_volume_name("one"),
            SessionSandbox::dependency_volume_name("two"),
        ];
        assert_eq!(
            SessionSandbox::remove_volume_args(&volumes),
            [
                "volume",
                "rm",
                "axo-ses-one-node-modules",
                "axo-ses-two-node-modules",
            ]
        );
        assert!(!SessionSandbox::remove_volume_args(&volumes)
            .iter()
            .any(|argument| argument == "--force"));
    }

    #[tokio::test]
    async fn lifecycle_supervisor_runs_cleanup_after_caller_cancellation() {
        let (cleaned, cleaned_rx) = tokio::sync::oneshot::channel();
        let lifecycle = SandboxLifecycleSupervisor::spawn(async move {
            let _ = cleaned.send(());
            Ok(())
        });

        // Dropping the owner models request/task cancellation: the supervisor
        // is detached, sees the missing Keep decision, and performs cleanup.
        drop(lifecycle);
        tokio::time::timeout(Duration::from_secs(1), cleaned_rx)
            .await
            .expect("cleanup supervisor should run")
            .expect("cleanup marker should be delivered");
    }

    #[tokio::test]
    async fn lifecycle_supervisor_keeps_a_successfully_handed_off_sandbox() {
        let cleaned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let marker = cleaned.clone();
        let lifecycle = SandboxLifecycleSupervisor::spawn(async move {
            marker.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        lifecycle
            .finish(SandboxLifecycleDisposition::Keep)
            .await
            .unwrap();
        assert!(!cleaned.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn same_name_starts_remain_serialized_after_the_caller_is_gone() {
        let first = SessionSandbox::start_lock("axo-ses-serialization-test");
        let second = SessionSandbox::start_lock("axo-ses-serialization-test");
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        let held = first.lock_owned().await;
        let (entered, entered_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            let _exclusive = second.lock_owned().await;
            let _ = entered.send(());
        });
        assert!(tokio::time::timeout(Duration::from_millis(50), entered_rx)
            .await
            .is_err());
        drop(held);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("next start should enter after the owned task releases the lock")
            .expect("start-lock waiter should not panic");
    }

    #[test]
    fn owned_handles_target_immutable_container_ids() {
        let mut sandbox = SessionSandbox::attach("axo-ses-reusable-name", Path::new("/workspace"));
        assert_eq!(sandbox.runtime_target(), "axo-ses-reusable-name");
        sandbox.container_id = Some("immutable-container-id".to_string());
        assert_eq!(sandbox.runtime_target(), "immutable-container-id");
    }

    #[test]
    fn run_args_always_apply_hardening() {
        let args = SessionSandbox::build_run_args(
            "axo-ses-x",
            "/w",
            DEFAULT_IMAGE,
            None,
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
        // Bridge network preserves the logical container port while asking
        // Podman for a unique, loopback-only host port.
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-p" && w[1] == "127.0.0.1::3000"));
        assert!(!args.iter().any(|arg| arg == "3000:3000"));
        assert!(!args
            .windows(2)
            .any(|w| w[0] == "--network" && w[1] == "none"));
    }

    #[test]
    fn runtime_authority_scopes_creation_and_orphan_reaping() {
        let authority_a = "a".repeat(64);
        let authority_b = "b".repeat(64);
        let policy_a = SandboxPolicy {
            runtime_authority: Some(authority_a.clone()),
            ..SandboxPolicy::default()
        };
        let policy_b = SandboxPolicy {
            runtime_authority: Some(authority_b.clone()),
            ..SandboxPolicy::default()
        };

        let run_a = SessionSandbox::build_run_args(
            "axo-ses-a",
            "/workspace/a",
            DEFAULT_IMAGE,
            None,
            false,
            &[],
            &policy_a,
        );
        let run_b = SessionSandbox::build_run_args(
            "axo-ses-b",
            "/workspace/b",
            DEFAULT_IMAGE,
            None,
            false,
            &[],
            &policy_b,
        );
        let label_a = format!("{RUNTIME_AUTHORITY_LABEL}={authority_a}");
        let label_b = format!("{RUNTIME_AUTHORITY_LABEL}={authority_b}");
        assert!(run_a
            .windows(2)
            .any(|pair| pair[0] == "--label" && pair[1] == label_a));
        assert!(!run_a.iter().any(|argument| argument == &label_b));
        assert!(run_b
            .windows(2)
            .any(|pair| pair[0] == "--label" && pair[1] == label_b));
        assert!(!run_b.iter().any(|argument| argument == &label_a));

        let reap_a = SessionSandbox::orphan_list_args(&authority_a);
        let reap_b = SessionSandbox::orphan_list_args(&authority_b);
        assert!(reap_a
            .windows(2)
            .any(|pair| pair[0] == "--filter" && pair[1] == "name=axo-ses-"));
        assert!(reap_a
            .windows(2)
            .any(|pair| { pair[0] == "--filter" && pair[1] == format!("label={label_a}") }));
        assert!(!reap_a
            .iter()
            .any(|argument| argument == &format!("label={label_b}")));
        assert!(reap_b
            .windows(2)
            .any(|pair| { pair[0] == "--filter" && pair[1] == format!("label={label_b}") }));
        assert!(!reap_b
            .iter()
            .any(|argument| argument == &format!("label={label_a}")));

        let unmanaged = SessionSandbox::build_run_args(
            "axo-ses-unmanaged",
            "/workspace/unmanaged",
            DEFAULT_IMAGE,
            None,
            false,
            &[],
            &SandboxPolicy::default(),
        );
        assert!(!unmanaged.iter().any(|argument| argument == "--label"));
    }

    #[test]
    fn run_args_network_none_cuts_off_publishing() {
        let policy = SandboxPolicy {
            network: SandboxNetwork::None,
            ..SandboxPolicy::default()
        };
        let args = SessionSandbox::build_run_args(
            "axo-ses-x",
            "/w",
            DEFAULT_IMAGE,
            None,
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
        let data_dir = dir.path().join("data");
        std::fs::create_dir(&data_dir).unwrap();
        std::fs::write(data_dir.join("host-control-state"), "host-only").unwrap();
        let policy = SandboxPolicy {
            control_plane_dirs: vec![data_dir.clone()],
            runtime_authority: Some("e".repeat(64)),
            ..SandboxPolicy::default()
        };
        let sb = SessionSandbox::start("test", dir.path(), None, &[], &[], &policy)
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

        // The Workspace may contain the v0.1-compatible ./data directory on
        // the host, but a nested tmpfs keeps it outside the repository's
        // writable container view.
        let hidden = sb
            .exec(
                &["sh", "-c", "test ! -e data/host-control-state"],
                Duration::from_secs(20),
            )
            .await
            .unwrap();
        assert!(hidden.ok());
        sb.exec(
            &["sh", "-c", "printf sandbox-only > data/forged-state"],
            Duration::from_secs(20),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(data_dir.join("host-control-state")).unwrap(),
            "host-only"
        );
        assert!(!data_dir.join("forged-state").exists());

        sb.stop().await;
    }

    /// End-to-end passive recovery liveness: needs Podman and package-network
    /// access the first time the local recovery tool image is prepared.
    #[tokio::test]
    #[ignore = "requires podman; run this test explicitly"]
    async fn passive_recovery_timeout_and_cancellation_remove_before_retry() {
        async fn wait_until_removed(sandbox_id: &str) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            while SessionSandbox::named_running(sandbox_id)
                .await
                .unwrap_or(false)
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(!SessionSandbox::named_running(sandbox_id)
                .await
                .unwrap_or(false));
        }

        let directory = tempfile::tempdir().unwrap();
        let sandbox_id = format!("passive-exec-test-{}", uuid::Uuid::new_v4());
        let policy = SandboxPolicy {
            passive_start: true,
            ..SandboxPolicy::default()
        };

        let sandbox = std::sync::Arc::new(
            SessionSandbox::start(
                &sandbox_id,
                directory.path(),
                Some(DEFAULT_IMAGE),
                &[],
                &[],
                &policy,
            )
            .await
            .expect("passive recovery sandbox should start"),
        );
        assert_eq!(sandbox.effective_image(), Some(PASSIVE_RECOVERY_IMAGE));
        let caller_sandbox = sandbox.clone();
        let caller = tokio::spawn(async move {
            caller_sandbox
                .exec(
                    &[
                        "sh",
                        "-c",
                        "printf begun > cancel-begun; sleep 30; printf finished > cancel-finished",
                    ],
                    Duration::from_secs(60),
                )
                .await
        });
        let begun = directory.path().join("cancel-begun");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !begun.exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(begun.exists(), "passive command should have started");
        caller.abort();
        let _ = caller.await;
        assert!(!sandbox.execution_boundary_usable());
        wait_until_removed(&sandbox_id).await;
        assert!(!directory.path().join("cancel-finished").exists());

        let timed = SessionSandbox::start(
            &sandbox_id,
            directory.path(),
            Some(DEFAULT_IMAGE),
            &[],
            &[],
            &policy,
        )
        .await
        .expect("replacement after cancellation should start");
        let timeout_error = timed
            .exec(
                &[
                    "sh",
                    "-c",
                    "printf begun > timeout-begun; sleep 30; printf finished > timeout-finished",
                ],
                Duration::from_millis(250),
            )
            .await
            .unwrap_err();
        assert!(matches!(timeout_error, IsolationError::Timeout(_)));
        assert!(!timed.execution_boundary_usable());
        wait_until_removed(&sandbox_id).await;
        assert!(!directory.path().join("timeout-finished").exists());

        let replacement = SessionSandbox::start(
            &sandbox_id,
            directory.path(),
            Some(DEFAULT_IMAGE),
            &[],
            &[],
            &policy,
        )
        .await
        .expect("replacement after timeout should start");
        assert!(replacement
            .exec(&["git", "--version"], Duration::from_secs(20))
            .await
            .unwrap()
            .ok());
        replacement.stop_checked().await.unwrap();
    }

    /// End-to-end Node isolation: needs Podman and package-network access.
    #[tokio::test]
    #[ignore = "requires podman; run this test explicitly"]
    async fn node_dependencies_are_linux_local_and_cleanup_is_exact() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("package.json"), "{}").unwrap();
        std::fs::create_dir(directory.path().join("node_modules")).unwrap();
        std::fs::write(
            directory.path().join("node_modules/host-marker"),
            "host-only",
        )
        .unwrap();
        let sandbox_id = format!("node-volume-test-{}", uuid::Uuid::new_v4());
        let sandbox = SessionSandbox::start(
            &sandbox_id,
            directory.path(),
            None,
            &[],
            &[],
            &SandboxPolicy::default(),
        )
        .await
        .expect("Node sandbox should become repository-ready");

        assert_eq!(sandbox.effective_image(), Some(DEFAULT_IMAGE));
        assert!(sandbox.uses_node_dependency_volume());
        assert!(sandbox
            .exec(&["git", "--version"], Duration::from_secs(20))
            .await
            .unwrap()
            .ok());
        assert!(sandbox
            .exec(
                &["sh", "-c", "test ! -e node_modules/host-marker"],
                Duration::from_secs(20),
            )
            .await
            .unwrap()
            .ok());
        assert!(sandbox
            .exec(
                &[
                    "sh",
                    "-c",
                    "printf container-only > node_modules/container-marker",
                ],
                Duration::from_secs(20),
            )
            .await
            .unwrap()
            .ok());
        assert_eq!(
            std::fs::read_to_string(directory.path().join("node_modules/host-marker")).unwrap(),
            "host-only"
        );
        assert!(!directory
            .path()
            .join("node_modules/container-marker")
            .exists());

        drop(sandbox);
        SessionSandbox::remove_named_with_dependencies(&sandbox_id)
            .await
            .expect("container and dependency volume should be removed exactly");
        SessionSandbox::remove_named_with_dependencies(&sandbox_id)
            .await
            .expect("dependency cleanup should be idempotent");
        assert!(SessionSandbox::named_volumes_present(
            &[SessionSandbox::dependency_volume_name(&sandbox_id)],
            Duration::from_secs(5),
        )
        .await
        .unwrap()
        .is_empty());
    }

    /// End-to-end cancellation: needs Podman and package-network access.
    #[tokio::test]
    #[ignore = "requires podman; run this test explicitly"]
    async fn cancelled_setup_removes_container_before_command_can_finish() {
        let directory = tempfile::tempdir().unwrap();
        let sandbox_id = format!("setup-cancel-test-{}", uuid::Uuid::new_v4());
        let sandbox = std::sync::Arc::new(
            SessionSandbox::start(
                &sandbox_id,
                directory.path(),
                None,
                &[],
                &[],
                &SandboxPolicy::default(),
            )
            .await
            .expect("sandbox should start"),
        );
        let setup_sandbox = sandbox.clone();
        let setup = tokio::spawn(async move {
            setup_sandbox
                .run_setup_commands(&[
                    "printf begun > setup-begun; sleep 30; printf finished > setup-finished"
                        .to_string(),
                ])
                .await
        });
        let begun = directory.path().join("setup-begun");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !begun.exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(begun.exists(), "setup command should have started");

        setup.abort();
        let _ = setup.await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while SessionSandbox::named_running(&sandbox_id)
            .await
            .unwrap_or(false)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!SessionSandbox::named_running(&sandbox_id)
            .await
            .unwrap_or(false));
        assert!(!directory.path().join("setup-finished").exists());
        SessionSandbox::remove_named_with_dependencies(&sandbox_id)
            .await
            .unwrap();
    }
}
