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
//! `CONTAINER_CONNECTION` passes without a local machine. [`detect`] is the
//! same decision, so onboarding, doctor, the daemon's start and its Session
//! cleanup, and the workbench's readiness report never disagree about
//! whether Podman is ready. [`machine_spec`]
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
                format!("podman not installed. {}", manual_install_hint())
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

/// Which Podman connection this process's environment selects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ConnectionOverride {
    /// Neither variable is set, or, on macOS and Windows, neither is
    /// non-empty: Podman uses its default connection (on Linux, the local
    /// engine; on macOS, the default machine).
    #[default]
    None,
    /// `CONTAINER_HOST=…` without a non-empty `CONTAINER_CONNECTION`: a
    /// Podman service reached by URL, never a machine Axocoatl manages. On
    /// Linux the value is empty when `CONTAINER_HOST` is set but empty
    /// (see [`ConnectionOverride::from_env`]).
    Host(String),
    /// `CONTAINER_CONNECTION=NAME`, whether or not `CONTAINER_HOST` is set
    /// too: a named connection, which is a machine of that name when
    /// `podman machine list` lists one. On Linux the name is empty when
    /// `CONTAINER_CONNECTION` is set but empty and `CONTAINER_HOST` is not
    /// set (see [`ConnectionOverride::from_env`]).
    Connection(String),
}

impl ConnectionOverride {
    /// The connection `CONTAINER_CONNECTION` and `CONTAINER_HOST` select,
    /// read as Podman 4.9.3 to 6.1.4 reads them. To choose the connection
    /// (`setupRemoteConnection` in `cmd/podman/root.go`), a non-empty
    /// `CONTAINER_CONNECTION` wins over `CONTAINER_HOST`, and an empty
    /// value counts as unset. To choose remote mode (`IsRemote` in
    /// `cmd/podman/registry/remote.go`), Podman asks only whether either
    /// variable is set (`os.LookupEnv`), so on Linux one set but empty
    /// turns the local engine off: Podman then reaches its default system
    /// connection, else this user's API socket
    /// (`unix:///run/user/<uid>/podman/podman.sock` rootless), and that is
    /// read here as an override with an empty value ([`Self::Host`] when
    /// `CONTAINER_HOST` is set, else [`Self::Connection`]). On macOS and
    /// Windows the client is always remote, so an empty value is the same
    /// as unset. A byte of a value that is not UTF-8 becomes U+FFFD here;
    /// Podman uses the bytes as they are.
    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var_os("CONTAINER_CONNECTION").as_deref(),
            std::env::var_os("CONTAINER_HOST").as_deref(),
            needs_machine(),
        )
    }

    /// [`ConnectionOverride::from_env`] given the two values, and whether
    /// the `podman` client is always remote (macOS and Windows).
    fn from_values(
        connection: Option<&std::ffi::OsStr>,
        host: Option<&std::ffi::OsStr>,
        always_remote: bool,
    ) -> Self {
        let set = |value: Option<&std::ffi::OsStr>| {
            value
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string_lossy().into_owned())
        };
        if let Some(name) = set(connection) {
            ConnectionOverride::Connection(name)
        } else if let Some(host) = set(host) {
            ConnectionOverride::Host(host)
        } else if always_remote {
            ConnectionOverride::None
        } else if host.is_some() {
            ConnectionOverride::Host(String::new())
        } else if connection.is_some() {
            ConnectionOverride::Connection(String::new())
        } else {
            ConnectionOverride::None
        }
    }

    /// `CONTAINER_HOST=…` or `CONTAINER_CONNECTION=…`, for messages.
    pub fn describe(&self) -> Option<String> {
        match self {
            ConnectionOverride::None => None,
            ConnectionOverride::Host(host) => Some(format!("CONTAINER_HOST={host}")),
            ConnectionOverride::Connection(name) => Some(format!("CONTAINER_CONNECTION={name}")),
        }
    }
}

/// Ask the runtime first: when `podman info` answers, Podman is ready,
/// however it is reached (a local machine, `CONTAINER_HOST` or
/// `CONTAINER_CONNECTION`). Only when it does not does it say why (not
/// installed, no machine, a stopped machine, or the error).
pub async fn preflight() -> PodmanReadiness {
    preflight_with(PODMAN).await
}

/// [`preflight`] with the Podman client at `program`.
pub async fn preflight_with(program: &str) -> PodmanReadiness {
    preflight_through(program, &ConnectionOverride::from_env()).await
}

/// [`preflight_with`] given the connection the environment selects. When
/// one is selected and Podman does not answer through it, the default
/// machine's state says nothing about that connection, so a missing or
/// stopped default machine is never reported (and onboarding never offers
/// to create one) for it. The one exception is `CONTAINER_CONNECTION`
/// naming a machine that is stopped: that machine starts on first use like
/// the default one.
pub async fn preflight_through(program: &str, connection: &ConnectionOverride) -> PodmanReadiness {
    if !command_exists(program) {
        return PodmanReadiness::NotInstalled;
    }
    let error = match runtime_available_with(program).await {
        Ok(()) => return PodmanReadiness::Ready,
        Err(error) => error,
    };
    let unavailable = |described: String| {
        PodmanReadiness::Unavailable(format!(
            "{described} is set, and Podman does not answer through it: {error}"
        ))
    };
    match connection {
        ConnectionOverride::None => default_connection_unanswered(program, error).await,
        ConnectionOverride::Host(_) => unavailable(connection.describe().unwrap_or_default()),
        ConnectionOverride::Connection(name) => {
            let stopped_machine = needs_machine()
                && machine_list_with(program).await.is_ok_and(|machines| {
                    machines
                        .iter()
                        .any(|machine| machine.name == *name && !machine.running)
                });
            if stopped_machine {
                PodmanReadiness::MachineStopped
            } else {
                unavailable(connection.describe().unwrap_or_default())
            }
        }
    }
}

/// `podman machine list --format json`, read.
async fn machine_list_with(
    program: &str,
) -> Result<Vec<crate::podman_report::MachineInfo>, IsolationError> {
    let output = bounded_output(
        program,
        &["machine", "list", "--format", "json"],
        READINESS_COMMAND_TIMEOUT,
    )
    .await?;
    if output.timed_out {
        return Err(IsolationError::Timeout(READINESS_COMMAND_TIMEOUT));
    }
    if !output.status.success() {
        return Err(IsolationError::OciSetupFailed(format!(
            "podman machine list failed: {}",
            command_error_detail(&output)
        )));
    }
    crate::podman_report::parse_machine_list(&output.stdout).map_err(IsolationError::OciSetupFailed)
}

/// The stopped machine to start: the one `CONTAINER_CONNECTION` names,
/// else the default machine, else the only machine. `None` when Podman
/// lists no such machine (then `podman machine start` starts the default).
pub fn machine_to_start(
    machines: &[crate::podman_report::MachineInfo],
    connection: &ConnectionOverride,
) -> Option<String> {
    if let ConnectionOverride::Connection(name) = connection {
        return machines
            .iter()
            .any(|machine| machine.name == *name)
            .then(|| name.clone());
    }
    machines
        .iter()
        .find(|machine| machine.default)
        .or_else(|| (machines.len() == 1).then(|| &machines[0]))
        .map(|machine| machine.name.clone())
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

/// Inspect the current podman readiness: [`preflight`], the decision
/// onboarding and doctor report, so the daemon's start, its Session
/// cleanup and its readiness report honour `CONTAINER_HOST` and
/// `CONTAINER_CONNECTION` as they do. A Podman that answers through either
/// is ready although `podman machine list` lists no machine.
pub async fn detect() -> PodmanReadiness {
    detect_with(PODMAN).await
}

/// [`detect`] with the Podman client at `program`.
pub async fn detect_with(program: &str) -> PodmanReadiness {
    preflight_with(program).await
}

/// Why Podman's default connection (neither `CONTAINER_HOST` nor
/// `CONTAINER_CONNECTION` is set) did not answer `podman info`, which
/// failed with `error`: Podman is not installed, it has no machine or a
/// stopped one (macOS/Windows), or that error.
async fn default_connection_unanswered(program: &str, error: IsolationError) -> PodmanReadiness {
    match has(program).await {
        Ok(true) => {}
        Ok(false) => return PodmanReadiness::NotInstalled,
        Err(error) => return PodmanReadiness::Unavailable(error.to_string()),
    }
    if !needs_machine() {
        // Linux / WSL — podman runs containers natively, no VM.
        return PodmanReadiness::Unavailable(error.to_string());
    }
    match machine_state_with(program).await {
        Err(error) => PodmanReadiness::Unavailable(error.to_string()),
        Ok((false, _)) => PodmanReadiness::MachineMissing,
        Ok((true, false)) => PodmanReadiness::MachineStopped,
        // A machine runs: it may have finished starting since `error`.
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
            "podman is required for local Session isolation but is not installed. Axocoatl will not install host software automatically. {}",
            manual_install_hint()
        )),
        PodmanReadiness::Unavailable(error) => ReadyAction::Fail(unavailable_message(&error)),
    }
}

/// What a Podman that did not answer is told: for want of `newuidmap` or
/// `newgidmap` (whose own message says to run `podman system migrate`,
/// which alone changes nothing), what Podman said and the packages that
/// ship them, as doctor and the workbench's readiness item say it;
/// otherwise Podman's error.
pub fn unavailable_message(error: &str) -> String {
    match crate::podman_report::id_map_tools_hint(error) {
        Some(hint) => format!(
            "Podman did not answer ({}). {hint}",
            crate::podman_report::id_map_failure_reason(error).unwrap_or_else(|| error.to_string())
        ),
        None => format!("podman readiness check did not complete safely: {error}"),
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
        ReadyAction::StartExistingMachine => {
            machine_start(PODMAN, &ConnectionOverride::from_env()).await
        }
        ReadyAction::Fail(message) => Err(IsolationError::OciSetupFailed(message)),
    }
}

/// Start an existing (stopped) `podman machine`: the one in use, by name
/// ([`machine_to_start`]), so a machine with another name than
/// `podman-machine-default` starts too.
async fn machine_start(
    program: &str,
    connection: &ConnectionOverride,
) -> Result<(), IsolationError> {
    // Podman starts `podman-machine-default` when no name is given.
    let name = match machine_list_with(program).await {
        Ok(machines) => {
            machine_to_start(&machines, connection).filter(|name| name != "podman-machine-default")
        }
        Err(_) => None,
    };
    tracing::info!(
        machine = name.as_deref().unwrap_or("default"),
        "starting the podman machine"
    );
    let mut args = vec!["machine", "start"];
    args.extend(name.as_deref());
    let output = bounded_output(program, &args, MACHINE_START_TIMEOUT)
        .await
        .map_err(|error| {
            IsolationError::OciSetupFailed(format!("podman machine start: {error}"))
        })?;
    if output.timed_out {
        return Err(IsolationError::Timeout(MACHINE_START_TIMEOUT));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() || stderr.contains("already running") {
        runtime_available_with(program).await.map_err(|error| {
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

/// How to install Podman on this computer: [`install_fix`] for
/// [`InstallRoute::detect`].
pub fn manual_install_hint() -> String {
    install_fix(&InstallRoute::detect())
}

/// How Podman gets onto a computer, which decides what `axocoatl onboard`
/// can do about a missing Podman. `axocoatl doctor`, onboarding, the
/// workbench's first screen and the daemon's refusals all print
/// [`install_fix`] for it, so none of them says onboarding installs Podman
/// where it cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallRoute {
    /// macOS on Apple silicon with Homebrew: onboarding runs
    /// `brew install podman` after asking.
    MacHomebrew,
    /// macOS on Apple silicon without Homebrew: onboarding installs nothing
    /// and points to Podman's installer.
    MacInstaller,
    /// macOS on Intel, which Podman 6 and Homebrew's Podman no longer
    /// support: onboarding installs nothing.
    IntelMac,
    /// Linux whose package manager onboarding knows: onboarding runs this
    /// command after asking.
    LinuxPackage(String),
    /// Ubuntu 22.04 or older, whose Podman (3.4.4 on 22.04) Axocoatl
    /// cannot use: onboarding installs nothing.
    LinuxTooOld,
    /// Linux whose package manager onboarding does not know.
    LinuxOther,
    /// Windows, which is not a release target.
    Windows,
}

impl InstallRoute {
    /// This computer's route, from what the daemon can see: the processor
    /// and Homebrew (`brew` on `PATH`, in `/opt/homebrew/bin` or in
    /// `/usr/local/bin`, where onboarding looks) on macOS, and
    /// `/etc/os-release` on Linux.
    pub fn detect() -> Self {
        #[cfg(target_os = "macos")]
        {
            let homebrew = command_exists_on_path("brew")
                || ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"]
                    .iter()
                    .any(|path| is_executable_file(std::path::Path::new(path)));
            Self::for_mac(std::env::consts::ARCH == "aarch64", homebrew)
        }
        #[cfg(target_os = "linux")]
        {
            Self::for_linux(&std::fs::read_to_string("/etc/os-release").unwrap_or_default())
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            Self::Windows
        }
    }

    /// The route on a Mac: `apple_silicon`, and whether Homebrew is installed.
    pub fn for_mac(apple_silicon: bool, homebrew: bool) -> Self {
        match (apple_silicon, homebrew) {
            (false, _) => Self::IntelMac,
            (true, true) => Self::MacHomebrew,
            (true, false) => Self::MacInstaller,
        }
    }

    /// The route on Linux, from the text of `/etc/os-release`: the package
    /// manager of the distribution or the one it is like (`ID`, `ID_LIKE`),
    /// and Ubuntu's `VERSION_ID`. Onboarding decides the same way.
    pub fn for_linux(os_release: &str) -> Self {
        let mut id = String::new();
        let mut id_like = Vec::new();
        let mut version_id = String::new();
        for line in os_release.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches('"').trim_matches('\'');
            match key.trim() {
                "ID" => id = value.to_ascii_lowercase(),
                "ID_LIKE" => {
                    id_like = value
                        .split_whitespace()
                        .map(str::to_ascii_lowercase)
                        .collect()
                }
                "VERSION_ID" => version_id = value.to_string(),
                _ => {}
            }
        }
        let is = |family: &str| id == family || id_like.iter().any(|like| like == family);
        let ubuntu_major = version_id
            .split('.')
            .next()
            .and_then(|major| major.parse::<u32>().ok());
        if id == "ubuntu" && ubuntu_major.is_some_and(|major| major <= 22) {
            return Self::LinuxTooOld;
        }
        let command = if is("debian") || is("ubuntu") {
            "sudo apt-get install -y podman"
        } else if is("fedora") || is("rhel") || is("centos") {
            "sudo dnf install -y podman"
        } else if is("arch") {
            "sudo pacman -S --noconfirm podman"
        } else if is("suse") || id.starts_with("opensuse") || is("sles") {
            "sudo zypper install -y podman"
        } else {
            return Self::LinuxOther;
        };
        Self::LinuxPackage(command.to_string())
    }
}

/// What to do about a missing Podman on `route`: `axocoatl onboard` once,
/// saying whether it installs Podman there, and the command or installer to
/// use instead. Commands are in backticks; there is no final period.
pub fn install_fix(route: &InstallRoute) -> String {
    match route {
        InstallRoute::MacHomebrew => {
            "Run `axocoatl onboard` (it installs Podman after asking), or: `brew install podman`"
                .to_string()
        }
        InstallRoute::MacInstaller => "Install Podman with the installer from https://podman.io \
                                       (or install Homebrew and run `brew install podman`), then \
                                       run `axocoatl onboard`"
            .to_string(),
        InstallRoute::IntelMac => "Podman 6 no longer supports Intel Macs and Homebrew refuses \
                                   it: install the Podman 5.8 amd64 package from \
                                   https://github.com/containers/podman/releases (Axocoatl has not \
                                   been tested on Intel Macs), then run `axocoatl onboard`"
            .to_string(),
        InstallRoute::LinuxPackage(command) => {
            format!("Run `axocoatl onboard` (it installs Podman after asking), or: `{command}`")
        }
        InstallRoute::LinuxTooOld => "This Ubuntu's Podman is too old (22.04 ships 3.4.4; \
                                      Axocoatl runs need 4.3 or later, 4.9 tested): use Ubuntu \
                                      24.04 or newer"
            .to_string(),
        InstallRoute::LinuxOther => "Install Podman 4.9 or later \
                                     (https://podman.io/docs/installation), then run `axocoatl \
                                     onboard`"
            .to_string(),
        InstallRoute::Windows => format!(
            "Install Podman (`winget install RedHat.Podman`), then: `{}`",
            machine_create_hint()
        ),
    }
}

#[cfg(target_os = "macos")]
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
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
        // An install hint that goes on to the machine prints the sized one.
        let windows = install_fix(&InstallRoute::Windows);
        assert!(windows.contains(&hint), "{windows}");
    }

    /// The fix for a missing Podman says `axocoatl onboard` once, claims it
    /// installs Podman only where it does (Homebrew on Apple silicon, a
    /// package manager onboarding knows), and otherwise names what to
    /// install.
    #[test]
    fn the_install_fix_follows_what_onboarding_can_do_here() {
        let fix = |route: InstallRoute| install_fix(&route);
        assert_eq!(
            fix(InstallRoute::for_mac(true, true)),
            "Run `axocoatl onboard` (it installs Podman after asking), or: `brew install podman`"
        );
        assert_eq!(
            fix(InstallRoute::for_mac(true, false)),
            "Install Podman with the installer from https://podman.io (or install Homebrew and \
             run `brew install podman`), then run `axocoatl onboard`"
        );
        for homebrew in [true, false] {
            let intel = fix(InstallRoute::for_mac(false, homebrew));
            assert!(intel.starts_with("Podman 6 no longer supports Intel Macs"));
            assert!(!intel.contains("brew install"), "{intel}");
        }
        let ubuntu =
            "PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\n";
        assert_eq!(
            fix(InstallRoute::for_linux(ubuntu)),
            "Run `axocoatl onboard` (it installs Podman after asking), or: `sudo apt-get install \
             -y podman`"
        );
        assert_eq!(
            InstallRoute::for_linux("ID=debian\nVERSION_ID=\"12\"\n"),
            InstallRoute::LinuxPackage("sudo apt-get install -y podman".into())
        );
        assert_eq!(
            InstallRoute::for_linux("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n"),
            InstallRoute::LinuxPackage("sudo dnf install -y podman".into())
        );
        assert_eq!(
            InstallRoute::for_linux("ID=arch\n"),
            InstallRoute::LinuxPackage("sudo pacman -S --noconfirm podman".into())
        );
        assert_eq!(
            InstallRoute::for_linux("ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n"),
            InstallRoute::LinuxPackage("sudo zypper install -y podman".into())
        );
        let jammy = InstallRoute::for_linux("ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"22.04\"\n");
        assert_eq!(jammy, InstallRoute::LinuxTooOld);
        assert!(!fix(jammy).contains("axocoatl onboard` (it installs"));
        let other = fix(InstallRoute::for_linux("ID=nixos\n"));
        assert!(other.starts_with("Install Podman 4.9 or later"), "{other}");
        for route in [
            InstallRoute::MacHomebrew,
            InstallRoute::MacInstaller,
            InstallRoute::IntelMac,
            InstallRoute::LinuxPackage("sudo apt-get install -y podman".into()),
            InstallRoute::LinuxTooOld,
            InstallRoute::LinuxOther,
            InstallRoute::Windows,
        ] {
            let text = fix(route);
            assert!(text.matches("axocoatl onboard").count() <= 1, "{text}");
            assert!(!text.ends_with('.'), "{text}");
            assert!(!text.contains("  "), "{text}");
        }
        assert_eq!(manual_install_hint(), install_fix(&InstallRoute::detect()));
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

    /// With CONTAINER_HOST or CONTAINER_CONNECTION set, a Podman that does
    /// not answer through it is unavailable, never "no machine": the
    /// default machine is not the one in use, so nothing offers to create
    /// it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_named_connection_that_does_not_answer_is_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let podman = shim(
            directory.path(),
            r#"case "$*" in
  "--version") echo "podman version 5.8.2" ;;
  "machine list --format json") echo '[]' ;;
  *) echo "Cannot connect to Podman" >&2; exit 125 ;;
esac"#,
        );
        let readiness = preflight_through(
            &podman,
            &ConnectionOverride::Host("unix:///tmp/x.sock".into()),
        )
        .await;
        let PodmanReadiness::Unavailable(error) = readiness else {
            panic!("{readiness:?}");
        };
        assert!(
            error.starts_with("CONTAINER_HOST=unix:///tmp/x.sock is set"),
            "{error}"
        );
        assert!(error.contains("Cannot connect to Podman"), "{error}");
        assert!(!calls(directory.path())
            .iter()
            .any(|call| call.starts_with("machine")));
        if needs_machine() {
            assert_eq!(
                preflight_through(&podman, &ConnectionOverride::None).await,
                PodmanReadiness::MachineMissing
            );
        }
    }

    /// `CONTAINER_CONNECTION` naming a stopped machine is that machine,
    /// stopped (it starts on first use, by name); naming anything else that
    /// does not answer is unavailable.
    #[cfg(unix)]
    #[tokio::test]
    async fn container_connection_naming_a_stopped_machine_starts_it_by_name() {
        let directory = tempfile::tempdir().unwrap();
        let podman = shim(
            directory.path(),
            r#"case "$*" in
  "--version") echo "podman version 5.8.2" ;;
  "machine list --format json") echo '[{"Name":"podman-machine-default","Default":true,"Running":false},{"Name":"axocoatl","Default":false,"Running":false}]' ;;
  "machine start axocoatl") touch "$(dirname "$0")/running" ;;
  "info --format json") if [ -f "$(dirname "$0")/running" ]; then echo '{"host":{}}'; else echo "Cannot connect to Podman" >&2; exit 125; fi ;;
  *) echo "unexpected: $*" >&2; exit 125 ;;
esac"#,
        );
        let named = ConnectionOverride::Connection("axocoatl".into());
        let other = ConnectionOverride::Connection("remote-box".into());
        if needs_machine() {
            assert_eq!(
                preflight_through(&podman, &named).await,
                PodmanReadiness::MachineStopped
            );
        }
        let PodmanReadiness::Unavailable(error) = preflight_through(&podman, &other).await else {
            panic!("a connection that is not a machine is unavailable");
        };
        assert!(
            error.starts_with("CONTAINER_CONNECTION=remote-box is set"),
            "{error}"
        );
        machine_start(&podman, &named).await.unwrap();
        assert!(calls(directory.path())
            .iter()
            .any(|call| call == "machine start axocoatl"));
        assert!(!calls(directory.path())
            .iter()
            .any(|call| call == "machine start"));
    }

    /// Runs [`detect_with_honours_the_connection_override_like_onboarding`]
    /// as a child that prints one verdict: `detect` or `report`.
    #[cfg(unix)]
    const DETECT_CHILD: &str = "AXOCOATL_PODMAN_DETECT_CHILD";

    /// The daemon (`detect_with`: its start, its Session cleanup and its
    /// readiness report) and onboarding and doctor (`podman_report`) read
    /// `CONTAINER_HOST` and `CONTAINER_CONNECTION` from the environment and
    /// reach the same verdict. A Podman that answers through
    /// `CONTAINER_HOST` is ready although `podman machine list` lists no
    /// machine (1.5.0's daemon said "no machine" there while onboarding
    /// said ready); one that does not answer through it is unavailable,
    /// never "no machine" or "stopped". Without an override the machine
    /// path is unchanged. Each case runs in a child process with exactly
    /// the variables it names, so no test changes this process's
    /// environment.
    #[cfg(unix)]
    #[tokio::test]
    async fn detect_with_honours_the_connection_override_like_onboarding() {
        if let Ok(mode) = std::env::var(DETECT_CHILD) {
            let program = std::env::var("AXOCOATL_PODMAN_DETECT_PROGRAM").unwrap();
            let readiness = match mode.as_str() {
                "detect" => detect_with(&program).await,
                "report" => crate::podman_report::inspect_with(&program).await.readiness,
                other => panic!("unknown child mode {other}"),
            };
            println!("READINESS={readiness:?}");
            return;
        }

        async fn verdict(mode: &str, podman: &str, env: &[(&str, &str)]) -> String {
            let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "podman::tests::detect_with_honours_the_connection_override_like_onboarding",
                    "--nocapture",
                ])
                .env_remove("CONTAINER_HOST")
                .env_remove("CONTAINER_CONNECTION")
                .env(DETECT_CHILD, mode)
                .env("AXOCOATL_PODMAN_DETECT_PROGRAM", podman)
                .kill_on_drop(true);
            for (name, value) in env {
                command.env(name, value);
            }
            let output = tokio::time::timeout(Duration::from_secs(60), command.output())
                .await
                .expect("the child finishes")
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "{mode} {env:?}: {stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            stdout
                .lines()
                .find_map(|line| line.strip_prefix("READINESS="))
                .unwrap_or_else(|| panic!("{mode} {env:?} printed no verdict: {stdout}"))
                .to_string()
        }

        let directory = tempfile::tempdir().unwrap();
        // `info` answers only when the case says Podman answers (through
        // the override when one is set); `machine list` prints the case's
        // machines.
        let podman = shim(
            directory.path(),
            r#"case "$*" in
  "--version") echo "podman version 6.1.4" ;;
  "machine list --format json") echo "$SHIM_MACHINES" ;;
  "machine inspect "*) echo '[]' ;;
  "machine info --format json") echo '{"Host":{}}' ;;
  "info --format json")
    if [ -n "$SHIM_ANSWERS" ]; then
      echo '{"host":{"security":{"rootless":true}}}'
    else
      echo "Cannot connect to Podman" >&2; exit 125
    fi ;;
  *) echo "unexpected: $*" >&2; exit 125 ;;
esac"#,
        );
        const NO_MACHINE: &str = "[]";
        const STOPPED: &str =
            r#"[{"Name":"podman-machine-default","Default":true,"Running":false}]"#;
        const RUNNING: &str =
            r#"[{"Name":"podman-machine-default","Default":true,"Running":true}]"#;
        const NAMED_STOPPED: &str = concat!(
            r#"[{"Name":"podman-machine-default","Default":true,"Running":true},"#,
            r#"{"Name":"axocoatl","Default":false,"Running":false}]"#
        );
        let host = (
            "CONTAINER_HOST",
            "unix:///run/axocoatl-test/podman-api.sock",
        );
        let named = |name| ("CONTAINER_CONNECTION", name);
        let machines = |list| ("SHIM_MACHINES", list);
        let answers = ("SHIM_ANSWERS", "1");
        let host_down = "Unavailable(\"CONTAINER_HOST=unix:///run/axocoatl-test/podman-api.sock \
                         is set, and Podman does not answer through it: ";
        let named_down = "Unavailable(\"CONTAINER_CONNECTION=axocoatl is set, and Podman does \
                          not answer through it: ";
        let default_down = "Unavailable(\"OCI container setup failed: podman info failed: \
                            Cannot connect to Podman\")";
        let empty_host_down =
            "Unavailable(\"CONTAINER_HOST= is set, and Podman does not answer through it: ";
        let empty_connection_down =
            "Unavailable(\"CONTAINER_CONNECTION= is set, and Podman does not answer through it: ";
        let mac = needs_machine();
        // (what the case is, its variables, the verdict on macOS, on Linux)
        let cases = [
            (
                "CONTAINER_HOST answers, no machine is listed",
                vec![host, machines(NO_MACHINE), answers],
                "Ready",
                "Ready",
            ),
            (
                "CONTAINER_HOST does not answer, no machine is listed",
                vec![host, machines(NO_MACHINE)],
                host_down,
                host_down,
            ),
            (
                "CONTAINER_HOST does not answer, the default machine is stopped",
                vec![host, machines(STOPPED)],
                host_down,
                host_down,
            ),
            (
                "CONTAINER_CONNECTION answers and is no machine",
                vec![named("remote-box"), machines(NO_MACHINE), answers],
                "Ready",
                "Ready",
            ),
            (
                "CONTAINER_CONNECTION names a stopped machine",
                vec![named("axocoatl"), machines(NAMED_STOPPED)],
                "MachineStopped",
                named_down,
            ),
            (
                "CONTAINER_CONNECTION names a stopped machine, a stale CONTAINER_HOST is set too",
                vec![named("axocoatl"), host, machines(NAMED_STOPPED)],
                "MachineStopped",
                named_down,
            ),
            (
                "both are set and Podman answers",
                vec![named("remote-box"), host, machines(NO_MACHINE), answers],
                "Ready",
                "Ready",
            ),
            (
                "no override, the default machine runs and answers",
                vec![machines(RUNNING), answers],
                "Ready",
                "Ready",
            ),
            (
                "no override, no machine",
                vec![machines(NO_MACHINE)],
                "MachineMissing",
                default_down,
            ),
            (
                "no override, the default machine is stopped",
                vec![machines(STOPPED)],
                "MachineStopped",
                default_down,
            ),
            (
                "no override, the default machine runs and does not answer",
                vec![machines(RUNNING)],
                default_down,
                default_down,
            ),
            (
                "CONTAINER_HOST is set but empty, the default machine is stopped",
                vec![("CONTAINER_HOST", ""), machines(STOPPED)],
                "MachineStopped",
                empty_host_down,
            ),
            (
                "CONTAINER_CONNECTION is set but empty, the default machine is stopped",
                vec![named(""), machines(STOPPED)],
                "MachineStopped",
                empty_connection_down,
            ),
            (
                "both are set but empty, and Podman answers",
                vec![
                    named(""),
                    ("CONTAINER_HOST", ""),
                    machines(RUNNING),
                    answers,
                ],
                "Ready",
                "Ready",
            ),
        ];
        for (case, env, on_mac, on_linux) in cases {
            let expected = if mac { on_mac } else { on_linux };
            let _ = std::fs::remove_file(directory.path().join("calls.log"));
            let detected = verdict("detect", &podman, &env).await;
            let detect_calls = calls(directory.path());
            let reported = verdict("report", &podman, &env).await;
            assert!(
                detected.starts_with(expected),
                "{case}: detect said {detected}, want {expected}"
            );
            assert_eq!(
                detected, reported,
                "{case}: the daemon and onboarding disagree"
            );
            // CONTAINER_CONNECTION wins over CONTAINER_HOST, as in Podman.
            if env
                .iter()
                .any(|(name, value)| *name == "CONTAINER_HOST" && !value.is_empty())
                && !env.iter().any(|(name, _)| *name == "CONTAINER_CONNECTION")
            {
                assert!(
                    !detect_calls.iter().any(|call| call.starts_with("machine")),
                    "{case}: a CONTAINER_HOST setup never asks about machines: {detect_calls:?}"
                );
            }
        }
    }

    /// Podman reads `CONTAINER_CONNECTION` before `CONTAINER_HOST`, and an
    /// empty value as unset, to choose the connection
    /// (`setupRemoteConnection`, `cmd/podman/root.go`, 4.9.3 to 6.1.4); so
    /// does Axocoatl. On Linux a variable set but empty still turns Podman
    /// remote (`IsRemote`), and Axocoatl reads it as an override.
    #[test]
    fn container_connection_wins_over_container_host_as_in_podman() {
        use std::ffi::OsStr;
        let read_on = |connection: Option<&str>, host: Option<&str>, always_remote| {
            ConnectionOverride::from_values(
                connection.map(OsStr::new),
                host.map(OsStr::new),
                always_remote,
            )
        };
        let read = |connection: Option<&str>, host: Option<&str>| {
            let on_mac = read_on(connection, host, true);
            let on_linux = read_on(connection, host, false);
            if host.is_some_and(str::is_empty) || connection.is_some_and(str::is_empty) {
                // Only an empty value reads differently.
            } else {
                assert_eq!(on_mac, on_linux, "{connection:?} {host:?}");
            }
            on_mac
        };
        assert_eq!(
            read(Some("axocoatl"), Some("unix:///run/x.sock")),
            ConnectionOverride::Connection("axocoatl".into())
        );
        assert_eq!(
            read(None, Some("unix:///run/x.sock")),
            ConnectionOverride::Host("unix:///run/x.sock".into())
        );
        assert_eq!(
            read(Some(""), Some("unix:///run/x.sock")),
            ConnectionOverride::Host("unix:///run/x.sock".into())
        );
        assert_eq!(
            read(Some("axocoatl"), Some("")),
            ConnectionOverride::Connection("axocoatl".into())
        );
        assert_eq!(read(Some(""), Some("")), ConnectionOverride::None);
        assert_eq!(read(None, None), ConnectionOverride::None);
        assert_eq!(read_on(None, None, false), ConnectionOverride::None);
        // Set but empty: the same as unset where the client is always
        // remote, the default remote connection on Linux.
        assert_eq!(read_on(None, Some(""), true), ConnectionOverride::None);
        assert_eq!(read_on(Some(""), None, true), ConnectionOverride::None);
        assert_eq!(
            read_on(None, Some(""), false),
            ConnectionOverride::Host(String::new())
        );
        assert_eq!(
            read_on(Some(""), Some(""), false),
            ConnectionOverride::Host(String::new())
        );
        assert_eq!(
            read_on(Some(""), None, false),
            ConnectionOverride::Connection(String::new())
        );
        assert_eq!(
            read_on(Some(""), Some("unix:///run/x.sock"), false),
            ConnectionOverride::Host("unix:///run/x.sock".into())
        );
        assert_eq!(
            read_on(Some("axocoatl"), Some(""), false),
            ConnectionOverride::Connection("axocoatl".into())
        );
        assert_eq!(
            ConnectionOverride::Host(String::new())
                .describe()
                .as_deref(),
            Some("CONTAINER_HOST=")
        );
        // Podman does not trim either: such a name is not found, and says so.
        assert_eq!(
            read(Some(" axocoatl"), None),
            ConnectionOverride::Connection(" axocoatl".into())
        );
    }

    /// The machine `ensure_ready` starts is named: the one
    /// `CONTAINER_CONNECTION` names, else the default, else the only one.
    #[test]
    fn the_machine_to_start_is_named() {
        let machine = |name: &str, default: bool| crate::podman_report::MachineInfo {
            name: name.to_string(),
            default,
            ..Default::default()
        };
        let none = ConnectionOverride::None;
        assert_eq!(
            machine_to_start(&[machine("dev", false)], &none).as_deref(),
            Some("dev")
        );
        assert_eq!(
            machine_to_start(&[machine("a", false), machine("b", true)], &none).as_deref(),
            Some("b")
        );
        assert_eq!(
            machine_to_start(&[machine("a", false), machine("b", false)], &none),
            None
        );
        assert_eq!(
            machine_to_start(
                &[machine("a", true), machine("axocoatl", false)],
                &ConnectionOverride::Connection("axocoatl".into())
            )
            .as_deref(),
            Some("axocoatl")
        );
        assert_eq!(
            machine_to_start(
                &[machine("a", true)],
                &ConnectionOverride::Connection("remote".into())
            ),
            None
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

    /// Session start, `axocoatl run`, `recipe build`, `connect codex`, the
    /// browser containers and SearXNG go through `ensure_ready`: a Podman
    /// that fails for want of `newuidmap` gets what doctor and the
    /// workbench say (the packages, then `podman system migrate`), not
    /// only Podman's own message, which says to run `podman system
    /// migrate` alone. Another error is passed on as Podman wrote it.
    #[test]
    fn a_podman_without_its_id_map_helpers_is_told_what_to_install() {
        for error in [
            "OCI container setup failed: podman info failed: Error: command required for \
             rootless mode with multiple IDs: exec: \"newuidmap\": executable file not found in \
             $PATH",
            r#"OCI container setup failed: podman info failed: time="2026-10-20T20:01:02-07:00" level=error msg="invalid internal status, try resetting the pause process with \"podman system migrate\": command required for rootless mode with multiple IDs: exec: \"newuidmap\": executable file not found in $PATH""#,
        ] {
            let ReadyAction::Fail(message) =
                ready_action(PodmanReadiness::Unavailable(error.to_string()))
            else {
                panic!("an unavailable Podman must fail");
            };
            assert_eq!(
                message,
                "Podman did not answer (command required for rootless mode with multiple IDs: \
                 exec: \"newuidmap\": executable file not found in $PATH). Rootless Podman \
                 needs newuidmap and newgidmap to map your subordinate user and group IDs, and \
                 newuidmap is not found on PATH: install (or reinstall) them (Debian and Ubuntu: \
                 uidmap; Fedora and RHEL: shadow-utils; Arch: shadow), then run `podman system \
                 migrate`; `axocoatl doctor` names the command for this computer"
            );
            assert!(!message.contains("did not complete safely"), "{message}");
        }
        let ReadyAction::Fail(message) = ready_action(PodmanReadiness::Unavailable(
            "Error: cannot set up namespace using \"/usr/bin/newuidmap\": should have setuid or \
             have filecaps setuid: exit status 1"
                .to_string(),
        )) else {
            panic!("an unavailable Podman must fail");
        };
        assert!(
            message.contains(
                "/usr/bin/newuidmap is neither setuid nor given file capabilities: install (or \
                 reinstall) them"
            ),
            "{message}"
        );
        assert_eq!(
            ready_action(PodmanReadiness::Unavailable(
                "Cannot connect to Podman".to_string()
            )),
            ReadyAction::Fail(
                "podman readiness check did not complete safely: Cannot connect to Podman"
                    .to_string()
            )
        );
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
