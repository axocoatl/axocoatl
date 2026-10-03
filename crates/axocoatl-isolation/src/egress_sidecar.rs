//! The egress sidecar, `axo-egr-{session}`: the execution supervisor's
//! `--egress-proxy` in the scratch egress image, attached to the daemon over
//! its stdio. It listens on `/run/axocoatl-egress/proxy.sock` in the
//! `axo-egr-{session}` volume, asks the daemon about every request, and
//! connects only to the addresses the daemon returns.
//!
//! This build starts it on demand for the browser tool's declared hosts.
//! A sidecar whose channel ended is started again on the next use, at most
//! [`RESTART_BUDGET`] times in [`RESTART_WINDOW`]; after that the sidecar is
//! failed and every request through it is refused by the bridge.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use crate::browser_container::{
    create_owned_volume, egress_volume, EGRESS_PROXY_SOCKET, EGRESS_ROLE, EGRESS_SOCKET_DIR,
    ROLE_LABEL,
};
use crate::egress::{EgressAuthority, SidecarEvent};
use crate::egress_control::{self, ControlEnd, ControlHandle, ControlTiming};
use crate::{IsolationError, SessionSandbox};

const RUNTIME_AUTHORITY_LABEL: &str = "io.axocoatl.runtime-authority";
pub const RESTART_BUDGET: usize = 5;
pub const RESTART_WINDOW: Duration = Duration::from_secs(600);
const REMOVE_TIMEOUT: Duration = Duration::from_secs(30);
const STDERR_KEEP: usize = 8 * 1024;

fn failed(message: impl std::fmt::Display) -> IsolationError {
    IsolationError::OciContainerFailed(format!("egress sidecar: {message}"))
}

/// What one Session's sidecar runs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarSpec {
    pub session_id: String,
    pub runtime_authority: String,
    /// The egress image tag from [`crate::egress_image::ensure_egress_image`].
    pub image: String,
    /// Podman network for the sidecar; `None` uses Podman's default.
    pub network: Option<String>,
    pub max_connections: u32,
    pub with_limits: bool,
    /// Refuse to start without limits when the host cannot apply them.
    pub require_limits: bool,
    /// Extra labels, such as a test owner label.
    pub labels: Vec<(String, String)>,
}

/// The sidecar's container name, which is also its socket volume's name.
pub fn sidecar_name(session_id: &str) -> String {
    egress_volume(session_id)
}

/// The `podman run` arguments for the sidecar (pure). No `-e`, no `--env`,
/// no secret and no bind mount: the sidecar learns everything over stdio.
pub fn build_sidecar_args(spec: &SidecarSpec) -> Vec<String> {
    let name = sidecar_name(&spec.session_id);
    let mut args: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "-i".into(),
        "--name".into(),
        name.clone(),
        "--pull=never".into(),
        "--label".into(),
        format!("{RUNTIME_AUTHORITY_LABEL}={}", spec.runtime_authority),
        "--label".into(),
        format!("{ROLE_LABEL}={EGRESS_ROLE}"),
    ];
    for (key, value) in &spec.labels {
        args.push("--label".into());
        args.push(format!("{key}={value}"));
    }
    args.extend(
        [
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--http-proxy=false",
            "--no-hosts",
            "--no-healthcheck",
            "--image-volume=ignore",
            "--user",
            "0:0",
        ]
        .map(String::from),
    );
    if spec.with_limits {
        args.push("--pids-limit".into());
        args.push((spec.max_connections + 32).to_string());
        args.push("--memory".into());
        args.push("128m".into());
    }
    if let Some(network) = &spec.network {
        args.push("--network".into());
        args.push(network.clone());
    }
    args.push("--mount".into());
    args.push(format!(
        "type=volume,source={name},destination={EGRESS_SOCKET_DIR}"
    ));
    args.push("--entrypoint".into());
    args.push(crate::supervisor_program::SUPERVISOR_CONTAINER_PATH.into());
    args.push(spec.image.clone());
    args.extend([
        "--egress-proxy".into(),
        "--socket".into(),
        EGRESS_PROXY_SOCKET.into(),
        "--max-connections".into(),
        spec.max_connections.to_string(),
    ]);
    args
}

struct Running {
    generation: u32,
    control: ControlHandle,
    child: Child,
    task: JoinHandle<ControlEnd>,
    stderr: Arc<std::sync::Mutex<Vec<u8>>>,
}

#[derive(Default)]
struct State {
    generation: u32,
    running: Option<Running>,
    starts: VecDeque<Instant>,
    failed: Option<String>,
}

/// A sidecar's phase, generation and restart count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidecarStatus {
    /// `not_started`, `starting`, `ready`, `channel_lost`, `stopped` or
    /// `failed`.
    pub phase: &'static str,
    pub generation: u32,
    pub restarts: u32,
}

/// One Session's sidecar, started on first use.
pub struct EgressSidecar {
    spec: SidecarSpec,
    authority: Arc<dyn EgressAuthority>,
    timing: ControlTiming,
    state: tokio::sync::Mutex<State>,
}

impl std::fmt::Debug for EgressSidecar {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EgressSidecar")
            .field("session", &self.spec.session_id)
            .finish_non_exhaustive()
    }
}

impl EgressSidecar {
    pub fn new(spec: SidecarSpec, authority: Arc<dyn EgressAuthority>) -> Self {
        Self {
            spec,
            authority,
            timing: ControlTiming::default(),
            state: tokio::sync::Mutex::new(State::default()),
        }
    }

    pub fn spec(&self) -> &SidecarSpec {
        &self.spec
    }

    /// Where the sidecar is, for status views. A sidecar being started or
    /// stopped reads as `starting`.
    pub fn status(&self) -> SidecarStatus {
        let Ok(state) = self.state.try_lock() else {
            return SidecarStatus {
                phase: "starting",
                generation: 0,
                restarts: 0,
            };
        };
        let phase = if state.failed.is_some() {
            "failed"
        } else {
            match &state.running {
                Some(running) if running.control.is_open() && !running.task.is_finished() => {
                    "ready"
                }
                Some(_) => "channel_lost",
                None if state.generation == 0 => "not_started",
                None => "stopped",
            }
        };
        SidecarStatus {
            phase,
            generation: state.generation,
            restarts: state.generation.saturating_sub(1),
        }
    }

    /// The running sidecar's control channel, starting or restarting it when
    /// needed. Fails once the restart budget is spent.
    pub async fn ensure_running(&self) -> Result<ControlHandle, IsolationError> {
        let mut state = self.state.lock().await;
        if let Some(running) = &state.running {
            if running.control.is_open() && !running.task.is_finished() {
                return Ok(running.control.clone());
            }
        }
        if let Some(mut lost) = state.running.take() {
            let _ = lost.child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), lost.child.wait()).await;
            let detail = String::from_utf8_lossy(
                &lost
                    .stderr
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()),
            )
            .trim()
            .to_string();
            if !detail.is_empty() {
                tracing::warn!(session = %self.spec.session_id, generation = lost.generation, %detail, "the egress sidecar ended");
            }
        }
        if let Some(reason) = &state.failed {
            return Err(failed(reason));
        }
        let now = Instant::now();
        while state
            .starts
            .front()
            .is_some_and(|start| now.duration_since(*start) > RESTART_WINDOW)
        {
            state.starts.pop_front();
        }
        if state.starts.len() > RESTART_BUDGET {
            let reason = format!(
                "the egress proxy stopped {RESTART_BUDGET} times in {} minutes; it is not started again until the Session restarts",
                RESTART_WINDOW.as_secs() / 60
            );
            self.authority
                .sidecar_event(SidecarEvent::Failed {
                    generation: state.generation,
                    detail: reason.clone(),
                })
                .await;
            state.failed = Some(reason.clone());
            return Err(failed(reason));
        }
        if state.generation > 0 {
            self.authority
                .sidecar_event(SidecarEvent::Restarting {
                    generation: state.generation + 1,
                })
                .await;
        }
        state.generation += 1;
        state.starts.push_back(now);
        let generation = state.generation;
        match self.start(generation).await {
            Ok(running) => {
                let control = running.control.clone();
                state.running = Some(running);
                Ok(control)
            }
            Err(error) => {
                self.authority
                    .sidecar_event(SidecarEvent::Failed {
                        generation,
                        detail: error.to_string(),
                    })
                    .await;
                Err(error)
            }
        }
    }

    async fn remove_container(&self) -> Result<(), IsolationError> {
        let mut remove = Command::new("podman");
        remove.args([
            "rm",
            "--force",
            "--time",
            "0",
            "--ignore",
            &sidecar_name(&self.spec.session_id),
        ]);
        let output = SessionSandbox::run_bounded_command(remove, REMOVE_TIMEOUT).await?;
        if output.timed_out || !output.status.success() {
            return Err(failed(format!(
                "removing the old sidecar: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }

    async fn start(&self, generation: u32) -> Result<Running, IsolationError> {
        let name = sidecar_name(&self.spec.session_id);
        create_owned_volume(
            &name,
            Some(self.spec.runtime_authority.as_str()),
            EGRESS_ROLE,
        )
        .await?;
        self.remove_container().await?;
        self.authority
            .sidecar_event(SidecarEvent::Starting {
                generation,
                container: Some(name.clone()),
            })
            .await;
        let mut with_limits = self.spec.with_limits;
        loop {
            let spec = SidecarSpec {
                with_limits,
                ..self.spec.clone()
            };
            let mut command = Command::new("podman");
            command
                .args(build_sidecar_args(&spec))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let mut child = command.spawn().map_err(failed)?;
            let stdin = child.stdin.take().ok_or_else(|| failed("no stdin"))?;
            let stdout = child.stdout.take().ok_or_else(|| failed("no stdout"))?;
            let mut stderr_pipe = child.stderr.take().ok_or_else(|| failed("no stderr"))?;
            let stderr = Arc::new(std::sync::Mutex::new(Vec::new()));
            let kept = stderr.clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 4096];
                while let Ok(read) = stderr_pipe.read(&mut buffer).await {
                    if read == 0 {
                        break;
                    }
                    let mut kept = kept.lock().unwrap_or_else(|poison| poison.into_inner());
                    kept.extend_from_slice(&buffer[..read]);
                    let excess = kept.len().saturating_sub(STDERR_KEEP);
                    kept.drain(..excess);
                }
            });
            match egress_control::start(
                generation,
                stdout,
                stdin,
                self.authority.clone(),
                self.timing,
            )
            .await
            {
                Ok((control, task)) => {
                    self.authority
                        .sidecar_event(SidecarEvent::Ready {
                            generation,
                            container: Some(name.clone()),
                        })
                        .await;
                    return Ok(Running {
                        generation,
                        control,
                        child,
                        task,
                        stderr,
                    });
                }
                Err(error) => {
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                    // Give the stderr reader a moment to collect Podman's reason.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let detail = String::from_utf8_lossy(
                        &stderr.lock().unwrap_or_else(|poison| poison.into_inner()),
                    )
                    .trim()
                    .to_string();
                    let _ = self.remove_container().await;
                    if with_limits && detail.contains("cgroup") && !self.spec.require_limits {
                        tracing::warn!(
                            "this host cannot apply container resource limits; starting the egress sidecar without them"
                        );
                        with_limits = false;
                        continue;
                    }
                    return Err(failed(format!("{error}: {detail}")));
                }
            }
        }
    }

    /// Stop the sidecar and remove its container. The socket volume stays
    /// until the Session is removed.
    pub async fn stop(&self) {
        let mut state = self.state.lock().await;
        if let Some(mut running) = state.running.take() {
            running.control.shutdown();
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut running.task).await;
            let _ = running.child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), running.child.wait()).await;
        }
        let _ = self.remove_container().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SidecarSpec {
        SidecarSpec {
            session_id: "ses-1".into(),
            runtime_authority: "authority".into(),
            image: "localhost/axocoatl-egress:866a01dbe5366107-aarch64".into(),
            network: None,
            max_connections: 128,
            with_limits: true,
            require_limits: false,
            labels: vec![],
        }
    }

    #[test]
    fn sidecar_arguments_carry_no_environment_secret_or_bind_mount() {
        let args = build_sidecar_args(&spec());
        let joined = args.join(" ");
        for required in [
            "--rm",
            "-i",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--http-proxy=false",
            "--no-hosts",
            "--no-healthcheck",
            "--image-volume=ignore",
            "--pull=never",
        ] {
            assert!(args.contains(&required.to_string()), "{required}");
        }
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--name", "axo-egr-ses-1"]));
        assert!(args.windows(2).any(|pair| pair == ["--user", "0:0"]));
        assert!(args.windows(2).any(|pair| pair == ["--pids-limit", "160"]));
        assert!(args.windows(2).any(|pair| pair == ["--memory", "128m"]));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--label" && pair[1] == "io.axocoatl.role=egress"));
        assert!(joined.contains(
            "--mount type=volume,source=axo-egr-ses-1,destination=/run/axocoatl-egress "
        ));
        assert!(joined.ends_with(
            "--entrypoint /axocoatl-exec-supervisor localhost/axocoatl-egress:866a01dbe5366107-aarch64 --egress-proxy --socket /run/axocoatl-egress/proxy.sock --max-connections 128"
        ));
        for refused in [
            "-e",
            "--env",
            "--env-file",
            "-v",
            "--volume",
            "-p",
            "--publish",
        ] {
            assert!(!args.contains(&refused.to_string()), "{refused}");
        }
        assert!(!joined.contains("type=bind"));
        assert!(!args.iter().any(|arg| arg.starts_with("--network")));

        let mut on_network = spec();
        on_network.network = Some("axo-egress-test".into());
        on_network.with_limits = false;
        let args = build_sidecar_args(&on_network);
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--network", "axo-egress-test"]));
        assert!(!args.contains(&"--pids-limit".to_string()));
    }
}
