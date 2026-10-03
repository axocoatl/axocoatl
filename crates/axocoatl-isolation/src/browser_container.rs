//! The browser container and the Session's service sockets.
//!
//! Each Session that uses the browser tools gets one browser container,
//! `axo-brw-{session}`, started on the first call and removed with the
//! Session. It has no network interface but loopback (`--network none`), a
//! read-only root, no capabilities, and no Workspace mount. Its PID 1 is the
//! execution supervisor's `--bridge`:
//!
//! - for each exposed Session port it listens on `127.0.0.1:{p}` and
//!   `[::1]:{p}` and forwards to `/run/axocoatl-svc/{p}.sock`, a socket the
//!   Session container serves from its own `127.0.0.1:{p}`. Any other
//!   `localhost` port refuses in the kernel.
//! - with declared hosts it also listens on `127.0.0.1:3128` and forwards to
//!   the egress proxy's socket, so Chromium reaches those hosts only through
//!   the daemon's egress policy.
//!
//! Scripts run through the supervisor's `--serve`, like repository tools.

use std::path::PathBuf;
use std::time::Duration;

use axocoatl_core::SecureDir;
use axocoatl_exec::protocol::{
    ExecRequest, ProcessOutcome, ServerMessage, StdinDescriptor, PROTOCOL_VERSION,
};
use serde::Deserialize;
use tokio::process::Command;

use crate::supervisor_image::SupervisorImage;
use crate::supervisor_program::{SupervisorProgram, SUPERVISOR_CONTAINER_PATH};
use crate::{IsolationError, SessionSandbox};

/// Where the Session container serves its ports as Unix sockets.
pub const SERVICE_SOCKET_DIR: &str = "/run/axocoatl-svc";
/// Where the egress sidecar's socket volume is mounted.
pub const EGRESS_SOCKET_DIR: &str = "/run/axocoatl-egress";
pub const EGRESS_PROXY_SOCKET: &str = "/run/axocoatl-egress/proxy.sock";
/// Where Chromium finds the egress proxy inside the browser container.
pub const BROWSER_PROXY_LISTEN: &str = "127.0.0.1:3128";
pub const ROLE_LABEL: &str = "io.axocoatl.role";
pub const BROWSER_ROLE: &str = "browser";
pub const SERVICE_SOCKETS_ROLE: &str = "service-sockets";
pub const EGRESS_ROLE: &str = "egress";
/// Containers that serve exactly one Session and are removed with it.
pub const COMPANION_PREFIXES: [&str; 2] = ["axo-brw-", "axo-egr-"];
const RUNTIME_AUTHORITY_LABEL: &str = "io.axocoatl.runtime-authority";
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

pub fn service_socket_volume(session_id: &str) -> String {
    format!("axo-svc-{session_id}")
}

/// The egress sidecar's container and socket volume share this name.
pub fn egress_volume(session_id: &str) -> String {
    format!("axo-egr-{session_id}")
}

pub fn companion_containers(session_id: &str) -> Vec<String> {
    vec![
        browser_container_name(session_id),
        egress_volume(session_id),
    ]
}

pub fn companion_volumes(session_id: &str) -> Vec<String> {
    vec![service_socket_volume(session_id), egress_volume(session_id)]
}

pub fn service_socket_path(port: u16) -> String {
    format!("{SERVICE_SOCKET_DIR}/{port}.sock")
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

/// Remove volumes; an absent one is not an error. A volume still in use is.
pub async fn remove_volumes_if_present(
    names: &[String],
    timeout: Duration,
) -> Result<(), IsolationError> {
    for name in names {
        let output = podman(&["volume".into(), "rm".into(), name.clone()], timeout).await?;
        if output.status.success() {
            continue;
        }
        let detail = stderr_text(&output);
        if !output.timed_out && detail.contains("no such volume") {
            continue;
        }
        return Err(failed(format!("removing volume {name}: {detail}")));
    }
    Ok(())
}

/// The forwarder's arguments in the Session container: one Unix socket per
/// exposed port, each forwarding to the app on `127.0.0.1:{p}` (pure).
pub fn service_forwarder_args(ports: &[u16]) -> Vec<String> {
    let mut args = vec!["--bridge".to_string()];
    for port in ports {
        args.push("--unix-to-tcp".into());
        args.push(format!("{}=127.0.0.1:{port}", service_socket_path(*port)));
    }
    args
}

#[derive(Deserialize)]
struct MountInspection {
    #[serde(rename = "Type")]
    kind: String,
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "Destination")]
    destination: String,
}

async fn probe_socket(container: &str, port: u16) -> Result<bool, IsolationError> {
    let output = podman(
        &[
            "exec".into(),
            "--user".into(),
            "0".into(),
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

/// Make sure the Session container serves each exposed port as a socket in
/// its `axo-svc-{session}` volume. A forwarder the Session's own processes
/// stopped is started again; it only ever connects to the Session's
/// loopback, so stopping it only blocks the browser's view of that app.
pub async fn ensure_service_sockets(session_id: &str, ports: &[u16]) -> Result<(), IsolationError> {
    if ports.is_empty() {
        return Ok(());
    }
    let container = format!("axo-ses-{session_id}");
    let volume = service_socket_volume(session_id);
    let output = podman(
        &[
            "container".into(),
            "inspect".into(),
            "--format".into(),
            "{{json .Mounts}}".into(),
            "--".into(),
            container.clone(),
        ],
        COMMAND_TIMEOUT,
    )
    .await?;
    if output.timed_out || !output.status.success() {
        return Err(failed(format!(
            "the Session container is not running: {}",
            stderr_text(&output)
        )));
    }
    let mounts: Vec<MountInspection> =
        serde_json::from_slice(&output.stdout).map_err(|error| failed(error.to_string()))?;
    if !mounts.iter().any(|mount| {
        mount.kind == "volume" && mount.name == volume && mount.destination == SERVICE_SOCKET_DIR
    }) {
        return Err(failed(
            "this Session's container was started before the browser was configured, so the browser cannot reach its apps; restart the Session",
        ));
    }
    if probe_socket(&container, ports[0]).await? {
        return Ok(());
    }
    let mut args: Vec<String> = vec![
        "exec".into(),
        "-d".into(),
        "--user".into(),
        "0".into(),
        container.clone(),
        SUPERVISOR_CONTAINER_PATH.into(),
    ];
    args.extend(service_forwarder_args(ports));
    let output = podman(&args, COMMAND_TIMEOUT).await?;
    if output.timed_out || !output.status.success() {
        return Err(failed(format!(
            "starting the Session's port forwarder: {}",
            stderr_text(&output)
        )));
    }
    for _ in 0..30 {
        if probe_socket(&container, ports[0]).await? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(failed(
        "the Session's port forwarder did not open its sockets",
    ))
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
    /// Mount the egress socket volume and listen on 127.0.0.1:3128.
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
        args.push(format!("{BROWSER_PROXY_LISTEN}={EGRESS_PROXY_SOCKET}"));
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
    fn the_forwarder_serves_each_port_from_the_session_loopback() {
        assert_eq!(
            service_forwarder_args(&[3000, 8765]),
            [
                "--bridge",
                "--unix-to-tcp",
                "/run/axocoatl-svc/3000.sock=127.0.0.1:3000",
                "--unix-to-tcp",
                "/run/axocoatl-svc/8765.sock=127.0.0.1:8765",
            ]
        );
        assert_eq!(browser_user(""), "1000:1000");
        assert_eq!(browser_user("root"), "1000:1000");
        assert_eq!(browser_user("0:0"), "1000:1000");
        assert_eq!(browser_user("node"), "node");
        assert_eq!(browser_user("1001:1001"), "1001:1001");
        assert_eq!(dedup_ports(&[8765, 0, 8765, 3000]), vec![8765, 3000]);
        assert_eq!(companion_containers("s"), ["axo-brw-s", "axo-egr-s"]);
        assert_eq!(companion_volumes("s"), ["axo-svc-s", "axo-egr-s"]);
    }
}
