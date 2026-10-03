//! The egress sidecar: one `axo-egr-{session}` container per Session under
//! `network: egress`, running the supervisor's `--egress-proxy` mode from the
//! scratch egress image.
//!
//! The daemon keeps the container's stdio as the control channel
//! ([`crate::egress_control`]). The proxy listens on a Unix socket in the
//! `axo-egr-{session}` volume, which the Session container mounts read-only;
//! its bridge (PID 1) serves that socket on `127.0.0.1:3128`.
//!
//! The sidecar has no environment, no secret and no bind mount. When its
//! control channel is lost it is removed and restarted with backoff (1 s,
//! 5 s, then 30 s), at most five times in ten minutes. After that it stays
//! down, and every proxied request gets the bridge's 502.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::egress::{EgressAuthority, SidecarEvent};
use crate::egress_control::{self, ControlEnd, ControlHandle, ControlTiming};
use crate::egress_image::ROLE_LABEL;
use crate::error::IsolationError;
use crate::session_sandbox::RUNTIME_AUTHORITY_LABEL;
use crate::SessionSandbox;

/// Where the proxy socket's volume is mounted, in both containers.
pub const EGRESS_SOCKET_DIR: &str = "/run/axocoatl-egress";
/// The proxy's listening socket.
pub const EGRESS_PROXY_SOCKET: &str = "/run/axocoatl-egress/proxy.sock";
/// Where the Session's service sockets (one per exposed port) live.
pub const SERVICE_SOCKET_DIR: &str = "/run/axocoatl-svc";
/// Where processes in the Session container reach the proxy.
pub const PROXY_LISTEN: &str = "127.0.0.1:3128";
/// Label that marks objects created by tests, for cleanup by label only.
pub const TEST_LABEL: &str = "io.axocoatl.test";

/// Restart delays: the first restart waits 1 s, the second 5 s, later ones 30 s.
pub const RESTART_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
];
/// Most restarts within [`RESTART_WINDOW`].
pub const RESTART_BUDGET: usize = 5;
pub const RESTART_WINDOW: Duration = Duration::from_secs(600);

/// Most listeners one bridge opens (the supervisor's own limit).
pub const MAX_BRIDGE_LISTENERS: usize = 64;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const STDERR_TAIL_BYTES: usize = 4096;
const SIDECAR_MEMORY: &str = "128m";

pub fn sidecar_container_name(session_id: &str) -> String {
    format!("axo-egr-{session_id}")
}

pub fn egress_volume_name(session_id: &str) -> String {
    format!("axo-egr-{session_id}")
}

pub fn service_volume_name(session_id: &str) -> String {
    format!("axo-svc-{session_id}")
}

/// What one Session's sidecar runs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarSpec {
    pub session_id: String,
    /// The daemon's runtime authority label value, for ownership and reaping.
    pub runtime_authority: Option<String>,
    /// The egress image tag ([`crate::egress_image::ensure_egress_image`]).
    pub image: String,
    /// Podman network for the sidecar; `None` is Podman's default network.
    pub network: Option<String>,
    pub max_connections: u32,
    /// Refuse to run the sidecar without its pid and memory limits.
    pub require_resource_limits: bool,
    /// Extra `key=value` labels (tests use [`TEST_LABEL`]).
    pub labels: Vec<String>,
}

impl SidecarSpec {
    pub fn container(&self) -> String {
        sidecar_container_name(&self.session_id)
    }

    fn validate(&self) -> Result<(), IsolationError> {
        let refuse = |message: String| Err(IsolationError::OciSetupFailed(message));
        if !(1..=axocoatl_exec::egress::protocol::MAX_MAX_CONNECTIONS)
            .contains(&self.max_connections)
        {
            return refuse(format!(
                "egress max_connections {} is outside 1-{}",
                self.max_connections,
                axocoatl_exec::egress::protocol::MAX_MAX_CONNECTIONS
            ));
        }
        if self.session_id.is_empty()
            || !self
                .session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return refuse(format!(
                "Session id {:?} cannot name an egress sidecar",
                self.session_id
            ));
        }
        if let Some(network) = &self.network {
            let valid = network.len() <= 64
                && network
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
                && network
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'));
            if !valid {
                return refuse(format!(
                    "egress sidecar network {network:?} is not a valid name"
                ));
            }
        }
        if self.image.starts_with('-') || self.image.chars().any(char::is_control) {
            return refuse(format!("egress image {:?} is invalid", self.image));
        }
        Ok(())
    }
}

fn labels(spec: &SidecarSpec, role: &str) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(authority) = &spec.runtime_authority {
        args.push("--label".into());
        args.push(format!("{RUNTIME_AUTHORITY_LABEL}={authority}"));
    }
    args.push("--label".into());
    args.push(format!("{ROLE_LABEL}={role}"));
    for label in &spec.labels {
        args.push("--label".into());
        args.push(label.clone());
    }
    args
}

/// The sidecar's `podman run` arguments (pure). No environment, no secret and
/// no bind mount: the only mount is the proxy socket's volume.
pub fn build_sidecar_args(spec: &SidecarSpec, with_limits: bool) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "-i".into(),
        "--name".into(),
        spec.container(),
        "--pull=never".into(),
    ];
    args.extend(labels(spec, "egress"));
    args.extend([
        "--read-only".into(),
        "--cap-drop=ALL".into(),
        "--security-opt=no-new-privileges".into(),
        "--http-proxy=false".into(),
        "--no-hosts".into(),
        "--no-healthcheck".into(),
        "--image-volume=ignore".into(),
        "--user".into(),
        "0:0".into(),
    ]);
    if with_limits {
        args.extend([
            "--pids-limit".into(),
            (spec.max_connections + 32).to_string(),
            "--memory".into(),
            SIDECAR_MEMORY.into(),
        ]);
    }
    if let Some(network) = &spec.network {
        args.push("--network".into());
        args.push(network.clone());
    }
    args.extend([
        "--mount".into(),
        format!(
            "type=volume,source={},destination={EGRESS_SOCKET_DIR}",
            egress_volume_name(&spec.session_id)
        ),
        "--entrypoint".into(),
        crate::supervisor_program::SUPERVISOR_CONTAINER_PATH.into(),
        spec.image.clone(),
        "--egress-proxy".into(),
        "--socket".into(),
        EGRESS_PROXY_SOCKET.into(),
        "--max-connections".into(),
        spec.max_connections.to_string(),
    ]);
    args
}

/// `podman volume create` for one of the Session's egress volumes (pure).
pub fn volume_create_args(spec: &SidecarSpec, name: &str, role: &str) -> Vec<String> {
    let mut args = vec!["volume".into(), "create".into(), "--ignore".into()];
    args.extend(labels(spec, role));
    args.push(name.into());
    args
}

/// Create the Session's service-socket volume when no sidecar does (bridge
/// and none modes with service sockets).
pub(crate) async fn create_service_volume(
    session_id: &str,
    runtime_authority: Option<&str>,
    labels: &[String],
) -> Result<(), IsolationError> {
    let spec = SidecarSpec {
        session_id: session_id.to_string(),
        runtime_authority: runtime_authority.map(str::to_string),
        image: String::new(),
        network: None,
        max_connections: axocoatl_exec::egress::protocol::DEFAULT_MAX_CONNECTIONS,
        require_resource_limits: false,
        labels: labels.to_vec(),
    };
    let name = service_volume_name(session_id);
    podman(
        volume_create_args(&spec, &name, "service-sockets"),
        COMMAND_TIMEOUT,
    )
    .await
    .map_err(|error| {
        IsolationError::OciSetupFailed(format!(
            "creating the service-socket volume {name}: {error}"
        ))
    })
}

/// Where the sidecar is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarPhase {
    Starting,
    Ready,
    Restarting,
    Failed,
    Stopped,
}

impl SidecarPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Restarting => "restarting",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

/// A snapshot for the Session network view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidecarStatus {
    pub phase: SidecarPhase,
    pub generation: u32,
    pub restarts: u32,
}

struct Generation {
    number: u32,
    child: Child,
    handle: ControlHandle,
    task: JoinHandle<ControlEnd>,
}

struct Shared {
    spec: SidecarSpec,
    authority: Arc<dyn EgressAuthority>,
    timing: ControlTiming,
    status: Mutex<SidecarStatus>,
    stopping: AtomicBool,
    stop_signal: watch::Sender<bool>,
    control: Mutex<Option<ControlHandle>>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
    /// Serializes concurrent stops so each waits for the first.
    stop_lock: tokio::sync::Mutex<()>,
}

/// Live sidecars by Session id, so removing a Session's containers by name
/// also stops its sidecar's supervision instead of provoking a restart.
fn registry() -> &'static Mutex<std::collections::HashMap<String, std::sync::Weak<Shared>>> {
    static REGISTRY: std::sync::OnceLock<
        Mutex<std::collections::HashMap<String, std::sync::Weak<Shared>>>,
    > = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Stop the live sidecars of these Sessions, if this process runs any.
pub(crate) async fn stop_session_sidecars(session_ids: &[String]) {
    let live: Vec<Arc<Shared>> = {
        let registry = registry()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        session_ids
            .iter()
            .filter_map(|session| registry.get(session).and_then(std::sync::Weak::upgrade))
            .collect()
    };
    for shared in live {
        shared.stop().await;
    }
}

impl Shared {
    fn set_status(&self, phase: SidecarPhase, generation: u32, restarts: Option<u32>) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        status.phase = phase;
        status.generation = generation;
        if let Some(restarts) = restarts {
            status.restarts = restarts;
        }
    }

    fn status(&self) -> SidecarStatus {
        *self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Shut the proxy down, wait for its supervisor, and remove the
    /// container. Idempotent.
    async fn stop(self: &Arc<Self>) {
        let _serial = self.stop_lock.lock().await;
        self.stopping.store(true, Ordering::Release);
        self.stop_signal.send_replace(true);
        let control = self
            .control
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        if let Some(control) = control {
            control.shutdown();
        }
        let supervisor = self
            .supervisor
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        if let Some(mut supervisor) = supervisor {
            if tokio::time::timeout(STOP_TIMEOUT * 2, &mut supervisor)
                .await
                .is_err()
            {
                supervisor.abort();
                let _ = supervisor.await;
            }
        }
        if let Err(error) = remove_container(&self.spec.container()).await {
            tracing::warn!(container = %self.spec.container(), %error, "removing the egress sidecar failed");
        }
        let status = self.status();
        if status.phase != SidecarPhase::Stopped {
            self.set_status(SidecarPhase::Stopped, status.generation, None);
            self.authority
                .sidecar_event(SidecarEvent::Stopped {
                    generation: status.generation,
                })
                .await;
        }
    }
}

/// A running sidecar and its supervisor. Dropping it stops supervision and
/// kills the Podman client, which closes the proxy's input; the proxy then
/// exits and Podman removes the container. Call [`EgressSidecar::stop`] for an
/// orderly shutdown.
pub struct EgressSidecar {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for EgressSidecar {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EgressSidecar")
            .field("container", &self.shared.spec.container())
            .field("status", &self.shared.status())
            .finish()
    }
}

async fn podman(args: Vec<String>, timeout: Duration) -> Result<(), String> {
    let mut command = Command::new("podman");
    command.args(&args);
    let output = SessionSandbox::run_bounded_command(command, timeout)
        .await
        .map_err(|error| error.to_string())?;
    if output.timed_out {
        return Err(format!("podman {} timed out", args[0]));
    }
    if !output.status.success() {
        return Err(format!(
            "podman {} {}: {}",
            args[0],
            args.get(1).map(String::as_str).unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

async fn remove_container(name: &str) -> Result<(), String> {
    podman(
        vec![
            "rm".into(),
            "--force".into(),
            "--time".into(),
            "0".into(),
            "--ignore".into(),
            name.into(),
        ],
        COMMAND_TIMEOUT,
    )
    .await
}

fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    let start = text.len().saturating_sub(1024);
    let mut start = start;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].replace('\n', " ")
}

async fn reap(child: &mut Child) {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(STOP_TIMEOUT, child.wait()).await;
}

/// Start one sidecar process and complete its handshake.
async fn launch(shared: &Shared, number: u32, with_limits: bool) -> Result<Generation, String> {
    let container = shared.spec.container();
    remove_container(&container).await?;
    let mut command = Command::new("podman");
    command
        .args(build_sidecar_args(&shared.spec, with_limits))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("starting podman: {error}"))?;
    let (Some(stdin), Some(stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        reap(&mut child).await;
        return Err("the sidecar's stdio was not captured".into());
    };
    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    let sink = diagnostics.clone();
    let reader = tokio::spawn(async move {
        let mut chunk = [0u8; 1024];
        while let Ok(read) = stderr.read(&mut chunk).await {
            if read == 0 {
                break;
            }
            let mut tail = sink.lock().unwrap_or_else(|poison| poison.into_inner());
            tail.extend_from_slice(&chunk[..read]);
            let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
            tail.drain(..excess);
        }
    });
    match egress_control::start(
        number,
        stdout,
        stdin,
        shared.authority.clone(),
        shared.timing,
    )
    .await
    {
        Ok((handle, task)) => Ok(Generation {
            number,
            child,
            handle,
            task,
        }),
        Err(error) => {
            reap(&mut child).await;
            let _ = tokio::time::timeout(Duration::from_secs(2), reader).await;
            let detail = tail(
                &diagnostics
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()),
            );
            let _ = remove_container(&container).await;
            Err(if detail.is_empty() {
                error
            } else {
                format!("{error}: {detail}")
            })
        }
    }
}

impl EgressSidecar {
    /// Create the Session's egress and service-socket volumes, start the
    /// sidecar and wait for its handshake. A start that cannot complete leaves
    /// no sidecar behind.
    pub async fn start(
        spec: SidecarSpec,
        authority: Arc<dyn EgressAuthority>,
        timing: ControlTiming,
    ) -> Result<Self, IsolationError> {
        spec.validate()?;
        let container = spec.container();
        for (name, role) in [
            (egress_volume_name(&spec.session_id), "egress"),
            (service_volume_name(&spec.session_id), "service-sockets"),
        ] {
            podman(volume_create_args(&spec, &name, role), COMMAND_TIMEOUT)
                .await
                .map_err(|error| {
                    IsolationError::OciSetupFailed(format!(
                        "creating the egress volume {name}: {error}"
                    ))
                })?;
        }
        let (stop_signal, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            spec,
            authority,
            timing,
            status: Mutex::new(SidecarStatus {
                phase: SidecarPhase::Starting,
                generation: 1,
                restarts: 0,
            }),
            stopping: AtomicBool::new(false),
            stop_signal,
            control: Mutex::new(None),
            supervisor: Mutex::new(None),
            stop_lock: tokio::sync::Mutex::new(()),
        });
        shared
            .authority
            .sidecar_event(SidecarEvent::Starting {
                generation: 1,
                container: Some(container.clone()),
            })
            .await;
        let mut with_limits = true;
        let first = loop {
            match launch(&shared, 1, with_limits).await {
                Ok(generation) => break generation,
                Err(error)
                    if with_limits
                        && error.contains("cgroup")
                        && !shared.spec.require_resource_limits =>
                {
                    tracing::warn!(
                        container = %container,
                        "this host cannot apply resource limits to the egress sidecar; starting it without pid and memory limits"
                    );
                    with_limits = false;
                }
                Err(error) => {
                    shared
                        .authority
                        .sidecar_event(SidecarEvent::Failed {
                            generation: 1,
                            detail: error.clone(),
                        })
                        .await;
                    shared.set_status(SidecarPhase::Failed, 1, None);
                    return Err(IsolationError::OciContainerFailed(format!(
                        "starting the egress proxy {container}: {error}"
                    )));
                }
            }
        };
        Self::ready(&shared, &first).await;
        let supervisor = tokio::spawn(supervise(shared.clone(), first, with_limits));
        *shared
            .supervisor
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(supervisor);
        registry()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(shared.spec.session_id.clone(), Arc::downgrade(&shared));
        Ok(Self { shared })
    }

    async fn ready(shared: &Shared, generation: &Generation) {
        *shared
            .control
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(generation.handle.clone());
        shared.authority.attach_control(generation.handle.clone());
        shared.set_status(SidecarPhase::Ready, generation.number, None);
        shared
            .authority
            .sidecar_event(SidecarEvent::Ready {
                generation: generation.number,
                container: Some(shared.spec.container()),
            })
            .await;
    }

    pub fn status(&self) -> SidecarStatus {
        self.shared.status()
    }

    pub fn container(&self) -> String {
        self.shared.spec.container()
    }

    /// Shut the proxy down, wait for its supervisor, and remove the
    /// container. Idempotent.
    pub async fn stop(&self) {
        self.shared.stop().await;
    }
}

impl Drop for EgressSidecar {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.stop_signal.send_replace(true);
        if let Some(supervisor) = self
            .shared
            .supervisor
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            // Dropping the supervisor's child kills the Podman client; the
            // proxy then reads end of input and exits, and `--rm` removes it.
            supervisor.abort();
        }
        let mut registry = registry()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if registry
            .get(&self.shared.spec.session_id)
            .is_some_and(|live| live.ptr_eq(&Arc::downgrade(&self.shared)))
        {
            registry.remove(&self.shared.spec.session_id);
        }
    }
}

async fn supervise(shared: Arc<Shared>, mut current: Generation, with_limits: bool) {
    let mut recent: VecDeque<Instant> = VecDeque::new();
    let mut restarts = 0u32;
    let mut stop = shared.stop_signal.subscribe();
    loop {
        let end = match (&mut current.task).await {
            Ok(end) => end,
            Err(error) => ControlEnd::ChannelLost(format!("the control task failed: {error}")),
        };
        reap(&mut current.child).await;
        let _ = remove_container(&shared.spec.container()).await;
        let mut generation = current.number;
        if shared.stopping.load(Ordering::Acquire) || end == ControlEnd::Shutdown {
            shared.set_status(SidecarPhase::Stopped, generation, None);
            return;
        }
        loop {
            let now = Instant::now();
            while recent
                .front()
                .is_some_and(|started| now.duration_since(*started) > RESTART_WINDOW)
            {
                recent.pop_front();
            }
            if recent.len() >= RESTART_BUDGET {
                shared.set_status(SidecarPhase::Failed, generation, None);
                shared
                    .authority
                    .sidecar_event(SidecarEvent::BudgetSpent {
                        generation,
                        detail: format!(
                            "the egress proxy was restarted {RESTART_BUDGET} times in {} minutes; it stays stopped and proxied connections are refused",
                            RESTART_WINDOW.as_secs() / 60
                        ),
                    })
                    .await;
                return;
            }
            let delay = RESTART_BACKOFF[recent.len().min(RESTART_BACKOFF.len() - 1)];
            recent.push_back(now);
            restarts += 1;
            generation += 1;
            shared.set_status(SidecarPhase::Restarting, generation, Some(restarts));
            shared
                .authority
                .sidecar_event(SidecarEvent::Restarting { generation })
                .await;
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = stop.wait_for(|stopping| *stopping) => {}
            }
            if shared.stopping.load(Ordering::Acquire) {
                shared.set_status(SidecarPhase::Stopped, generation, None);
                return;
            }
            shared
                .authority
                .sidecar_event(SidecarEvent::Starting {
                    generation,
                    container: Some(shared.spec.container()),
                })
                .await;
            match launch(&shared, generation, with_limits).await {
                Ok(next) => {
                    if shared.stopping.load(Ordering::Acquire) {
                        next.handle.shutdown();
                        current = next;
                        break;
                    }
                    EgressSidecar::ready(&shared, &next).await;
                    current = next;
                    break;
                }
                Err(error) => {
                    shared
                        .authority
                        .sidecar_event(SidecarEvent::ChannelLost {
                            generation,
                            detail: format!("restarting the egress proxy failed: {error}"),
                        })
                        .await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SidecarSpec {
        SidecarSpec {
            session_id: "ses-1234".into(),
            runtime_authority: Some("a".repeat(64)),
            image: "localhost/axocoatl-egress:866a01dbe5366107-aarch64".into(),
            network: None,
            max_connections: 128,
            require_resource_limits: false,
            labels: Vec::new(),
        }
    }

    #[test]
    fn sidecar_arguments_carry_no_environment_secret_or_bind_mount() {
        let args = build_sidecar_args(&spec(), true);
        let joined = args.join(" ");
        for forbidden in [
            "-e",
            "--env",
            "--env-file",
            "-v",
            "--volume",
            "--privileged",
        ] {
            assert!(
                !args.iter().any(|arg| arg == forbidden),
                "{forbidden} in {joined}"
            );
        }
        assert!(!joined.contains("type=bind"), "{joined}");
        assert!(!joined.contains("axe_"), "{joined}");
        for required in [
            "--rm",
            "-i",
            "--pull=never",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--http-proxy=false",
            "--no-hosts",
            "--no-healthcheck",
            "--image-volume=ignore",
        ] {
            assert!(args.iter().any(|arg| arg == required), "{required} missing");
        }
        assert!(joined.contains("--name axo-egr-ses-1234"));
        assert!(joined.contains("--user 0:0"));
        assert!(joined.contains("--pids-limit 160 --memory 128m"));
        assert!(joined.contains(&format!(
            "--label {RUNTIME_AUTHORITY_LABEL}={}",
            "a".repeat(64)
        )));
        assert!(joined.contains("--label io.axocoatl.role=egress"));
        assert!(joined.contains(
            "--mount type=volume,source=axo-egr-ses-1234,destination=/run/axocoatl-egress --entrypoint /axocoatl-exec-supervisor localhost/axocoatl-egress:866a01dbe5366107-aarch64 --egress-proxy --socket /run/axocoatl-egress/proxy.sock --max-connections 128"
        ));
        assert!(joined.ends_with("--max-connections 128"));
        assert!(!joined.contains("--network"));
    }

    #[test]
    fn limits_and_network_follow_the_spec() {
        let mut spec = spec();
        spec.network = Some("axo-egress-test-1".into());
        spec.max_connections = 8;
        spec.labels = vec![format!("{TEST_LABEL}=egress-1")];
        let args = build_sidecar_args(&spec, false).join(" ");
        assert!(!args.contains("--pids-limit"));
        assert!(!args.contains("--memory"));
        assert!(args.contains("--network axo-egress-test-1"));
        assert!(args.contains("--label io.axocoatl.test=egress-1"));
        assert!(build_sidecar_args(&spec, true)
            .join(" ")
            .contains("--pids-limit 40"));
        let volume = volume_create_args(&spec, "axo-svc-ses-1234", "service-sockets").join(" ");
        assert!(volume.starts_with("volume create --ignore"));
        assert!(volume.contains("--label io.axocoatl.role=service-sockets"));
        assert!(volume.ends_with("axo-svc-ses-1234"));
    }

    #[test]
    fn invalid_specs_are_refused_before_podman_runs() {
        for broken in [
            SidecarSpec {
                max_connections: 0,
                ..spec()
            },
            SidecarSpec {
                max_connections: 257,
                ..spec()
            },
            SidecarSpec {
                session_id: "ses/../x".into(),
                ..spec()
            },
            SidecarSpec {
                network: Some("--privileged".into()),
                ..spec()
            },
            SidecarSpec {
                image: "--rm".into(),
                ..spec()
            },
        ] {
            assert!(broken.validate().is_err(), "{broken:?}");
        }
        assert!(spec().validate().is_ok());
    }

    #[test]
    fn names_derive_from_the_session() {
        assert_eq!(sidecar_container_name("s1"), "axo-egr-s1");
        assert_eq!(egress_volume_name("s1"), "axo-egr-s1");
        assert_eq!(service_volume_name("s1"), "axo-svc-s1");
    }
}
