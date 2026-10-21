//! Step 0: what this computer has. Nothing here changes anything: every
//! command it runs only reads (`--version`, `podman info`, `podman machine
//! list`, `GET /api/version`).

use std::path::{Path, PathBuf};

use axocoatl_isolation::podman_report::{self, PodmanReport};

use super::host::{free_bytes, port_free, Cmd, Host};
use crate::ollama_setup;

/// `/etc/os-release`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OsRelease {
    pub(crate) id: String,
    pub(crate) id_like: Vec<String>,
    pub(crate) version_id: String,
    pub(crate) pretty_name: String,
}

impl OsRelease {
    pub(crate) fn parse(text: &str) -> Self {
        let mut release = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            match key.trim() {
                "ID" => release.id = value.to_ascii_lowercase(),
                "ID_LIKE" => {
                    release.id_like = value
                        .split_whitespace()
                        .map(str::to_ascii_lowercase)
                        .collect()
                }
                "VERSION_ID" => release.version_id = value,
                "PRETTY_NAME" => release.pretty_name = value,
                _ => {}
            }
        }
        release
    }

    fn is(&self, family: &str) -> bool {
        self.id == family || self.id_like.iter().any(|like| like == family)
    }

    /// The package manager that installs Podman here.
    pub(crate) fn package_manager(&self) -> Option<PackageManager> {
        if self.is("debian") || self.is("ubuntu") {
            Some(PackageManager::Apt)
        } else if self.is("fedora") || self.is("rhel") || self.is("centos") {
            Some(PackageManager::Dnf)
        } else if self.is("arch") {
            Some(PackageManager::Pacman)
        } else if self.is("suse") || self.id.starts_with("opensuse") || self.is("sles") {
            Some(PackageManager::Zypper)
        } else {
            None
        }
    }

    /// Ubuntu 22.04 (and older), whose Podman (3.4.4) Axocoatl cannot use.
    pub(crate) fn podman_too_old(&self) -> bool {
        self.id == "ubuntu"
            && self
                .version_id
                .split('.')
                .next()
                .and_then(|major| major.parse::<u32>().ok())
                .is_some_and(|major| major <= 22)
    }

    pub(crate) fn name(&self) -> String {
        if !self.pretty_name.is_empty() {
            self.pretty_name.clone()
        } else if !self.id.is_empty() {
            format!("{} {}", self.id, self.version_id)
                .trim()
                .to_string()
        } else {
            "Linux".to_string()
        }
    }
}

/// How Podman (and other system packages) are installed on this Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageManager {
    Apt,
    Dnf,
    Pacman,
    Zypper,
}

impl PackageManager {
    pub(crate) fn install(self, packages: &[&str]) -> Cmd {
        let mut args: Vec<&str> = match self {
            PackageManager::Apt => vec!["apt-get", "install", "-y"],
            PackageManager::Dnf => vec!["dnf", "install", "-y"],
            PackageManager::Pacman => vec!["pacman", "-S", "--noconfirm"],
            PackageManager::Zypper => vec!["zypper", "install", "-y"],
        };
        args.extend_from_slice(packages);
        Cmd::sudo(args[0], &args[1..])
    }

    /// The package with `newuidmap` and `newgidmap`, which rootless Podman
    /// maps subordinate ids with: `uidmap` on Debian and Ubuntu (Podman
    /// only recommends it), `shadow-utils` on Fedora, `shadow` on Arch (as
    /// `rpm -qf` and `pacman -Qo` say in their images). openSUSE moved them
    /// from `shadow` to `shadow-pw-mgmt` in Tumbleweed, so no one name
    /// fits every release: `None`.
    pub(crate) fn id_map_package(self) -> Option<&'static str> {
        match self {
            PackageManager::Apt => Some("uidmap"),
            PackageManager::Dnf => Some("shadow-utils"),
            PackageManager::Pacman => Some("shadow"),
            PackageManager::Zypper => None,
        }
    }
}

/// Windows Subsystem for Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wsl {
    One,
    Two,
}

/// WSL from `/proc/sys/kernel/osrelease` (`…-microsoft-standard-WSL2`;
/// WSL 1 reports `…-Microsoft`) or `WSL_DISTRO_NAME`.
pub(crate) fn wsl_from(kernel_release: &str, distro: Option<&str>) -> Option<Wsl> {
    let lower = kernel_release.to_ascii_lowercase();
    if lower.contains("microsoft-standard") || lower.contains("wsl2") {
        Some(Wsl::Two)
    } else if kernel_release.contains("Microsoft") {
        Some(Wsl::One)
    } else if lower.contains("microsoft") || distro.is_some_and(|d| !d.is_empty()) {
        Some(Wsl::Two)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Os {
    MacOs {
        version: Option<String>,
        apple_silicon: bool,
        chip: Option<String>,
    },
    Linux {
        release: OsRelease,
        wsl: Option<Wsl>,
        /// systemd runs (`/run/systemd/system`): the always-on service and
        /// Ollama's own service are available.
        systemd: bool,
    },
}

impl Os {
    pub(crate) fn is_macos(&self) -> bool {
        matches!(self, Os::MacOs { .. })
    }

    pub(crate) fn release(&self) -> Option<&OsRelease> {
        match self {
            Os::Linux { release, .. } => Some(release),
            Os::MacOs { .. } => None,
        }
    }

    pub(crate) fn wsl(&self) -> Option<Wsl> {
        match self {
            Os::Linux { wsl, .. } => *wsl,
            Os::MacOs { .. } => None,
        }
    }
}

/// How the Ollama server at the chosen address is run, which decides how
/// onboarding restarts it and where its settings go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OllamaService {
    /// `brew services` (a LaunchAgent running as this user, reading
    /// `~/.ollama/server.json` and Homebrew's `services/ollama.env`).
    Brew,
    /// The Ollama macOS app.
    App,
    /// The systemd unit Ollama's Linux installer creates (`User=ollama`).
    Systemd,
    /// Something else, such as `ollama serve` in a terminal.
    Manual,
}

/// The port in an `OLLAMA_HOST` value (`127.0.0.1:11440`,
/// `http://localhost:11440`, `0.0.0.0`): 11434 when it names none.
pub(crate) fn ollama_host_port(value: &str) -> u16 {
    let value = value.trim().trim_matches('"');
    let without_scheme = value.split_once("://").map_or(value, |(_, rest)| rest);
    let authority = without_scheme.split('/').next().unwrap_or_default();
    authority
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(11434)
}

/// The port of a base URL (`http://localhost:11434`).
pub(crate) fn url_port(base_url: &str) -> u16 {
    let without_scheme = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest);
    let authority = without_scheme.split('/').next().unwrap_or_default();
    authority
        .rsplit_once(':')
        .filter(|(host, _)| !host.ends_with(']') || authority.starts_with('['))
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(if base_url.starts_with("https://") {
            443
        } else {
            80
        })
}

/// Homebrew's per-user service environment directory:
/// `$XDG_CONFIG_HOME/homebrew/services`, else `~/.homebrew/services`
/// (Homebrew 7 reads `<formula>.env` there on `brew services start` and
/// `restart`).
pub(crate) fn homebrew_services_dir(home: &Path, xdg_config_home: Option<&Path>) -> PathBuf {
    match xdg_config_home.filter(|path| path.is_absolute()) {
        Some(config) => config.join("homebrew").join("services"),
        None => home.join(".homebrew").join("services"),
    }
}

/// The value `NAME=value` sets in an env file's text, or in a plist's
/// `EnvironmentVariables` (`<key>NAME</key><string>value</string>`).
pub(crate) fn variable_in(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some(value) = line.strip_prefix(&format!("{name}=")) {
            return Some(value.trim().trim_matches('"').to_string());
        }
        // systemd: Environment="NAME=value" or Environment=NAME=value
        if let Some(rest) = line.strip_prefix("Environment=") {
            let rest = rest.trim().trim_matches('"');
            if let Some(value) = rest.strip_prefix(&format!("{name}=")) {
                return Some(value.trim_matches('"').to_string());
            }
        }
    }
    let key = format!("<key>{name}</key>");
    let after = text.split_once(&key)?.1;
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")?;
    Some(after[start..start + end].trim().to_string())
}

/// The LaunchAgent names Homebrew gives the Ollama service: `sh.brew.*` since
/// Homebrew 5, `homebrew.mxcl.*` before.
pub(crate) const BREW_OLLAMA_PLISTS: [&str; 2] =
    ["sh.brew.ollama.plist", "homebrew.mxcl.ollama.plist"];

/// The brew service's LaunchAgent: the one that exists, else the current name.
pub(crate) fn brew_plist(host: &Host) -> PathBuf {
    let agents = host.home.join("Library").join("LaunchAgents");
    BREW_OLLAMA_PLISTS
        .iter()
        .map(|name| agents.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| agents.join(BREW_OLLAMA_PLISTS[0]))
}

/// The Ollama app, if installed.
pub(crate) fn ollama_app(host: &Host) -> Option<PathBuf> {
    [
        host.at("/Applications/Ollama.app"),
        host.home.join("Applications").join("Ollama.app"),
    ]
    .into_iter()
    .find(|path| path.is_dir())
}

/// The systemd unit and drop-in directory Ollama's installer uses.
pub(crate) const OLLAMA_UNIT: &str = "/etc/systemd/system/ollama.service";
pub(crate) const OLLAMA_DROP_IN_DIR: &str = "/etc/systemd/system/ollama.service.d";

/// Which service serves Ollama on `port`.
pub(crate) fn ollama_service(
    host: &Host,
    xdg_config_home: Option<&Path>,
    port: u16,
) -> OllamaService {
    if host.macos {
        let plist = brew_plist(host);
        if plist.is_file() && host.find("brew").is_some() {
            let env_file = homebrew_services_dir(&host.home, xdg_config_home).join("ollama.env");
            let configured = std::fs::read_to_string(&env_file)
                .ok()
                .and_then(|text| variable_in(&text, "OLLAMA_HOST"))
                .or_else(|| {
                    std::fs::read_to_string(&plist)
                        .ok()
                        .and_then(|text| variable_in(&text, "OLLAMA_HOST"))
                });
            if configured.as_deref().map_or(11434, ollama_host_port) == port {
                return OllamaService::Brew;
            }
        }
        if ollama_app(host).is_some() && port == 11434 {
            return OllamaService::App;
        }
        OllamaService::Manual
    } else {
        let unit = host.at(OLLAMA_UNIT);
        if unit.is_file() {
            let mut configured = std::fs::read_to_string(&unit)
                .ok()
                .and_then(|text| variable_in(&text, "OLLAMA_HOST"));
            if let Ok(entries) = std::fs::read_dir(host.at(OLLAMA_DROP_IN_DIR)) {
                let mut drop_ins: Vec<PathBuf> = entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "conf"))
                    .collect();
                drop_ins.sort();
                for drop_in in drop_ins {
                    if let Some(value) = std::fs::read_to_string(&drop_in)
                        .ok()
                        .and_then(|text| variable_in(&text, "OLLAMA_HOST"))
                    {
                        configured = Some(value);
                    }
                }
            }
            if configured.as_deref().map_or(11434, ollama_host_port) == port {
                return OllamaService::Systemd;
            }
        }
        OllamaService::Manual
    }
}

/// The `ollama` CLI and the versions `ollama --version` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OllamaCli {
    pub(crate) path: PathBuf,
    /// The CLI's own version.
    pub(crate) client: Option<String>,
    /// The server it reached (`OLLAMA_HOST`), when it reached one.
    pub(crate) server: Option<String>,
}

/// `ollama --version`: `ollama version is 0.20.6` (the server), then
/// `Warning: client version is 0.40.6` when the CLI differs; without a
/// server only the client line.
pub(crate) fn parse_ollama_version(text: &str) -> (Option<String>, Option<String>) {
    let mut server = None;
    let mut client = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(version) = line.strip_prefix("Warning: client version is ") {
            client = Some(version.trim().to_string());
        } else if let Some(version) = line.strip_prefix("ollama version is ") {
            server = Some(version.trim().to_string());
        }
    }
    let client = client.or_else(|| server.clone());
    (client, server)
}

/// The state of the Ollama server at the chosen address.
#[derive(Debug, Clone)]
pub(crate) enum OllamaServer {
    Unreachable(String),
    Reachable {
        check: Result<axocoatl_llm_ollama::NativeOllamaServerCheck, String>,
        models: Vec<ollama_setup::InstalledModel>,
    },
}

impl OllamaServer {
    pub(crate) async fn probe(base_url: &str) -> Self {
        match ollama_setup::probe_server(base_url).await {
            ollama_setup::ServerProbe::Unreachable(error) => Self::Unreachable(error),
            ollama_setup::ServerProbe::Reachable { models, check } => {
                Self::Reachable { check, models }
            }
        }
    }

    pub(crate) fn version(&self) -> Option<&str> {
        match self {
            Self::Reachable {
                check: Ok(check), ..
            } => check.version.as_deref(),
            _ => None,
        }
    }

    pub(crate) fn models(&self) -> &[ollama_setup::InstalledModel] {
        match self {
            Self::Reachable { models, .. } => models,
            Self::Unreachable(_) => &[],
        }
    }
}

/// What Step 0 found.
#[derive(Debug, Clone)]
pub(crate) struct Facts {
    pub(crate) os: Os,
    pub(crate) memory_bytes: Option<u64>,
    pub(crate) cpus: u64,
    /// Free space where `~/.ollama` lives (the models).
    pub(crate) disk_free_bytes: Option<u64>,
    /// Free space where Podman keeps its machine and images, when that is
    /// another file system than the models'.
    pub(crate) containers_free_bytes: Option<u64>,
    pub(crate) root_user: bool,
    pub(crate) user: String,
    pub(crate) port_8080_free: bool,
    /// `git --version`, or `None` when git is missing (on macOS: the
    /// Command Line Tools shim without the tools).
    pub(crate) git: Option<String>,
    pub(crate) brew: Option<String>,
    pub(crate) podman_path: Option<PathBuf>,
    pub(crate) podman: PodmanReport,
    pub(crate) ollama_cli: Option<OllamaCli>,
    pub(crate) ollama_url: String,
    pub(crate) ollama_server: OllamaServer,
    pub(crate) ollama_service: OllamaService,
    pub(crate) claude: Option<(PathBuf, Option<String>)>,
    pub(crate) codex: Option<PathBuf>,
    pub(crate) gh: Option<PathBuf>,
    /// An `ollama.exe` on PATH inside WSL: the Windows app, which WSL in NAT
    /// mode cannot reach.
    pub(crate) windows_ollama: bool,
    /// A daemon answers on this user's IPC socket.
    pub(crate) daemon_running: bool,
    /// The running daemon's version (`GET /health`), when it answers.
    pub(crate) daemon_version: Option<String>,
    /// The program the installed Always-On Service runs.
    pub(crate) service_program: Option<PathBuf>,
    pub(crate) xdg_config_home: Option<PathBuf>,
    /// The kernel's Landlock ABI (Linux; `None` elsewhere or without it).
    pub(crate) landlock_abi: Option<i64>,
}

impl Facts {
    pub(crate) fn memory_gb(&self) -> Option<u64> {
        self.memory_bytes.map(axocoatl_isolation::host::memory_gb)
    }
}

fn first_word_version(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|word| word.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(|word| word.trim_end_matches(',').to_string())
}

/// Where Podman keeps its machines and images for this user.
pub(crate) fn containers_dir(home: &Path) -> PathBuf {
    home.join(".local").join("share").join("containers")
}

/// The nearest existing path at or above `path`.
fn existing_ancestor(path: &Path) -> Option<&Path> {
    let mut existing = path;
    while !existing.exists() {
        existing = existing.parent()?;
    }
    Some(existing)
}

/// `a` and `b` are on one file system.
fn same_file_system(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let device = |path: &Path| {
            existing_ancestor(path)
                .and_then(|path| std::fs::metadata(path).ok())
                .map(|metadata| metadata.dev())
        };
        device(a).is_some() && device(a) == device(b)
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        true
    }
}

/// Read this computer. `ollama_url` is the server native Sessions will use;
/// `check_port_8080`: whether to see if port 8080 is free (not when
/// `--port` names the port).
pub(crate) async fn detect(host: &Host, ollama_url: &str, check_port_8080: bool) -> Facts {
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    let os = if host.macos {
        let version = host
            .probe("/usr/bin/sw_vers", &["-productVersion"])
            .await
            .filter(|probe| probe.success)
            .map(|probe| probe.stdout.trim().to_string());
        let chip = host
            .probe("/usr/sbin/sysctl", &["-n", "machdep.cpu.brand_string"])
            .await
            .filter(|probe| probe.success)
            .map(|probe| probe.stdout.trim().to_string())
            .filter(|chip| !chip.is_empty());
        Os::MacOs {
            version,
            apple_silicon: std::env::consts::ARCH == "aarch64",
            chip,
        }
    } else {
        let release = OsRelease::parse(
            &std::fs::read_to_string(host.at("/etc/os-release")).unwrap_or_default(),
        );
        let kernel =
            std::fs::read_to_string(host.at("/proc/sys/kernel/osrelease")).unwrap_or_default();
        let wsl = wsl_from(&kernel, std::env::var("WSL_DISTRO_NAME").ok().as_deref());
        Os::Linux {
            release,
            wsl,
            systemd: host.at("/run/systemd/system").is_dir(),
        }
    };
    let git = match host.probe("git", &["--version"]).await {
        Some(probe) if probe.success => first_word_version(&probe.stdout),
        _ => None,
    };
    let brew = homebrew_version(host).await;
    let podman_path = host.find("podman");
    let user = current_user();
    let mut podman = match &podman_path {
        Some(path) => {
            podman_report::inspect_through(&path.to_string_lossy(), host.podman_connection.clone())
                .await
        }
        None => PodmanReport::not_installed(),
    };
    crate::doctor::read_id_ranges(&mut podman, host, &host.root, &os, &user);
    let ollama_cli = match host.find("ollama") {
        Some(path) => {
            let host_value = ollama_url.to_string();
            let probe = host
                .probe_env(
                    &path.to_string_lossy(),
                    &["--version"],
                    &[("OLLAMA_HOST", host_value.as_str())],
                )
                .await;
            let (client, server) = probe
                .map(|probe| parse_ollama_version(&format!("{}\n{}", probe.stdout, probe.stderr)))
                .unwrap_or((None, None));
            Some(OllamaCli {
                path,
                client,
                server,
            })
        }
        None => None,
    };
    let ollama_server = OllamaServer::probe(ollama_url).await;
    let claude =
        crate::connect_cmd::find_claude(None, Some(&host.run_path()), Some(&host.home)).ok();
    let claude = match claude {
        Some(path) => {
            let version = host
                .probe(&path.to_string_lossy(), &["--version"])
                .await
                .filter(|probe| probe.success)
                .and_then(|probe| first_word_version(&probe.stdout));
            Some((path, version))
        }
        None => None,
    };
    let daemon_running =
        axocoatl_daemon::ipc::IpcClient::connect(&axocoatl_daemon::ipc::default_socket_path())
            .await
            .is_ok();
    let models_dir = host.home.join(".ollama");
    let containers = containers_dir(&host.home);
    Facts {
        memory_bytes: axocoatl_isolation::host::memory_bytes(),
        cpus: axocoatl_isolation::host::cpu_count(),
        disk_free_bytes: free_bytes(&models_dir),
        containers_free_bytes: (!same_file_system(&models_dir, &containers))
            .then(|| free_bytes(&containers))
            .flatten(),
        root_user: is_root(),
        user,
        port_8080_free: !check_port_8080 || port_free(8080),
        git,
        brew,
        podman_path,
        podman,
        ollama_service: ollama_service(host, xdg_config_home.as_deref(), url_port(ollama_url)),
        ollama_cli,
        ollama_url: ollama_url.to_string(),
        ollama_server,
        claude,
        codex: host.find("codex"),
        gh: host.find("gh"),
        windows_ollama: os.wsl().is_some() && host.find("ollama.exe").is_some(),
        daemon_running,
        daemon_version: None,
        service_program: crate::doctor::installed_service_program(&host.home, host.macos),
        xdg_config_home,
        landlock_abi: landlock_abi(),
        os,
    }
}

/// `brew --version`, on macOS only, where onboarding installs Podman and
/// Ollama with Homebrew. On Linux nothing runs `brew`, or any package
/// manager, except an install the user agreed to.
pub(crate) async fn homebrew_version(host: &Host) -> Option<String> {
    if !host.macos {
        return None;
    }
    match host.probe("brew", &["--version"]).await {
        Some(probe) if probe.success => probe.stdout.lines().next().and_then(first_word_version),
        _ => None,
    }
}

pub(crate) fn is_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid takes no arguments and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

pub(crate) fn current_user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|user| !user.is_empty())
        .or_else(|| {
            std::process::Command::new("id")
                .arg("-un")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "$USER".to_string())
}

/// The kernel's Landlock ABI version on Linux, read with
/// `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`;
/// `None` elsewhere or when the kernel has no Landlock.
pub(crate) fn landlock_abi() -> Option<i64> {
    #[cfg(target_os = "linux")]
    {
        const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
        // SAFETY: with a null attribute pointer, a size of zero and only the
        // version flag, the call reads and writes no memory: it returns the
        // ABI version or -1.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<libc::c_void>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        // `c_long` is `i64` on the 64-bit targets Axocoatl ships for.
        #[allow(clippy::unnecessary_cast)]
        let abi = abi as i64;
        (abi > 0).then_some(abi)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn gb(bytes: u64) -> String {
    format!("{} GB", bytes / 1_000_000_000)
}

/// Step 0's lines.
pub(crate) fn summary(facts: &Facts) -> Vec<String> {
    let mut computer = Vec::new();
    match &facts.os {
        Os::MacOs {
            version,
            apple_silicon,
            chip,
        } => {
            computer.push(format!(
                "macOS {}",
                version.as_deref().unwrap_or("(version unknown)")
            ));
            computer.push(chip.clone().unwrap_or_else(|| {
                if *apple_silicon {
                    "Apple silicon".to_string()
                } else {
                    "Intel".to_string()
                }
            }));
        }
        Os::Linux { release, wsl, .. } => {
            computer.push(match wsl {
                Some(Wsl::Two) => format!("{} (WSL2)", release.name()),
                Some(Wsl::One) => format!("{} (WSL 1)", release.name()),
                None => release.name(),
            });
            computer.push(std::env::consts::ARCH.to_string());
        }
    }
    if let Some(gb) = facts.memory_gb() {
        computer.push(if facts.os.wsl().is_some() {
            format!("{gb} GB memory (by default WSL gives Linux half of Windows' memory)")
        } else {
            format!("{gb} GB memory")
        });
    }
    computer.push(format!("{} cores", facts.cpus));
    match (facts.disk_free_bytes, facts.containers_free_bytes) {
        (Some(free), Some(containers)) => computer.push(format!(
            "{} free for models, {} free for Podman",
            gb(free),
            gb(containers)
        )),
        (Some(free), None) => computer.push(format!("{} free", gb(free))),
        (None, _) => {}
    }
    let mut found = Vec::new();
    let mut missing = Vec::new();
    match &facts.brew {
        Some(version) => found.push(format!("Homebrew {version}")),
        None if facts.os.is_macos() => missing.push("Homebrew".to_string()),
        None => {}
    }
    match &facts.podman.client_version {
        Some(version) => {
            let state = match &facts.podman.readiness {
                axocoatl_isolation::podman::PodmanReadiness::Ready => "ready".to_string(),
                axocoatl_isolation::podman::PodmanReadiness::MachineMissing => {
                    "no machine".to_string()
                }
                axocoatl_isolation::podman::PodmanReadiness::MachineStopped => {
                    "machine stopped".to_string()
                }
                axocoatl_isolation::podman::PodmanReadiness::Unavailable(_) => {
                    "not answering".to_string()
                }
                axocoatl_isolation::podman::PodmanReadiness::NotInstalled => {
                    "not installed".to_string()
                }
            };
            found.push(format!("Podman {version} ({state})"));
        }
        None if facts.podman_path.is_some() => found.push("Podman (version unknown)".into()),
        None => missing.push("Podman".to_string()),
    }
    match (&facts.ollama_cli, &facts.ollama_server) {
        (_, OllamaServer::Reachable { check, .. }) => {
            let (version, cloud) = match check {
                Ok(check) => (
                    check.version.clone().unwrap_or_else(|| "?".into()),
                    if check.cloud_disabled() {
                        "cloud models off"
                    } else {
                        "cloud models on"
                    },
                ),
                Err(_) => ("?".to_string(), "cloud mode unknown"),
            };
            found.push(format!(
                "Ollama {version} at {} ({cloud})",
                facts.ollama_url
            ));
        }
        (Some(cli), OllamaServer::Unreachable(_)) => found.push(format!(
            "Ollama {} (not running at {})",
            cli.client.as_deref().unwrap_or("?"),
            facts.ollama_url
        )),
        (None, OllamaServer::Unreachable(_)) => missing.push("Ollama".to_string()),
    }
    match &facts.claude {
        Some((_, Some(version))) => found.push(format!("claude {version}")),
        Some((_, None)) => found.push("claude".to_string()),
        None => missing.push("claude".to_string()),
    }
    match &facts.codex {
        Some(_) => found.push("codex".to_string()),
        None => missing.push("codex".to_string()),
    }
    match &facts.gh {
        Some(_) => found.push("gh".to_string()),
        None => missing.push("gh".to_string()),
    }
    match &facts.git {
        Some(version) => found.push(format!("git {version}")),
        None => missing.push("git".to_string()),
    }
    match (facts.daemon_running, &facts.daemon_version) {
        (true, Some(version)) => found.push(format!("a running Axocoatl {version} daemon")),
        (true, None) => found.push("a running Axocoatl daemon".to_string()),
        (false, _) => {}
    }
    if let Some(program) = &facts.service_program {
        found.push(format!(
            "the Always-On Service ({}, {})",
            if facts.daemon_running {
                "installed"
            } else {
                "installed, not running"
            },
            program.display()
        ));
    }
    let mut lines = vec![format!("This computer: {}.", computer.join(", "))];
    if !found.is_empty() {
        lines.push(format!("Found: {}.", found.join(", ")));
    }
    if !missing.is_empty() {
        lines.push(format!("Not found: {}.", missing.join(", ")));
    }
    if !facts.port_8080_free {
        lines.push("Port 8080 is in use; Axocoatl will use the next free port.".to_string());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::super::host::testing::Shims;
    use super::*;

    #[test]
    fn os_release_chooses_the_package_manager_and_spots_ubuntu_22_04() {
        let ubuntu = OsRelease::parse(
            "PRETTY_NAME=\"Ubuntu 24.04.5 LTS\"\nID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\n",
        );
        assert_eq!(ubuntu.package_manager(), Some(PackageManager::Apt));
        assert!(!ubuntu.podman_too_old());
        assert_eq!(ubuntu.name(), "Ubuntu 24.04.5 LTS");
        let jammy = OsRelease::parse("ID=ubuntu\nVERSION_ID=\"22.04\"\n");
        assert!(jammy.podman_too_old());
        let fedora = OsRelease::parse("ID=fedora\nVERSION_ID=42\n");
        assert_eq!(fedora.package_manager(), Some(PackageManager::Dnf));
        let rocky = OsRelease::parse("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n");
        assert_eq!(rocky.package_manager(), Some(PackageManager::Dnf));
        let arch = OsRelease::parse("ID=arch\n");
        assert_eq!(arch.package_manager(), Some(PackageManager::Pacman));
        let suse = OsRelease::parse("ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n");
        assert_eq!(suse.package_manager(), Some(PackageManager::Zypper));
        assert_eq!(OsRelease::parse("ID=nixos\n").package_manager(), None);
        assert_eq!(
            PackageManager::Apt.install(&["podman"]).display(),
            "sudo apt-get install -y podman"
        );
        assert_eq!(
            PackageManager::Dnf.install(&["podman"]).display(),
            "sudo dnf install -y podman"
        );
        assert_eq!(
            PackageManager::Pacman.install(&["podman"]).display(),
            "sudo pacman -S --noconfirm podman"
        );
        assert_eq!(
            PackageManager::Zypper.install(&["podman"]).display(),
            "sudo zypper install -y podman"
        );
        assert_eq!(PackageManager::Apt.id_map_package(), Some("uidmap"));
        assert_eq!(PackageManager::Dnf.id_map_package(), Some("shadow-utils"));
        assert_eq!(PackageManager::Pacman.id_map_package(), Some("shadow"));
        assert_eq!(PackageManager::Zypper.id_map_package(), None);
    }

    #[test]
    fn wsl_versions_are_told_apart() {
        assert_eq!(
            wsl_from("5.15.167.4-microsoft-standard-WSL2", None),
            Some(Wsl::Two)
        );
        assert_eq!(wsl_from("4.4.0-19041-Microsoft", None), Some(Wsl::One));
        assert_eq!(wsl_from("6.8.0-142-generic", None), None);
        assert_eq!(wsl_from("6.8.0", Some("Ubuntu")), Some(Wsl::Two));
    }

    #[test]
    fn ollama_versions_and_hosts_are_read() {
        assert_eq!(
            parse_ollama_version("ollama version is 0.40.6\n"),
            (Some("0.40.6".into()), Some("0.40.6".into()))
        );
        assert_eq!(
            parse_ollama_version("ollama version is 0.20.6\nWarning: client version is 0.40.6\n"),
            (Some("0.40.6".into()), Some("0.20.6".into()))
        );
        assert_eq!(
            parse_ollama_version(
                "Warning: could not connect to a running Ollama instance\nWarning: client version is 0.40.6\n"
            ),
            (Some("0.40.6".into()), None)
        );
        assert_eq!(ollama_host_port("127.0.0.1:11440"), 11440);
        assert_eq!(ollama_host_port("http://localhost:11441/"), 11441);
        assert_eq!(ollama_host_port("0.0.0.0"), 11434);
        assert_eq!(url_port("http://localhost:11434"), 11434);
        assert_eq!(url_port("http://127.0.0.1:11440/"), 11440);
        assert_eq!(url_port("http://[::1]:11442"), 11442);
        assert_eq!(url_port("http://localhost"), 80);
        assert_eq!(
            variable_in("# c\nOLLAMA_HOST=127.0.0.1:11440\n", "OLLAMA_HOST").as_deref(),
            Some("127.0.0.1:11440")
        );
        assert_eq!(
            variable_in(
                "[Service]\nEnvironment=\"OLLAMA_HOST=0.0.0.0:11500\"\n",
                "OLLAMA_HOST"
            )
            .as_deref(),
            Some("0.0.0.0:11500")
        );
        assert_eq!(
            variable_in(
                "<key>EnvironmentVariables</key><dict><key>OLLAMA_HOST</key>\n<string>127.0.0.1:11450</string>",
                "OLLAMA_HOST"
            )
            .as_deref(),
            Some("127.0.0.1:11450")
        );
    }

    /// The service onboarding restarts is the one that serves the chosen
    /// address: never the brew service or app for another port.
    #[test]
    fn the_service_serving_the_chosen_port_is_found() {
        let shims = Shims::new();
        let host = shims.host(true);
        assert_eq!(ollama_service(&host, None, 11434), OllamaService::Manual);
        std::fs::create_dir_all(host.at("/Applications/Ollama.app")).unwrap();
        assert_eq!(ollama_service(&host, None, 11434), OllamaService::App);
        assert_eq!(ollama_service(&host, None, 11440), OllamaService::Manual);
        shims.add("brew", "exit 0");
        let plist = brew_plist(&host);
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&plist, "<plist/>").unwrap();
        assert_eq!(ollama_service(&host, None, 11434), OllamaService::Brew);
        let env = homebrew_services_dir(&host.home, None).join("ollama.env");
        std::fs::create_dir_all(env.parent().unwrap()).unwrap();
        std::fs::write(&env, "OLLAMA_HOST=127.0.0.1:11440\n").unwrap();
        assert_eq!(ollama_service(&host, None, 11440), OllamaService::Brew);
        assert_eq!(ollama_service(&host, None, 11434), OllamaService::App);

        let linux = shims.host(false);
        assert_eq!(ollama_service(&linux, None, 11434), OllamaService::Manual);
        std::fs::create_dir_all(linux.at(OLLAMA_DROP_IN_DIR)).unwrap();
        std::fs::write(linux.at(OLLAMA_UNIT), "[Service]\nUser=ollama\n").unwrap();
        assert_eq!(ollama_service(&linux, None, 11434), OllamaService::Systemd);
        std::fs::write(
            linux.at(OLLAMA_DROP_IN_DIR).join("host.conf"),
            "[Service]\nEnvironment=\"OLLAMA_HOST=127.0.0.1:11445\"\n",
        )
        .unwrap();
        assert_eq!(ollama_service(&linux, None, 11434), OllamaService::Manual);
        assert_eq!(ollama_service(&linux, None, 11445), OllamaService::Systemd);
    }

    /// Linux onboarding never runs `brew`, even when one is on PATH (the
    /// release gate's guard shims put one there); macOS asks it its
    /// version.
    #[cfg(unix)]
    #[tokio::test]
    async fn only_macos_runs_brew() {
        let shims = Shims::new();
        shims.add("brew", "echo 'Homebrew 7.0.10'");
        assert_eq!(homebrew_version(&shims.host(false)).await, None);
        assert!(shims.calls().is_empty(), "{:?}", shims.calls());
        assert_eq!(
            homebrew_version(&shims.host(true)).await.as_deref(),
            Some("7.0.10")
        );
        assert_eq!(shims.calls(), ["brew --version"]);
    }

    #[test]
    fn homebrew_service_environment_follows_xdg() {
        assert_eq!(
            homebrew_services_dir(Path::new("/Users/a"), None),
            Path::new("/Users/a/.homebrew/services")
        );
        assert_eq!(
            homebrew_services_dir(Path::new("/Users/a"), Some(Path::new("/cfg"))),
            Path::new("/cfg/homebrew/services")
        );
    }

    #[test]
    fn landlock_is_read_only_on_linux() {
        if cfg!(target_os = "linux") {
            // Any kernel CI runs has Landlock or reports it absent; either
            // way the call returns without failing.
            let _ = landlock_abi();
        } else {
            assert_eq!(landlock_abi(), None);
        }
    }
}
