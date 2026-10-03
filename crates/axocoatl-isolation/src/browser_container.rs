//! The browser container and the Session's service sockets.
//!
//! Each Session that uses the browser tools gets one browser container,
//! `axo-brw-{session}`, started on the first call and removed with the
//! Session. It has no network interface but loopback (`--network none`), a
//! read-only root, no capabilities, and no Workspace mount. Its PID 1 is the
//! execution supervisor's `--bridge`:
//!
//! - for each exposed Session port it listens on `127.0.0.1:{p}` and
//!   `[::1]:{p}` and forwards to `/run/axocoatl-svc/{p}.sock`. Any other
//!   `localhost` port refuses in the kernel.
//! - with declared hosts it also listens on a loopback proxy port (3128
//!   unless the Session exposes that port) and forwards to the egress proxy's
//!   socket, so Chromium reaches those hosts only through the daemon's
//!   egress policy. Without declared hosts there is no proxy listener.
//!
//! Under `bridge` and `none` the sockets come from the service forwarder,
//! `axo-svc-{session}`: the supervisor's `--bridge --unix-to-tcp` in the
//! scratch egress image, in a container of its own that joins the Session
//! container's network namespace (`--network container:<id>`). Only it
//! mounts the `axo-svc-{session}` volume writable, and only the browser
//! container mounts it at all (read-only). The Session container never sees
//! the sockets, so a read-only helper whose shell may not open TCP
//! connections cannot reach the Session's apps through them either.
//!
//! Under `network: egress` the Session container's own bridge (PID 1)
//! already serves each exposed port in that volume for Preview, so no
//! forwarder starts; the browser checks that those sockets answer.
//!
//! Scripts run through the supervisor's `--serve`, like repository tools.

use std::path::PathBuf;
use std::time::Duration;

use axocoatl_core::SecureDir;
use axocoatl_exec::protocol::{
    ExecRequest, ProcessOutcome, ServerMessage, StdinDescriptor, PROTOCOL_VERSION,
};
use tokio::process::Command;

use crate::supervisor_image::SupervisorImage;
use crate::supervisor_program::{SupervisorProgram, SUPERVISOR_CONTAINER_PATH};
use crate::{IsolationError, SessionSandbox};

pub use crate::egress_image::ROLE_LABEL;
// The service-socket and egress socket mount points are the egress
// sidecar's. Under `bridge` and `none` the Session container never mounts
// the service sockets.
pub use crate::egress_sidecar::{EGRESS_PROXY_SOCKET, EGRESS_SOCKET_DIR, SERVICE_SOCKET_DIR};
/// The loopback port Chromium finds the egress proxy on, unless the Session
/// exposes it (see [`browser_proxy_port`]).
pub const DEFAULT_PROXY_PORT: u16 = 3128;
pub const BROWSER_ROLE: &str = "browser";
pub const SERVICE_SOCKETS_ROLE: &str = "service-sockets";
pub const EGRESS_ROLE: &str = "egress";
/// The ports a service forwarder serves, comma separated.
pub const SERVICE_PORTS_LABEL: &str = "io.axocoatl.service-ports";
/// The start time (Unix nanoseconds) of the Session container a service
/// forwarder joined; a restarted Session container has a new namespace.
pub const SESSION_STARTED_LABEL: &str = "io.axocoatl.session-started";
use crate::session_sandbox::RUNTIME_AUTHORITY_LABEL;
const DEFAULT_USER: &str = "1000:1000";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const START_TIMEOUT: Duration = Duration::from_secs(120);
const PULL_TIMEOUT: Duration = Duration::from_secs(600);
const STDOUT_BYTES: usize = 4 * 1024 * 1024;
const STDERR_BYTES: usize = 64 * 1024;

fn failed(message: impl std::fmt::Display) -> IsolationError {
    IsolationError::OciContainerFailed(format!("browser container: {message}"))
}

pub fn browser_container_name(session_id: &str) -> String {
    format!("axo-brw-{session_id}")
}

/// The service-socket volume. The service forwarder's container has the same
/// name, as the egress sidecar shares its volume's.
pub fn service_socket_volume(session_id: &str) -> String {
    crate::egress_sidecar::service_volume_name(session_id)
}

pub fn service_forwarder_name(session_id: &str) -> String {
    service_socket_volume(session_id)
}

/// The egress sidecar's container and socket volume share this name.
pub fn egress_volume(session_id: &str) -> String {
    crate::egress_sidecar::egress_volume_name(session_id)
}

pub fn service_socket_path(port: u16) -> String {
    format!("{SERVICE_SOCKET_DIR}/{port}.sock")
}

/// The loopback port the browser container's proxy listener uses: 3128, or
/// the next port the Session does not expose, so an app on 3128 is never
/// mistaken for the proxy.
pub fn browser_proxy_port(exposed_ports: &[u16]) -> u16 {
    (DEFAULT_PROXY_PORT..=u16::MAX)
        .find(|port| !exposed_ports.contains(port))
        .unwrap_or(DEFAULT_PROXY_PORT)
}

async fn podman(
    args: &[String],
    timeout: Duration,
) -> Result<crate::session_sandbox::BoundedCommandOutput, IsolationError> {
    let mut command = Command::new("podman");
    command.args(args);
    SessionSandbox::run_bounded_command(command, timeout).await
}

fn stderr_text(output: &crate::session_sandbox::BoundedCommandOutput) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

/// Create a volume carrying Axocoatl's labels unless it exists.
pub(crate) async fn create_owned_volume(
    name: &str,
    runtime_authority: Option<&str>,
    role: &str,
) -> Result<(), IsolationError> {
    let mut args: Vec<String> = vec!["volume".into(), "create".into(), "--ignore".into()];
    if let Some(authority) = runtime_authority {
        args.push("--label".into());
        args.push(format!("{RUNTIME_AUTHORITY_LABEL}={authority}"));
    }
    args.push("--label".into());
    args.push(format!("{ROLE_LABEL}={role}"));
    args.push(name.into());
    let output = podman(&args, COMMAND_TIMEOUT).await?;
    if output.timed_out || !output.status.success() {
        return Err(failed(format!(
            "creating volume {name}: {}",
            stderr_text(&output)
        )));
    }
    Ok(())
}

/// Everything `podman run` needs for one Session's service forwarder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceForwarderSpec {
    pub session_id: String,
    pub runtime_authority: String,
    /// The scratch egress image tag (only the static supervisor).
    pub image: String,
    /// The exact Session container whose network namespace it joins.
    pub session_container_id: String,
    /// That container's start time, in Unix nanoseconds.
    pub session_started: String,
    pub ports: Vec<u16>,
    pub with_limits: bool,
    pub labels: Vec<(String, String)>,
}

fn ports_label(ports: &[u16]) -> String {
    ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// The `podman run` arguments for the service forwarder (pure). It joins
/// the Session container's network namespace and nothing else of it: no
/// Workspace, no environment, no published port. The socket volume is
/// writable here and nowhere else.
pub fn build_forwarder_args(spec: &ServiceForwarderSpec) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        service_forwarder_name(&spec.session_id),
        "--pull=never".into(),
        "--label".into(),
        format!("{RUNTIME_AUTHORITY_LABEL}={}", spec.runtime_authority),
        "--label".into(),
        format!("{ROLE_LABEL}={SERVICE_SOCKETS_ROLE}"),
        "--label".into(),
        format!("{SERVICE_PORTS_LABEL}={}", ports_label(&spec.ports)),
        "--label".into(),
        format!("{SESSION_STARTED_LABEL}={}", spec.session_started),
    ];
    for (key, value) in &spec.labels {
        args.push("--label".into());
        args.push(format!("{key}={value}"));
    }
    args.push("--network".into());
    args.push(format!("container:{}", spec.session_container_id));
    args.extend(
        [
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--http-proxy=false",
            "--no-healthcheck",
            "--image-volume=ignore",
            "--user",
            "0:0",
        ]
        .map(String::from),
    );
    if spec.with_limits {
        args.extend(["--memory", "128m", "--pids-limit", "512"].map(String::from));
    }
    args.push("--mount".into());
    args.push(format!(
        "type=volume,source={},destination={SERVICE_SOCKET_DIR}",
        service_socket_volume(&spec.session_id)
    ));
    args.push("--entrypoint".into());
    args.push(SUPERVISOR_CONTAINER_PATH.into());
    args.push(spec.image.clone());
    args.push("--bridge".into());
    for port in &spec.ports {
        args.push("--unix-to-tcp".into());
        args.push(format!("{}=127.0.0.1:{port}", service_socket_path(*port)));
    }
    args
}

/// How to make sure one Session's ports are served as sockets.
#[derive(Debug, Clone)]
pub struct ServiceSocketsLaunch {
    pub session_id: String,
    pub runtime_authority: String,
    /// The scratch egress image tag, from
    /// [`crate::egress_image::ensure_egress_image`].
    pub image: String,
    pub ports: Vec<u16>,
    pub require_resource_limits: bool,
    pub labels: Vec<(String, String)>,
}

/// One line per template field of `podman container inspect`, or `None`
/// when the container does not exist.
async fn inspect_lines(
    container: &str,
    format: &str,
) -> Result<Option<Vec<String>>, IsolationError> {
    let output = podman(
        &[
            "container".into(),
            "inspect".into(),
            "--format".into(),
            format.into(),
            "--".into(),
            container.into(),
        ],
        COMMAND_TIMEOUT,
    )
    .await?;
    if output.timed_out {
        return Err(failed(format!("inspecting {container} timed out")));
    }
    if !output.status.success() {
        let detail = stderr_text(&output);
        if detail.contains("no such container") {
            return Ok(None);
        }
        return Err(failed(format!("inspecting {container}: {detail}")));
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.trim().to_string())
            .collect(),
    ))
}

async fn probe_socket(container: &str, port: u16) -> Result<bool, IsolationError> {
    let output = podman(
        &[
            "exec".into(),
            container.into(),
            SUPERVISOR_CONTAINER_PATH.into(),
            "--probe-unix".into(),
            service_socket_path(port),
        ],
        COMMAND_TIMEOUT,
    )
    .await?;
    Ok(!output.timed_out && output.status.success())
}

async fn remove_forwarder(name: &str) -> Result<(), IsolationError> {
    let output = podman(
        &[
            "rm".into(),
            "--force".into(),
            "--time".into(),
            "0".into(),
            "--ignore".into(),
            name.into(),
        ],
        COMMAND_TIMEOUT,
    )
    .await?;
    if output.timed_out || !output.status.success() {
        return Err(failed(format!(
            "removing the service forwarder: {}",
            stderr_text(&output)
        )));
    }
    Ok(())
}

/// Under `network: egress`: check that the running Session container's own
/// bridge (PID 1), which serves each exposed port as a socket in the
/// `axo-svc-{session}` volume for Preview, answers on every one of `ports`.
/// No forwarder is started: a second bridge would replace those sockets.
pub async fn check_session_served_sockets(
    session_id: &str,
    ports: &[u16],
) -> Result<(), IsolationError> {
    let ports = dedup_ports(ports);
    if ports.is_empty() {
        return Ok(());
    }
    let session = format!("axo-ses-{session_id}");
    match inspect_lines(&session, "{{.State.Running}}").await? {
        Some(state) if state.first().is_some_and(|running| running == "true") => {}
        _ => return Err(failed("the Session container is not running")),
    }
    let mut missing = Vec::new();
    for port in &ports {
        if !probe_socket(&session, *port).await? {
            missing.push(port.to_string());
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(failed(format!(
            "the Session's bridge is not serving the socket for port(s) {}",
            missing.join(", ")
        )))
    }
}

/// Make sure each exposed port of the running Session container is served
/// as a socket in its `axo-svc-{session}` volume, by a service forwarder
/// joined to that exact container's network namespace. A forwarder that
/// stopped, serves other ports or joined an earlier start of the Session
/// container is replaced. Returns the forwarder's container id.
pub async fn ensure_service_sockets(
    launch: &ServiceSocketsLaunch,
) -> Result<Option<String>, IsolationError> {
    let ports = dedup_ports(&launch.ports);
    if ports.is_empty() {
        return Ok(None);
    }
    let session = format!("axo-ses-{}", launch.session_id);
    let Some(state) = inspect_lines(
        &session,
        "{{.Id}}\n{{.State.Running}}\n{{.State.StartedAt.UnixNano}}",
    )
    .await?
    else {
        return Err(failed("the Session container is not running"));
    };
    let (session_id, running, started) = match state.as_slice() {
        [id, running, started, ..] => (id.clone(), running == "true", started.clone()),
        _ => return Err(failed("Podman did not describe the Session container")),
    };
    if !running {
        return Err(failed("the Session container is not running"));
    }
    let name = service_forwarder_name(&launch.session_id);
    let expected_network = format!("container:{session_id}");
    let wanted_ports = ports_label(&ports);
    let current = inspect_lines(
        &name,
        &format!(
            "{{{{.Id}}}}\n{{{{.State.Running}}}}\n{{{{.HostConfig.NetworkMode}}}}\n{{{{index .Config.Labels \"{SERVICE_PORTS_LABEL}\"}}}}\n{{{{index .Config.Labels \"{SESSION_STARTED_LABEL}\"}}}}"
        ),
    )
    .await?;
    if let Some(current) = &current {
        if let [id, running, network, served, joined, ..] = current.as_slice() {
            if running == "true"
                && *network == expected_network
                && *served == wanted_ports
                && *joined == started
                && probe_socket(&name, ports[0]).await?
            {
                return Ok(Some(id.clone()));
            }
        }
        remove_forwarder(&name).await?;
    }
    create_owned_volume(
        &service_socket_volume(&launch.session_id),
        Some(launch.runtime_authority.as_str()),
        SERVICE_SOCKETS_ROLE,
    )
    .await?;
    let mut spec = ServiceForwarderSpec {
        session_id: launch.session_id.clone(),
        runtime_authority: launch.runtime_authority.clone(),
        image: launch.image.clone(),
        session_container_id: session_id,
        session_started: started,
        ports: ports.clone(),
        with_limits: true,
        labels: launch.labels.clone(),
    };
    let id = loop {
        let output = podman(&build_forwarder_args(&spec), START_TIMEOUT).await?;
        if !output.timed_out && output.status.success() {
            break String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
        let detail = stderr_text(&output);
        let _ = remove_forwarder(&name).await;
        if spec.with_limits && detail.contains("cgroup") && !launch.require_resource_limits {
            tracing::warn!(
                "this host cannot apply container resource limits; starting the service forwarder without them"
            );
            spec.with_limits = false;
            continue;
        }
        return Err(failed(format!("starting the service forwarder: {detail}")));
    };
    for _ in 0..30 {
        if probe_socket(&name, ports[0]).await? {
            return Ok(Some(id));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let logs = podman(
        &["logs".into(), "--tail".into(), "20".into(), name.clone()],
        COMMAND_TIMEOUT,
    )
    .await
    .map(|output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
    .unwrap_or_default();
    let _ = remove_forwarder(&name).await;
    Err(failed(format!(
        "the service forwarder did not open its sockets: {}",
        logs.trim()
    )))
}

/// Everything `podman run` needs for one browser container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserContainerSpec {
    pub session_id: String,
    pub runtime_authority: String,
    /// The immutable image id.
    pub image: String,
    /// `--user`: the image's own non-root user, or 1000:1000.
    pub user: String,
    pub exposed_ports: Vec<u16>,
    /// Mount the egress socket volume and listen on the loopback proxy port
    /// ([`browser_proxy_port`]).
    pub egress: bool,
    /// Host path of the supervisor for the image's architecture.
    pub supervisor_path: PathBuf,
    pub with_limits: bool,
    pub labels: Vec<(String, String)>,
}

/// The `podman run` arguments for the browser container (pure).
pub fn build_browser_args(spec: &BrowserContainerSpec) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        browser_container_name(&spec.session_id),
        "--pull=never".into(),
        "--label".into(),
        format!("{RUNTIME_AUTHORITY_LABEL}={}", spec.runtime_authority),
        "--label".into(),
        format!("{ROLE_LABEL}={BROWSER_ROLE}"),
    ];
    for (key, value) in &spec.labels {
        args.push("--label".into());
        args.push(format!("{key}={value}"));
    }
    args.extend(
        [
            "--network",
            "none",
            "--read-only",
            "--tmpfs",
            "/tmp:rw,nosuid,nodev,size=1g",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--http-proxy=false",
            "--no-healthcheck",
            "--image-volume=ignore",
        ]
        .map(String::from),
    );
    args.push("--user".into());
    args.push(spec.user.clone());
    args.push("--env".into());
    args.push("HOME=/tmp".into());
    if spec.with_limits {
        args.extend(["--memory", "2g", "--cpus", "2", "--pids-limit", "512"].map(String::from));
    }
    args.push("--mount".into());
    args.push(format!(
        "type=volume,source={},destination={SERVICE_SOCKET_DIR},ro=true",
        service_socket_volume(&spec.session_id)
    ));
    if spec.egress {
        args.push("--mount".into());
        args.push(format!(
            "type=volume,source={},destination={EGRESS_SOCKET_DIR},ro=true",
            egress_volume(&spec.session_id)
        ));
    }
    args.push("--mount".into());
    args.push(format!(
        "type=bind,source={},destination={SUPERVISOR_CONTAINER_PATH},ro=true",
        spec.supervisor_path.display()
    ));
    args.push("--entrypoint".into());
    args.push(SUPERVISOR_CONTAINER_PATH.into());
    args.push(spec.image.clone());
    args.push("--bridge".into());
    if spec.egress {
        args.push("--tcp-to-unix".into());
        args.push(format!(
            "127.0.0.1:{}={EGRESS_PROXY_SOCKET}",
            browser_proxy_port(&spec.exposed_ports)
        ));
        args.push("--http-errors".into());
    }
    for port in &spec.exposed_ports {
        for listen in [format!("127.0.0.1:{port}"), format!("[::1]:{port}")] {
            args.push("--tcp-to-unix".into());
            args.push(format!("{listen}={}", service_socket_path(*port)));
        }
    }
    args
}

/// How to start one Session's browser container.
#[derive(Debug, Clone)]
pub struct BrowserLaunch {
    pub session_id: String,
    pub runtime_authority: String,
    /// The configured image reference.
    pub image: String,
    pub exposed_ports: Vec<u16>,
    pub egress: bool,
    /// The private directory supervisors are installed in.
    pub supervisor_installation: SecureDir,
    pub require_resource_limits: bool,
    pub labels: Vec<(String, String)>,
}

/// What one script run produced.
#[derive(Debug, Clone, Default)]
pub struct ScriptOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// A running browser container.
#[derive(Debug)]
pub struct BrowserContainer {
    session_id: String,
    container_id: String,
    spec: BrowserContainerSpec,
    supervisor: SupervisorProgram,
}

async fn image_user(image_id: &str) -> Result<String, IsolationError> {
    let output = podman(
        &[
            "image".into(),
            "inspect".into(),
            "--format".into(),
            "{{.Config.User}}".into(),
            "--".into(),
            image_id.into(),
        ],
        COMMAND_TIMEOUT,
    )
    .await?;
    if output.timed_out || !output.status.success() {
        return Err(failed(format!(
            "inspecting the browser image: {}",
            stderr_text(&output)
        )));
    }
    let user = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(browser_user(&user))
}

/// The image's own user when it is not root, else 1000:1000.
pub fn browser_user(image_user: &str) -> String {
    let name = image_user.split(':').next().unwrap_or_default();
    if name.is_empty() || name == "root" || name == "0" {
        DEFAULT_USER.into()
    } else {
        image_user.into()
    }
}

async fn image_present(image: &str) -> Result<bool, IsolationError> {
    let output = podman(
        &["image".into(), "exists".into(), "--".into(), image.into()],
        COMMAND_TIMEOUT,
    )
    .await?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(failed(format!(
            "checking the browser image: {}",
            stderr_text(&output)
        ))),
    }
}

impl BrowserContainer {
    /// Start the Session's browser container, replacing any stale one.
    pub async fn start(launch: &BrowserLaunch) -> Result<Self, IsolationError> {
        crate::podman::ensure_ready().await?;
        if launch.image.starts_with("localhost/") && !image_present(&launch.image).await? {
            return Err(failed(format!(
                "the browser image {} is not built; run `axocoatl browser install`",
                launch.image
            )));
        }
        let selected = tokio::time::timeout(PULL_TIMEOUT, SupervisorImage::resolve(&launch.image))
            .await
            .map_err(|_| failed("resolving the browser image timed out"))??;
        let supervisor = SupervisorProgram::install_embedded_async(
            selected.architecture(),
            &launch.supervisor_installation,
        )
        .await?;
        let user = image_user(selected.id()).await?;
        let name = browser_container_name(&launch.session_id);
        let removed = podman(
            &[
                "rm".into(),
                "--force".into(),
                "--time".into(),
                "0".into(),
                "--ignore".into(),
                name.clone(),
            ],
            COMMAND_TIMEOUT,
        )
        .await?;
        if removed.timed_out || !removed.status.success() {
            return Err(failed(format!(
                "removing a stale browser container: {}",
                stderr_text(&removed)
            )));
        }
        let mut spec = BrowserContainerSpec {
            session_id: launch.session_id.clone(),
            runtime_authority: launch.runtime_authority.clone(),
            image: selected.id().to_string(),
            user,
            exposed_ports: dedup_ports(&launch.exposed_ports),
            egress: launch.egress,
            supervisor_path: supervisor.path().to_path_buf(),
            with_limits: true,
            labels: launch.labels.clone(),
        };
        let container_id = loop {
            supervisor.verify()?;
            let output = podman(&build_browser_args(&spec), START_TIMEOUT).await?;
            if !output.timed_out && output.status.success() {
                let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if id.is_empty() {
                    return Err(failed("Podman reported success without a container id"));
                }
                break id;
            }
            let detail = stderr_text(&output);
            let _ = podman(
                &[
                    "rm".into(),
                    "--force".into(),
                    "--time".into(),
                    "0".into(),
                    "--ignore".into(),
                    name.clone(),
                ],
                COMMAND_TIMEOUT,
            )
            .await;
            if spec.with_limits && detail.contains("cgroup") && !launch.require_resource_limits {
                tracing::warn!(
                    "this host cannot apply container resource limits; starting the browser container without them"
                );
                spec.with_limits = false;
                continue;
            }
            return Err(failed(format!("starting {name}: {detail}")));
        };
        let container = Self {
            session_id: launch.session_id.clone(),
            container_id,
            spec,
            supervisor,
        };
        let verified = async {
            selected
                .verify_container(&container.container_id, &container.supervisor)
                .await?;
            // A bridge that cannot open its listeners exits at once.
            tokio::time::sleep(Duration::from_millis(300)).await;
            if !container.is_running().await {
                let logs = podman(
                    &[
                        "logs".into(),
                        "--tail".into(),
                        "20".into(),
                        container.container_id.clone(),
                    ],
                    COMMAND_TIMEOUT,
                )
                .await
                .map(|output| {
                    format!(
                        "{}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    )
                })
                .unwrap_or_default();
                return Err(failed(format!(
                    "the browser container stopped right after starting: {}",
                    logs.trim()
                )));
            }
            Ok(())
        }
        .await;
        if let Err(error) = verified {
            let _ = container.remove().await;
            return Err(error);
        }
        Ok(container)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn container_id(&self) -> &str {
        &self.container_id
    }

    pub fn spec(&self) -> &BrowserContainerSpec {
        &self.spec
    }

    /// Whether this container was started for these ports and egress mode.
    pub fn serves(&self, ports: &[u16], egress: bool) -> bool {
        self.spec.exposed_ports == dedup_ports(ports) && self.spec.egress == egress
    }

    pub async fn is_running(&self) -> bool {
        podman(
            &[
                "container".into(),
                "inspect".into(),
                "--format".into(),
                "{{.State.Running}}".into(),
                "--".into(),
                self.container_id.clone(),
            ],
            COMMAND_TIMEOUT,
        )
        .await
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
        })
    }

    /// Run `node --input-type=module -e <script>` under the supervisor with
    /// `stdin` as its input. Only the input's digest enters the request.
    pub async fn run_script(
        &self,
        invocation_id: &str,
        script: &str,
        stdin: Vec<u8>,
        timeout: Duration,
    ) -> Result<ScriptOutput, IsolationError> {
        self.supervisor.verify()?;
        let request = ExecRequest {
            protocol: PROTOCOL_VERSION,
            invocation_id: invocation_id.chars().take(128).collect(),
            argv: vec![
                "node".into(),
                "--input-type=module".into(),
                "-e".into(),
                script.into(),
            ],
            stdin: Some(StdinDescriptor::for_bytes(&stdin).map_err(failed)?),
            timeout_ms: u64::try_from(timeout.as_millis()).map_err(failed)?,
            stdout_bytes: STDOUT_BYTES,
            stderr_bytes: STDERR_BYTES,
            write_restriction: None,
        };
        request.validate().map_err(failed)?;
        let mut command = Command::new("podman");
        command.args([
            "exec",
            "-i",
            "-w",
            "/tmp",
            &self.container_id,
            SUPERVISOR_CONTAINER_PATH,
            "--serve",
        ]);
        let prepared = crate::supervisor_transport::prepare_command_with_stdin(
            command,
            request,
            Some(std::sync::Arc::from(stdin)),
            self.container_id.clone(),
            self.supervisor.sha256().to_owned(),
        )
        .await?;
        let execution = prepared.dispatch()?.finish().await?;
        execution
            .result()
            .validate_for(execution.request())
            .map_err(failed)?;
        let ServerMessage::Finished {
            outcome,
            stdout,
            stderr,
            ..
        } = execution.result()
        else {
            return Err(failed("the script has no terminal result"));
        };
        let stdout = stdout.retained_bytes(STDOUT_BYTES).map_err(failed)?;
        let stderr = String::from_utf8_lossy(&stderr.retained_bytes(STDERR_BYTES).map_err(failed)?)
            .into_owned();
        let exit_code = match outcome {
            ProcessOutcome::Exited { code } => Some(*code),
            ProcessOutcome::Signalled { .. } => None,
            ProcessOutcome::TimedOut => {
                return Err(failed(format!(
                    "the call ran out of time after {} s",
                    timeout.as_secs()
                )))
            }
            ProcessOutcome::Cancelled => return Err(failed("the call was cancelled")),
            ProcessOutcome::LaunchFailed { message } | ProcessOutcome::Failed { message } => {
                return Err(failed(message))
            }
        };
        Ok(ScriptOutput {
            exit_code,
            stdout,
            stderr,
        })
    }

    /// Remove this exact container.
    pub async fn remove(&self) -> Result<(), IsolationError> {
        let output = podman(
            &[
                "rm".into(),
                "--force".into(),
                "--time".into(),
                "0".into(),
                "--ignore".into(),
                self.container_id.clone(),
            ],
            COMMAND_TIMEOUT,
        )
        .await?;
        if output.timed_out || !output.status.success() {
            return Err(failed(format!(
                "removing the browser container: {}",
                stderr_text(&output)
            )));
        }
        Ok(())
    }
}

fn dedup_ports(ports: &[u16]) -> Vec<u16> {
    let mut seen = std::collections::BTreeSet::new();
    ports
        .iter()
        .copied()
        .filter(|port| *port != 0 && seen.insert(*port))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(egress: bool) -> BrowserContainerSpec {
        BrowserContainerSpec {
            session_id: "ses-1".into(),
            runtime_authority: "authority".into(),
            image: "b".repeat(64),
            user: "node".into(),
            exposed_ports: vec![5173, 8765],
            egress,
            supervisor_path: PathBuf::from("/data/execution-supervisors/supervisor-sha256-x"),
            with_limits: true,
            labels: vec![("io.axocoatl.test".into(), "browser-1".into())],
        }
    }

    #[test]
    fn browser_arguments_isolate_the_container() {
        let args = build_browser_args(&spec(false));
        let joined = args.join(" ");
        assert!(args.windows(2).any(|pair| pair == ["--network", "none"]));
        for required in [
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--http-proxy=false",
            "--no-healthcheck",
            "--image-volume=ignore",
            "--pull=never",
        ] {
            assert!(args.contains(&required.to_string()), "{required}");
        }
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--name", "axo-brw-ses-1"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--label", "io.axocoatl.role=browser"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--label", "io.axocoatl.test=browser-1"]));
        assert!(args.windows(2).any(|pair| pair == ["--user", "node"]));
        // HOME is the only environment variable.
        let envs: Vec<&String> = args
            .windows(2)
            .filter(|pair| pair[0] == "--env" || pair[0] == "-e")
            .map(|pair| &pair[1])
            .collect();
        assert_eq!(envs, vec!["HOME=/tmp"]);
        assert!(!args
            .iter()
            .any(|arg| arg == "-p" || arg.starts_with("--publish")));
        assert!(!args.iter().any(|arg| arg == "-v" || arg == "--volume"));
        assert!(!joined.contains("/workspace") && !joined.contains("/Users/"));
        assert!(joined
            .contains("type=volume,source=axo-svc-ses-1,destination=/run/axocoatl-svc,ro=true"));
        assert!(!joined.contains("axo-egr-"));
        assert!(!joined.contains("3128"));
        assert!(joined.contains(
            "type=bind,source=/data/execution-supervisors/supervisor-sha256-x,destination=/axocoatl-exec-supervisor,ro=true"
        ));
        let bridge = &args[args.iter().position(|arg| arg == "--bridge").unwrap()..];
        assert_eq!(
            bridge,
            [
                "--bridge",
                "--tcp-to-unix",
                "127.0.0.1:5173=/run/axocoatl-svc/5173.sock",
                "--tcp-to-unix",
                "[::1]:5173=/run/axocoatl-svc/5173.sock",
                "--tcp-to-unix",
                "127.0.0.1:8765=/run/axocoatl-svc/8765.sock",
                "--tcp-to-unix",
                "[::1]:8765=/run/axocoatl-svc/8765.sock",
            ]
        );
        let image = args.iter().position(|arg| arg == &"b".repeat(64)).unwrap();
        assert_eq!(args[image - 1], "/axocoatl-exec-supervisor");
        assert_eq!(args[image - 2], "--entrypoint");
        assert!(joined.contains("--memory 2g --cpus 2 --pids-limit 512"));
    }

    #[test]
    fn declared_hosts_add_the_read_only_egress_socket_and_the_proxy_listener() {
        let mut with_egress = spec(true);
        with_egress.with_limits = false;
        let args = build_browser_args(&with_egress);
        let joined = args.join(" ");
        assert!(joined
            .contains("type=volume,source=axo-egr-ses-1,destination=/run/axocoatl-egress,ro=true"));
        assert!(joined.contains(
            "--bridge --tcp-to-unix 127.0.0.1:3128=/run/axocoatl-egress/proxy.sock --http-errors --tcp-to-unix 127.0.0.1:5173"
        ));
        assert!(!joined.contains("--memory"));
    }

    #[test]
    fn the_forwarder_joins_the_session_namespace_and_alone_writes_the_sockets() {
        let spec = ServiceForwarderSpec {
            session_id: "ses-1".into(),
            runtime_authority: "authority".into(),
            image: "localhost/axocoatl-egress:866a01dbe5366107-aarch64".into(),
            session_container_id: "c".repeat(64),
            session_started: "1790998455827417067".into(),
            ports: vec![3000, 8765],
            with_limits: true,
            labels: vec![("io.axocoatl.test".into(), "browser-1".into())],
        };
        let args = build_forwarder_args(&spec);
        let joined = args.join(" ");
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--name", "axo-svc-ses-1"]));
        assert!(args.windows(2).any(|pair| pair
            == [
                "--network".to_string(),
                format!("container:{}", "c".repeat(64))
            ]));
        for required in [
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--http-proxy=false",
            "--pull=never",
            "--image-volume=ignore",
        ] {
            assert!(args.contains(&required.to_string()), "{required}");
        }
        for label in [
            "io.axocoatl.role=service-sockets",
            "io.axocoatl.runtime-authority=authority",
            "io.axocoatl.service-ports=3000,8765",
            "io.axocoatl.session-started=1790998455827417067",
            "io.axocoatl.test=browser-1",
        ] {
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == "--label" && pair[1] == label),
                "{label}"
            );
        }
        // The socket volume is writable only here; no Workspace, environment
        // or published port.
        assert!(joined
            .contains("--mount type=volume,source=axo-svc-ses-1,destination=/run/axocoatl-svc "));
        assert!(!joined.contains("ro=true"));
        for refused in ["-e", "--env", "-v", "--volume", "-p", "--publish"] {
            assert!(!args.contains(&refused.to_string()), "{refused}");
        }
        assert!(!joined.contains("type=bind"));
        let bridge = &args[args.iter().position(|arg| arg == "--bridge").unwrap()..];
        assert_eq!(
            bridge,
            [
                "--bridge",
                "--unix-to-tcp",
                "/run/axocoatl-svc/3000.sock=127.0.0.1:3000",
                "--unix-to-tcp",
                "/run/axocoatl-svc/8765.sock=127.0.0.1:8765",
            ]
        );
        assert_eq!(
            args[args.len() - 6],
            "localhost/axocoatl-egress:866a01dbe5366107-aarch64"
        );
        assert!(joined.contains("--memory 128m --pids-limit 512"));
        let unlimited = build_forwarder_args(&ServiceForwarderSpec {
            with_limits: false,
            ..spec
        });
        assert!(!unlimited.contains(&"--memory".to_string()));
    }

    #[test]
    fn names_users_and_ports() {
        assert_eq!(browser_user(""), "1000:1000");
        assert_eq!(browser_user("root"), "1000:1000");
        assert_eq!(browser_user("0:0"), "1000:1000");
        assert_eq!(browser_user("node"), "node");
        assert_eq!(browser_user("1001:1001"), "1001:1001");
        assert_eq!(dedup_ports(&[8765, 0, 8765, 3000]), vec![8765, 3000]);
        assert_eq!(browser_container_name("s"), "axo-brw-s");
        assert_eq!(egress_volume("s"), "axo-egr-s");
        assert_eq!(service_socket_volume("s"), "axo-svc-s");
        assert_eq!(service_forwarder_name("s"), "axo-svc-s");
        // Each of them is removed after the Session's container.
        for name in [
            browser_container_name("s"),
            egress_volume("s"),
            service_forwarder_name("s"),
        ] {
            assert!(crate::session_sandbox::DEPENDENT_CONTAINER_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix)));
        }
        // The proxy listener never takes an exposed port.
        assert_eq!(browser_proxy_port(&[]), 3128);
        assert_eq!(browser_proxy_port(&[5173, 8765]), 3128);
        assert_eq!(browser_proxy_port(&[3128]), 3129);
        assert_eq!(browser_proxy_port(&[3129, 3128, 3130]), 3131);
    }

    #[test]
    fn an_exposed_port_3128_is_an_app_not_the_proxy() {
        let mut with_app = spec(true);
        with_app.exposed_ports = vec![3128];
        let args = build_browser_args(&with_app);
        let bridge = &args[args.iter().position(|arg| arg == "--bridge").unwrap()..];
        assert_eq!(
            bridge,
            [
                "--bridge",
                "--tcp-to-unix",
                "127.0.0.1:3129=/run/axocoatl-egress/proxy.sock",
                "--http-errors",
                "--tcp-to-unix",
                "127.0.0.1:3128=/run/axocoatl-svc/3128.sock",
                "--tcp-to-unix",
                "[::1]:3128=/run/axocoatl-svc/3128.sock",
            ]
        );
        // Without declared hosts nothing listens for a proxy at all.
        with_app.egress = false;
        let args = build_browser_args(&with_app);
        assert!(!args
            .iter()
            .any(|arg| arg.contains("proxy.sock") || arg.contains(":3129")));
    }
}
