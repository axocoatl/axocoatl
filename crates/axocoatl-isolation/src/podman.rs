//! Podman detection, setup, and lifecycle for session sandboxes.
//!
//! Podman is the only supported container runtime — it is rootless,
//! daemonless, and cross-platform: native on Linux/WSL, and a managed Linux VM
//! (`podman machine`) on macOS and Windows. Docker is deliberately not used.
//!
//! [`ensure_ready`] verifies that podman is installed and starts an existing,
//! stopped VM when needed. It never installs host software or creates a VM;
//! those changes require an explicit user action outside Session startup
//! (`axocoatl onboard` asks before it does either). [`preflight`] asks the
//! runtime first, so a setup that reaches Podman through `CONTAINER_HOST` or
//! `CONTAINER_CONNECTION` passes without a local machine. [`machine_spec`]
//! sizes the machine onboarding creates, and every hint that tells someone to
//! create one prints the same sized command ([`machine_create_hint`]).

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
            PodmanReadiness::MachineMissing => format!(
                "podman installed, but no VM — run `axocoatl onboard`, or: {}",
                machine_create_hint()
            ),
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
    if !command_exists(bin) {
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
    runtime_available_with(PODMAN).await
}

async fn runtime_available_with(program: &str) -> Result<(), IsolationError> {
    let output = bounded_output(
        program,
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
async fn machine_state_with(program: &str) -> Result<(bool, bool), IsolationError> {
    let output = bounded_output(
        program,
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

/// Ask the runtime first: when `podman info` answers, Podman is ready,
/// however it is reached (a local machine, `CONTAINER_HOST` or
/// `CONTAINER_CONNECTION`). Only when it does not does [`detect`] say why
/// (not installed, no machine, a stopped machine, or the error).
pub async fn preflight() -> PodmanReadiness {
    preflight_with(PODMAN).await
}

/// [`preflight`] with the Podman client at `program`.
pub async fn preflight_with(program: &str) -> PodmanReadiness {
    if command_exists(program) && runtime_available_with(program).await.is_ok() {
        return PodmanReadiness::Ready;
    }
    detect_with(program).await
}

const PODMAN: &str = "podman";

/// `program` is runnable: a path to an executable file, or a name found on
/// PATH.
fn command_exists(program: &str) -> bool {
    if program.contains('/') {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            return std::fs::metadata(program).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            });
        }
        #[cfg(not(unix))]
        {
            return std::path::Path::new(program).is_file();
        }
    }
    command_exists_on_path(program)
}

/// Inspect the current podman readiness.
pub async fn detect() -> PodmanReadiness {
    detect_with(PODMAN).await
}

/// [`detect`] with the Podman client at `program`.
pub async fn detect_with(program: &str) -> PodmanReadiness {
    let installed = if command_exists(program) {
        match has(program).await {
            Ok(installed) => installed,
            Err(error) => return PodmanReadiness::Unavailable(error.to_string()),
        }
    } else {
        false
    };
    if !installed {
        return PodmanReadiness::NotInstalled;
    }
    if !needs_machine() {
        // Linux / WSL — podman runs containers natively, no VM.
        return match runtime_available_with(program).await {
            Ok(()) => PodmanReadiness::Ready,
            Err(error) => PodmanReadiness::Unavailable(error.to_string()),
        };
    }
    match machine_state_with(program).await {
        Err(error) => PodmanReadiness::Unavailable(error.to_string()),
        Ok((false, _)) => PodmanReadiness::MachineMissing,
        Ok((true, false)) => PodmanReadiness::MachineStopped,
        Ok((true, true)) => match runtime_available_with(program).await {
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
        PodmanReadiness::MachineMissing => ReadyAction::Fail(format!(
            "podman is installed but has no VM — run `axocoatl onboard`, or: {}",
            machine_create_hint()
        )),
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
    match ready_action(preflight().await) {
        ReadyAction::Ready => Ok(()),
        ReadyAction::StartExistingMachine => machine_start().await,
        ReadyAction::Fail(message) => Err(IsolationError::OciSetupFailed(message)),
    }
}

/// Start an existing (stopped) `podman machine`.
async fn machine_start() -> Result<(), IsolationError> {
    tracing::info!("starting the podman machine");
    let output = bounded_output(PODMAN, &["machine", "start"], MACHINE_START_TIMEOUT)
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
pub fn manual_install_hint() -> String {
    #[cfg(target_os = "linux")]
    {
        "run `axocoatl onboard` (it asks before installing), or install it, e.g.: sudo apt-get \
         install -y podman  (or dnf/pacman/zypper)"
            .to_string()
    }
    #[cfg(target_os = "macos")]
    {
        format!(
            "run `axocoatl onboard` (it asks before installing), or install it: brew install \
             podman && {}",
            machine_create_hint()
        )
    }
    #[cfg(target_os = "windows")]
    {
        format!(
            "install it: winget install RedHat.Podman, then: {}",
            machine_create_hint()
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        "install podman — see https://podman.io/docs/installation".to_string()
    }
}

// ── The machine Axocoatl creates ────────────────────────────────────────

/// The provider Axocoatl creates a macOS machine with: Apple's
/// Virtualization framework, the one Axocoatl is tested with. Homebrew's
/// Podman uses it by default; the podman.io package defaults to libkrun.
pub const MACHINE_PROVIDER: &str = "applehv";

/// The machine size Axocoatl is tested with: 6 CPUs and 8 GiB of memory.
pub const TESTED_MACHINE_CPUS: u64 = 6;
/// See [`TESTED_MACHINE_CPUS`].
pub const TESTED_MACHINE_MEMORY_MIB: u64 = 8192;
/// The disk the machine Axocoatl creates gets, in GiB.
pub const MACHINE_DISK_GIB: u64 = 60;
/// Below this a machine holds too few Sessions (each may take 2 GiB), and
/// `doctor` warns.
pub const MIN_MACHINE_MEMORY_MIB: u64 = 4096;
/// What creating a machine downloads (its Fedora CoreOS image).
pub const MACHINE_DOWNLOAD: &str = "about 0.9 GB";
/// Starting a machine that exists.
pub const MACHINE_START_COMMAND: &str = "podman machine start";

/// The size of the Podman machine Axocoatl creates on a computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineSpec {
    pub cpus: u64,
    pub memory_mib: u64,
    pub disk_gib: u64,
}

/// The machine for a computer with `ram_bytes` of memory and `ncpu`
/// processors: the tested 6 CPUs and 8 GiB, clamped to half the processors
/// and half the memory (rounded down to 512 MiB, at least 2 GiB). Never
/// rootful; [`MACHINE_PROVIDER`] on macOS.
pub fn machine_spec(ram_bytes: u64, ncpu: u64) -> MachineSpec {
    let cpus = (ncpu / 2).clamp(1, TESTED_MACHINE_CPUS);
    let half_mib = ram_bytes / 2 / (1024 * 1024);
    let memory_mib = (half_mib / 512 * 512).clamp(2048, TESTED_MACHINE_MEMORY_MIB);
    MachineSpec {
        cpus,
        memory_mib,
        disk_gib: MACHINE_DISK_GIB,
    }
}

/// [`machine_spec`] for this computer. When its memory cannot be read, the
/// tested size.
pub fn host_machine_spec() -> MachineSpec {
    machine_spec(
        crate::host::memory_bytes().unwrap_or(TESTED_MACHINE_MEMORY_MIB * 2 * 1024 * 1024),
        crate::host::cpu_count(),
    )
}

impl MachineSpec {
    /// The size Axocoatl is tested with.
    pub fn tested(&self) -> bool {
        self.cpus == TESTED_MACHINE_CPUS && self.memory_mib == TESTED_MACHINE_MEMORY_MIB
    }

    /// `podman` arguments that create a machine of this size (`name`: the
    /// default machine when `None`). Never `--rootful`.
    pub fn init_args(&self, name: Option<&str>) -> Vec<String> {
        let mut args = vec![
            "machine".to_string(),
            "init".to_string(),
            "--cpus".to_string(),
            self.cpus.to_string(),
            "--memory".to_string(),
            self.memory_mib.to_string(),
            "--disk-size".to_string(),
            self.disk_gib.to_string(),
        ];
        args.extend(name.map(str::to_string));
        args
    }

    /// The command that creates the default machine of this size.
    pub fn init_command(&self) -> String {
        format!("podman {}", self.init_args(None).join(" "))
    }

    /// The memory as people say it: `8 GiB`, `6.5 GiB`.
    pub fn memory_text(&self) -> String {
        gib_text(self.memory_mib)
    }
}

/// `mib` as GiB, with one decimal only when it is not whole.
pub fn gib_text(mib: u64) -> String {
    if mib.is_multiple_of(1024) {
        format!("{} GiB", mib / 1024)
    } else {
        format!("{:.1} GiB", mib as f64 / 1024.0)
    }
}

/// The commands that create and start the machine for this computer, as
/// every hint prints them.
pub fn machine_create_hint() -> String {
    format!(
        "{} && {MACHINE_START_COMMAND}",
        host_machine_spec().init_command()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_machine_is_sized_from_memory_and_processors_and_capped_at_the_tested_size() {
        const GIB: u64 = 1 << 30;
        let tested = machine_spec(48 * GIB, 18);
        assert_eq!(
            tested,
            MachineSpec {
                cpus: 6,
                memory_mib: 8192,
                disk_gib: 60
            }
        );
        assert!(tested.tested());
        assert_eq!(
            tested.init_command(),
            "podman machine init --cpus 6 --memory 8192 --disk-size 60"
        );
        assert_eq!(tested.memory_text(), "8 GiB");
        assert!(!tested
            .init_args(None)
            .iter()
            .any(|arg| arg.contains("rootful")));
        assert_eq!(
            tested
                .init_args(Some("axocoatl"))
                .last()
                .map(String::as_str),
            Some("axocoatl")
        );
        let small = machine_spec(16 * GIB, 8);
        assert_eq!((small.cpus, small.memory_mib), (4, 8192));
        assert!(!small.tested());
        let smaller = machine_spec(8 * GIB, 8);
        assert_eq!((smaller.cpus, smaller.memory_mib), (4, 4096));
        let odd = machine_spec(13 * GIB, 3);
        assert_eq!((odd.cpus, odd.memory_mib), (1, 6656));
        assert_eq!(odd.memory_text(), "6.5 GiB");
        let tiny = machine_spec(2 * GIB, 1);
        assert_eq!((tiny.cpus, tiny.memory_mib), (1, 2048));
        assert_eq!(machine_spec(512 * GIB, 64), tested);
    }

    /// Every hint that tells someone to create a machine prints the sized
    /// command onboarding runs, never Podman's 2 GiB default.
    #[test]
    fn every_create_hint_prints_the_sized_machine() {
        let hint = machine_create_hint();
        assert!(hint.starts_with("podman machine init --cpus "), "{hint}");
        assert!(hint.contains(" --memory "), "{hint}");
        assert!(hint.ends_with("&& podman machine start"), "{hint}");
        assert!(PodmanReadiness::MachineMissing.summary().contains(&hint));
        let ReadyAction::Fail(message) = ready_action(PodmanReadiness::MachineMissing) else {
            panic!("a missing machine fails");
        };
        assert!(message.contains(&hint), "{message}");
        assert!(message.contains("axocoatl onboard"), "{message}");
        if cfg!(target_os = "macos") {
            assert!(manual_install_hint().contains(&hint));
        }
    }

    #[cfg(unix)]
    fn shim(directory: &std::path::Path, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = directory.join("podman");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$*\" >> \"{}/calls.log\"\n{body}\n",
                directory.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

    #[cfg(unix)]
    fn calls(directory: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(directory.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// A Podman that answers `podman info` is ready however it is reached:
    /// preflight never lists machines then (CONTAINER_HOST or
    /// CONTAINER_CONNECTION setups have none).
    #[cfg(unix)]
    #[tokio::test]
    async fn preflight_asks_the_runtime_first() {
        let directory = tempfile::tempdir().unwrap();
        let podman = shim(
            directory.path(),
            r#"case "$*" in
  "info --format json") echo '{"host":{},"store":{}}' ;;
  *) echo "unexpected: $*" >&2; exit 125 ;;
esac"#,
        );
        assert_eq!(preflight_with(&podman).await, PodmanReadiness::Ready);
        assert_eq!(calls(directory.path()), ["info --format json"]);
    }

    /// When the runtime does not answer, preflight says why: a stopped
    /// machine (which `ensure_ready` starts), no machine, or no Podman.
    #[cfg(unix)]
    #[tokio::test]
    async fn preflight_explains_a_runtime_that_does_not_answer() {
        let directory = tempfile::tempdir().unwrap();
        let podman = shim(
            directory.path(),
            r#"case "$*" in
  "--version") echo "podman version 5.8.2" ;;
  "machine list --format json") echo '[{"Name":"podman-machine-default","Running":false}]' ;;
  *) echo "Cannot connect to Podman" >&2; exit 125 ;;
esac"#,
        );
        let expected = if needs_machine() {
            PodmanReadiness::MachineStopped
        } else {
            PodmanReadiness::Unavailable(String::new())
        };
        match (preflight_with(&podman).await, expected) {
            (PodmanReadiness::Unavailable(error), PodmanReadiness::Unavailable(_)) => {
                assert!(error.contains("Cannot connect to Podman"), "{error}")
            }
            (actual, expected) => assert_eq!(actual, expected),
        }
        assert_eq!(calls(directory.path())[0], "info --format json");
        let missing = directory.path().join("absent").display().to_string();
        assert_eq!(
            preflight_with(&missing).await,
            PodmanReadiness::NotInstalled
        );
    }

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
