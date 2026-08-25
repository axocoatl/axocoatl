//! Podman detection, setup, and lifecycle for session sandboxes.
//!
//! Podman is the only supported container runtime — it is rootless,
//! daemonless, and cross-platform: native on Linux/WSL, and a managed Linux VM
//! (`podman machine`) on macOS and Windows. Docker is deliberately not used.
//!
//! [`ensure_ready`] verifies that podman is installed and starts an existing,
//! stopped VM when needed. It never installs host software or creates a VM;
//! those changes require an explicit user action outside Session startup.

use std::time::Duration;

use tokio::process::Command;

use crate::error::IsolationError;
use crate::session_sandbox::{BoundedCommandOutput, SessionSandbox};

const READINESS_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
/// Starting an existing macOS VM is a bounded mutation but can take
/// substantially longer than the read-only version, list, and info probes.
const MACHINE_START_TIMEOUT: Duration = Duration::from_secs(120);

/// Whether podman is usable, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PodmanReadiness {
    /// Podman is installed and (where applicable) its VM is running.
    Ready,
    /// Podman is not installed.
    NotInstalled,
    /// macOS/Windows: podman is installed but no `podman machine` exists yet.
    MachineMissing,
    /// macOS/Windows: a `podman machine` exists but is stopped.
    MachineStopped,
    /// The Podman client could not complete a bounded readiness probe.
    Unavailable(String),
}

impl PodmanReadiness {
    /// A human-readable status line for `axocoatl doctor`.
    pub fn summary(&self) -> String {
        match self {
            PodmanReadiness::Ready => "podman ready".to_string(),
            PodmanReadiness::NotInstalled => {
                format!("podman not installed — {}", manual_install_hint())
            }
            PodmanReadiness::MachineMissing => {
                "podman installed, but no VM — run: podman machine init && podman machine start"
                    .to_string()
            }
            PodmanReadiness::MachineStopped => {
                "podman installed, VM stopped — run: podman machine start".to_string()
            }
            PodmanReadiness::Unavailable(error) => {
                format!("podman readiness check failed: {error}")
            }
        }
    }
}

/// True on platforms where podman runs containers inside a managed VM.
fn needs_machine() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

fn command_exists_on_path(bin: &str) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|directory| {
                std::fs::metadata(directory.join(bin))
                    .ok()
                    .is_some_and(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    })
            })
        })
    }
    #[cfg(not(unix))]
    {
        // Native Windows is not a release target (Windows runs through WSL2),
        // but retain the bounded process probe for cross-compilation callers.
        let _ = bin;
        true
    }
}

/// Is `bin` on PATH and runnable?
async fn bounded_output(
    bin: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<BoundedCommandOutput, IsolationError> {
    let mut command = Command::new(bin);
    command.args(args);
    SessionSandbox::run_bounded_command(command, timeout).await
}

async fn has(bin: &str) -> Result<bool, IsolationError> {
    if !command_exists_on_path(bin) {
        return Ok(false);
    }
    let output = bounded_output(bin, &["--version"], READINESS_COMMAND_TIMEOUT).await?;
    if output.timed_out {
        return Err(IsolationError::Timeout(READINESS_COMMAND_TIMEOUT));
    }
    if output.status.success() {
        Ok(true)
    } else {
        Err(IsolationError::OciSetupFailed(format!(
            "{bin} --version failed: {}",
            command_error_detail(&output)
        )))
    }
}

fn command_error_detail(output: &BoundedCommandOutput) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim();
    if !stdout.is_empty() {
        return stdout.to_string();
    }
    format!("process exited with {}", output.status)
}

fn validate_runtime_info(stdout: &[u8]) -> Result<(), IsolationError> {
    let info: serde_json::Value = serde_json::from_slice(stdout).map_err(|error| {
        IsolationError::OciSetupFailed(format!("podman info returned invalid JSON: {error}"))
    })?;
    if info.is_object() {
        Ok(())
    } else {
        Err(IsolationError::OciSetupFailed(
            "podman info did not return a JSON object".to_string(),
        ))
    }
}

/// Prove that the Podman service can answer a bounded runtime request. A
/// working client binary or a running machine entry alone is not enough.
async fn runtime_available() -> Result<(), IsolationError> {
    let output = bounded_output(
        "podman",
        &["info", "--format", "json"],
        READINESS_COMMAND_TIMEOUT,
    )
    .await?;
    if output.timed_out {
        return Err(IsolationError::Timeout(READINESS_COMMAND_TIMEOUT));
    }
    if !output.status.success() {
        return Err(IsolationError::OciSetupFailed(format!(
            "podman info failed: {}",
            command_error_detail(&output)
        )));
    }
    validate_runtime_info(&output.stdout)
}

/// `(machine exists, machine running)` — only meaningful on macOS/Windows.
async fn machine_state() -> Result<(bool, bool), IsolationError> {
    let output = bounded_output(
        "podman",
        &["machine", "list", "--format", "json"],
        READINESS_COMMAND_TIMEOUT,
    )
    .await?;
    machine_state_from_output(&output)
}

fn machine_state_from_output(
    output: &BoundedCommandOutput,
) -> Result<(bool, bool), IsolationError> {
    if output.timed_out {
        return Err(IsolationError::Timeout(READINESS_COMMAND_TIMEOUT));
    }
    if !output.status.success() {
        return Err(IsolationError::OciSetupFailed(format!(
            "podman machine list failed: {}",
            command_error_detail(output)
        )));
    }
    let machines: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).map_err(|error| {
            IsolationError::OciSetupFailed(format!(
                "podman machine list returned invalid JSON: {error}"
            ))
        })?;
    let running = machines
        .iter()
        .any(|m| m.get("Running").and_then(|v| v.as_bool()).unwrap_or(false));
    Ok((!machines.is_empty(), running))
}

/// Inspect the current podman readiness.
pub async fn detect() -> PodmanReadiness {
    let installed = match has("podman").await {
        Ok(installed) => installed,
        Err(error) => return PodmanReadiness::Unavailable(error.to_string()),
    };
    if !installed {
        return PodmanReadiness::NotInstalled;
    }
    if !needs_machine() {
        // Linux / WSL — podman runs containers natively, no VM.
        return match runtime_available().await {
            Ok(()) => PodmanReadiness::Ready,
            Err(error) => PodmanReadiness::Unavailable(error.to_string()),
        };
    }
    match machine_state().await {
        Err(error) => PodmanReadiness::Unavailable(error.to_string()),
        Ok((false, _)) => PodmanReadiness::MachineMissing,
        Ok((true, false)) => PodmanReadiness::MachineStopped,
        Ok((true, true)) => match runtime_available().await {
            Ok(()) => PodmanReadiness::Ready,
            Err(error) => PodmanReadiness::Unavailable(error.to_string()),
        },
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ReadyAction {
    Ready,
    StartExistingMachine,
    Fail(String),
}

fn ready_action(readiness: PodmanReadiness) -> ReadyAction {
    match readiness {
        PodmanReadiness::Ready => ReadyAction::Ready,
        PodmanReadiness::MachineStopped => ReadyAction::StartExistingMachine,
        PodmanReadiness::MachineMissing => ReadyAction::Fail(
            "podman is installed but has no VM — run: podman machine init && podman machine start"
                .to_string(),
        ),
        PodmanReadiness::NotInstalled => ReadyAction::Fail(format!(
            "podman is required for local Session isolation but is not installed. Axocoatl will not install host software automatically; {}",
            manual_install_hint()
        )),
        PodmanReadiness::Unavailable(error) => ReadyAction::Fail(format!(
            "podman readiness check did not complete safely: {error}"
        )),
    }
}

/// Bring an already-installed podman runtime to a [`PodmanReadiness::Ready`]
/// state. Starting an existing VM is safe to retry, but installing podman or
/// creating its VM is deliberately left to an explicit user action.
pub async fn ensure_ready() -> Result<(), IsolationError> {
    static READINESS_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let readiness_lock = READINESS_LOCK.get_or_init(|| tokio::sync::Mutex::new(()));
    // Several Sessions may be prepared concurrently. Serialize the
    // detect/start/re-probe transition so one daemon never launches competing
    // `podman machine start` clients against the same managed VM.
    let _readiness = readiness_lock.lock().await;
    match ready_action(detect().await) {
        ReadyAction::Ready => Ok(()),
        ReadyAction::StartExistingMachine => machine_start().await,
        ReadyAction::Fail(message) => Err(IsolationError::OciSetupFailed(message)),
    }
}

/// Start an existing (stopped) `podman machine`.
async fn machine_start() -> Result<(), IsolationError> {
    tracing::info!("starting the podman machine");
    let output = bounded_output("podman", &["machine", "start"], MACHINE_START_TIMEOUT)
        .await
        .map_err(|error| {
            IsolationError::OciSetupFailed(format!("podman machine start: {error}"))
        })?;
    if output.timed_out {
        return Err(IsolationError::Timeout(MACHINE_START_TIMEOUT));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() || stderr.contains("already running") {
        runtime_available().await.map_err(|error| {
            IsolationError::OciSetupFailed(format!(
                "podman machine started but its runtime is not ready: {error}"
            ))
        })
    } else {
        Err(IsolationError::OciSetupFailed(format!(
            "could not start the podman machine: {}",
            stderr.trim()
        )))
    }
}

/// The exact command to install podman on the current OS.
pub fn manual_install_hint() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "install it, e.g.: sudo apt-get install -y podman  (or dnf/pacman/zypper)"
    }
    #[cfg(target_os = "macos")]
    {
        "install it: brew install podman && podman machine init && podman machine start"
    }
    #[cfg(target_os = "windows")]
    {
        "install it: winget install RedHat.Podman, then: podman machine init && podman machine start"
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        "install podman — see https://podman.io/docs/installation"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_runtime_requires_explicit_host_install() {
        let ReadyAction::Fail(message) = ready_action(PodmanReadiness::NotInstalled) else {
            panic!("missing podman must fail closed");
        };
        assert!(message.contains("will not install host software automatically"));

        assert_eq!(
            ready_action(PodmanReadiness::MachineStopped),
            ReadyAction::StartExistingMachine
        );
        assert!(matches!(
            ready_action(PodmanReadiness::MachineMissing),
            ReadyAction::Fail(message) if message.contains("podman machine init")
        ));
    }

    #[test]
    fn runtime_info_requires_a_json_object() {
        assert!(validate_runtime_info(br#"{"host":{},"store":{}}"#).is_ok());
        assert!(validate_runtime_info(br#"[]"#).is_err());
        assert!(validate_runtime_info(b"not-json").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn machine_list_failures_are_unavailable_not_missing() {
        let failed = bounded_output(
            "sh",
            &["-c", "printf 'machine state denied' >&2; exit 17"],
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        let error = machine_state_from_output(&failed).unwrap_err().to_string();
        assert!(error.contains("podman machine list failed"));
        assert!(error.contains("machine state denied"));

        let malformed = bounded_output("sh", &["-c", "printf 'not-json'"], Duration::from_secs(1))
            .await
            .unwrap();
        let error = machine_state_from_output(&malformed)
            .unwrap_err()
            .to_string();
        assert!(error.contains("podman machine list returned invalid JSON"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hanging_readiness_client_is_killed_and_reaped_within_its_bound() {
        let data = tempfile::tempdir().unwrap();
        let pid_file = data.path().join("client.pid");
        let pid_path = pid_file.to_str().unwrap();
        let started = tokio::time::Instant::now();
        let output = bounded_output(
            "sh",
            &[
                "-c",
                "echo $$ > \"$1\"; exec sleep 30",
                "podman-readiness-test",
                pid_path,
            ],
            Duration::from_millis(100),
        )
        .await
        .unwrap();

        assert!(output.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid = std::fs::read_to_string(pid_file).unwrap();
        let status = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "timed-out client must be reaped");
    }
}
