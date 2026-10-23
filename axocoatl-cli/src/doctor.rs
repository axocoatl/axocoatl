//! `axocoatl doctor`, and the check onboarding's Step 5 runs: one report in
//! four sections (Sandbox, Models, Writers, Daemon), each item OK, WARN or
//! FAIL with its fix, then what is ready to run and the steps left.
//! `doctor` exits 0 only when nothing failed. `--json` prints the same
//! report as one JSON document:
//! `{"ready": bool, "config": path, "sections": {"sandbox": [...], "models":
//! [...], "writers": [...], "daemon": [...]}, "ready_for": [...],
//! "steps_left": [...]}`, each item `{"name", "status": "ok|warn|fail",
//! "detail", "fix"}`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use axocoatl_isolation::podman::ConnectionOverride;
use axocoatl_isolation::podman_report::{self, Level, Platform};
use serde::Serialize;

use crate::ollama_setup::{self, OllamaHost};
use crate::onboard::config::{self as onboard_config, ModelRef, Provider};
use crate::onboard::detect::{self, Os};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Item {
    pub(crate) name: String,
    pub(crate) status: Status,
    pub(crate) detail: String,
    pub(crate) fix: Option<String>,
}

impl Item {
    /// The item as one line: its detail, after its name unless the detail
    /// starts by saying what it is about.
    pub(crate) fn line(&self) -> String {
        let name = self.name.to_ascii_lowercase();
        let key = name.trim_start_matches("model ");
        let detail = self.detail.to_ascii_lowercase().replace(['\'', '"'], "");
        if detail.starts_with(&name) || detail.starts_with(key) {
            self.detail.clone()
        } else {
            format!("{}: {}", self.name, self.detail)
        }
    }

    /// The item as a step left: the line and its fix.
    pub(crate) fn step(&self) -> String {
        match &self.fix {
            Some(fix) => format!("{}: {}", self.line(), fix),
            None => self.line(),
        }
    }

    fn ok(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status: Status::Ok,
            detail: detail.into(),
            fix: None,
        }
    }

    fn warn(name: &str, detail: impl Into<String>, fix: Option<String>) -> Self {
        Self {
            name: name.to_string(),
            status: Status::Warn,
            detail: detail.into(),
            fix,
        }
    }

    fn fail(name: &str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status: Status::Fail,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct Sections {
    pub(crate) sandbox: Vec<Item>,
    pub(crate) models: Vec<Item>,
    pub(crate) writers: Vec<Item>,
    pub(crate) daemon: Vec<Item>,
}

impl Sections {
    fn all(&self) -> impl Iterator<Item = &Item> {
        self.sandbox
            .iter()
            .chain(&self.models)
            .chain(&self.writers)
            .chain(&self.daemon)
    }
}

/// One way to run Axocoatl and whether it is ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RunPath {
    pub(crate) command: String,
    pub(crate) ok: bool,
    pub(crate) detail: String,
    /// `false` for an optional writer (Claude Code, Codex) that nothing of
    /// is set up and that the setup did not choose: not ready, but nothing
    /// failed either.
    pub(crate) set_up: bool,
    /// What the setup chose (its writer, the workbench): `false` for an
    /// optional writer, which, partly set up and not ready, is a warning
    /// (`!`), not a failure (`✗`), so a ready setup never lists a `✗`.
    pub(crate) required: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Report {
    pub(crate) ready: bool,
    pub(crate) config: String,
    pub(crate) sections: Sections,
    pub(crate) ready_for: Vec<RunPath>,
    pub(crate) steps_left: Vec<String>,
}

/// How a model on Ollama fared.
#[derive(Debug, Clone, Default)]
struct ModelState {
    ok: bool,
    why: String,
}

/// What the check needs about this computer that a test may give instead.
pub(crate) struct Environment {
    pub(crate) os: Os,
    pub(crate) user: String,
    pub(crate) memory_bytes: Option<u64>,
    pub(crate) cpus: u64,
    pub(crate) home: PathBuf,
    pub(crate) xdg_config_home: Option<PathBuf>,
    /// Find a program on PATH.
    pub(crate) host: crate::onboard::host::Host,
    /// Load each configured Ollama model to read its context.
    pub(crate) load_models: bool,
    /// Models whose context was already read in this process (onboarding's
    /// Step 3), with that context: not loaded again.
    pub(crate) contexts_read: BTreeMap<String, usize>,
    /// The daemon's IPC socket.
    pub(crate) socket_path: PathBuf,
    /// The Podman client.
    pub(crate) podman: String,
    /// The connection `CONTAINER_HOST` or `CONTAINER_CONNECTION` selects
    /// for it.
    pub(crate) podman_connection: ConnectionOverride,
    /// `/`, where the user's subordinate id files are read (a test's
    /// directory instead).
    pub(crate) id_root: PathBuf,
    /// The data directory, when it is not the one the configuration selects
    /// (tests).
    pub(crate) data_dir: Option<PathBuf>,
    /// The kernel's Landlock ABI on Linux.
    pub(crate) landlock_abi: Option<i64>,
    /// This user's systemd manager, on a Linux with systemd.
    pub(crate) user_systemd: Option<detect::UserSystemd>,
    /// SELinux is enforcing.
    pub(crate) selinux_enforcing: bool,
    /// `git --version`'s version; `None` when git is missing (on macOS,
    /// also when the Command Line Tools are, [`detect::git_version`]).
    pub(crate) git: Option<String>,
    /// The proxy variables set here ([`proxy_variables`]), by name.
    pub(crate) proxy_variables: Vec<String>,
    /// Says that a model is being loaded to read its context: doctor
    /// prints a line on stderr; onboarding says it as its own line.
    pub(crate) on_load: Option<OnLoad>,
    /// The default configuration, the one `axocoatl onboard` writes (it
    /// has no `--config`): a fix for another one names it with `--config`,
    /// or says what to change in its file instead of an onboarding command.
    pub(crate) default_config: PathBuf,
}

/// What says a model is being loaded ([`Environment::on_load`]).
pub(crate) type OnLoad = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// The variables that name a proxy for HTTP clients, in the case curl and
/// most tools read.
const PROXY_VARIABLES: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// The proxy variables `lookup` gives a value, by name (never the value,
/// which may hold a password).
pub(crate) fn proxy_variables(lookup: impl Fn(&str) -> Option<String>) -> Vec<String> {
    PROXY_VARIABLES
        .iter()
        .filter(|name| lookup(name).is_some_and(|value| !value.trim().is_empty()))
        .map(|name| name.to_string())
        .collect()
}

/// The warning for a proxy set in the environment: Axocoatl 1.5.0 has no
/// proxy support for Sessions and runs, whose network connects directly
/// whatever it says (only the daemon's own requests to hosted model
/// providers follow it, as its HTTP library reads the variables).
pub(crate) fn proxy_item(variables: &[String]) -> Option<Item> {
    if variables.is_empty() {
        return None;
    }
    let (names, verb) = match variables {
        [one] => (one.clone(), "is"),
        [rest @ .., last] => (format!("{} and {last}", rest.join(", ")), "are"),
        [] => return None,
    };
    Some(Item::warn(
        "Proxy",
        format!(
            "{names} {verb} set, but Axocoatl does not support a proxy for Sessions and runs: \
             a Session's network (a run's dependency install, web tools, Claude Code's and \
             Codex's model calls) connects directly, and its containers do not get the \
             variables; only the daemon's own model calls to OpenRouter or another hosted \
             provider follow them, untested behind a proxy"
        ),
        Some(
            "On a network that reaches the internet only through a proxy, a run cannot install \
             its dependencies or reach a model API from the sandbox: use a network with direct \
             access (https://docs.axocoatl.ai/start/install/#proxies-vpns-and-rate-limits)"
                .to_string(),
        ),
    ))
}

/// For tests of the `axocoatl` binary: a directory that stands in for `/`
/// where doctor reads `etc/subuid`, `etc/subgid` and `etc/nsswitch.conf`,
/// so a test does not depend on the user ranges of the computer running it.
/// Not documented, and read only in debug builds (`cargo test` builds the
/// binary it runs that way): a release build always reads `/`.
#[cfg(debug_assertions)]
const ID_ROOT_VARIABLE: &str = "AXOCOATL_TEST_ID_ROOT";

/// [`ID_ROOT_VARIABLE`], when it is an absolute path in a debug build.
#[cfg(debug_assertions)]
fn test_id_root() -> Option<PathBuf> {
    std::env::var_os(ID_ROOT_VARIABLE)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// A release build reads no test root.
#[cfg(not(debug_assertions))]
fn test_id_root() -> Option<PathBuf> {
    None
}

impl Environment {
    pub(crate) async fn current() -> Result<Self, String> {
        let host = crate::onboard::host::Host::from_env(None)?;
        let id_root = test_id_root().unwrap_or_else(|| host.root.clone());
        let podman_connection = host.podman_connection.clone();
        let os = if host.macos {
            Os::MacOs {
                version: None,
                apple_silicon: std::env::consts::ARCH == "aarch64",
                chip: None,
            }
        } else {
            let release = detect::OsRelease::parse(
                &std::fs::read_to_string("/etc/os-release").unwrap_or_default(),
            );
            let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
            Os::Linux {
                release,
                wsl: detect::wsl_from(&kernel, std::env::var("WSL_DISTRO_NAME").ok().as_deref()),
                systemd: Path::new("/run/systemd/system").is_dir(),
            }
        };
        let user = detect::current_user();
        let user_systemd = match &os {
            Os::Linux { systemd: true, .. } => Some(detect::user_systemd(&host, &user).await),
            _ => None,
        };
        let git = detect::git_version(&host).await;
        let selinux_enforcing = !host.macos && detect::selinux_enforcing(&host);
        Ok(Self {
            os,
            user,
            memory_bytes: axocoatl_isolation::host::memory_bytes(),
            cpus: axocoatl_isolation::host::cpu_count(),
            home: host.home.clone(),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute()),
            host,
            load_models: true,
            contexts_read: BTreeMap::new(),
            socket_path: axocoatl_daemon::ipc::default_socket_path(),
            podman: "podman".to_string(),
            podman_connection,
            id_root,
            data_dir: None,
            landlock_abi: detect::landlock_abi(),
            user_systemd,
            selinux_enforcing,
            git,
            proxy_variables: proxy_variables(|name| std::env::var(name).ok()),
            on_load: None,
            default_config: crate::default_config_path_for_clap(),
        })
    }

    fn platform(&self) -> Platform {
        let spec = axocoatl_isolation::podman::machine_spec(
            self.memory_bytes
                .unwrap_or((axocoatl_isolation::podman::TESTED_MACHINE_MEMORY_MIB * 2) << 20),
            self.cpus,
        );
        Platform {
            machine: self.os.is_macos(),
            user: self.user.clone(),
            install_fix: podman_install_fix(
                &self.os,
                self.os.is_macos() && self.host.find("brew").is_some(),
            ),
            too_old_note: self
                .os
                .release()
                .filter(|release| release.podman_too_old())
                .map(|_| "Ubuntu 22.04 ships Podman 3.4.4; use Ubuntu 24.04 or newer".to_string()),
            spec,
        }
    }
}

/// How to install Podman here; `homebrew`: Homebrew is installed (macOS),
/// so onboarding can install Podman with it. The words are the workbench
/// readiness item's ([`axocoatl_isolation::podman::install_fix`]).
pub(crate) fn podman_install_fix(os: &Os, homebrew: bool) -> String {
    axocoatl_isolation::podman::install_fix(&podman_install_route(os, homebrew))
}

/// What onboarding does about a missing Podman on `os`, as its install step
/// decides it.
pub(crate) fn podman_install_route(
    os: &Os,
    homebrew: bool,
) -> axocoatl_isolation::podman::InstallRoute {
    use axocoatl_isolation::podman::InstallRoute;
    match os {
        Os::MacOs { apple_silicon, .. } => InstallRoute::for_mac(*apple_silicon, homebrew),
        Os::Linux { release, .. } if release.podman_too_old() => InstallRoute::LinuxTooOld,
        Os::Linux { release, .. } => match release.package_manager() {
            Some(manager) => InstallRoute::LinuxPackage(manager.install(&["podman"]).display()),
            None => InstallRoute::LinuxOther,
        },
    }
}

/// Record whether `os` runs Podman in a machine, and on Linux, when Podman
/// runs on this computer ([`PodmanReport::runs_here`]):
/// add to `report` what `/etc/subuid` and `/etc/subgid` (under `root`) give
/// `user`, this process's user, and whether `newuidmap` and `newgidmap` on
/// the host's `PATH` can map them ([`id_map_tools`]). Nothing is read on
/// macOS, whose machine has its own, nor for a Podman reached through
/// `CONTAINER_HOST`, `CONTAINER_CONNECTION` or a remote connection: this
/// computer's files do not govern it, so its own `podman info` decides.
///
/// [`PodmanReport::runs_here`]: podman_report::PodmanReport::runs_here
pub(crate) async fn read_id_ranges(
    report: &mut podman_report::PodmanReport,
    host: &crate::onboard::host::Host,
    root: &Path,
    os: &Os,
    user: &str,
) {
    report.subordinate_ids = None;
    report.id_map_tools = None;
    report.machine_platform = os.is_macos();
    if os.is_macos() || !report.runs_here() {
        return;
    }
    let uid = {
        #[cfg(unix)]
        {
            // SAFETY: getuid takes no arguments and cannot fail.
            Some(unsafe { libc::getuid() })
        }
        #[cfg(not(unix))]
        {
            None
        }
    };
    report.subordinate_ids = podman_report::read_subordinate_ids(root, user, uid);
    report.id_map_tools = Some(id_map_tools(host, root, os).await);
}

/// The system directories a distribution installs `newuidmap` and
/// `newgidmap` to, searched (under the root) for a helper that is not on
/// `PATH`, to say where it is.
const SYSTEM_BIN_DIRECTORIES: [&str; 4] = ["/usr/bin", "/usr/sbin", "/bin", "/sbin"];

/// `newuidmap` and `newgidmap` as rootless Podman finds them: on `PATH`
/// only (the host's, which the Podman that onboarding, doctor and the
/// daemon run inherits), as Podman's `exec.LookPath` looks them up, so a
/// helper in `/usr/bin` that is not on `PATH` counts as missing, as it does
/// for Podman. When one is missing or cannot map, also what is known about
/// why: a missing one found in a system directory under `root` that is not
/// on `PATH`, whether the package that ships them here is installed,
/// whether one that cannot map is a package's file at all, and what a
/// reinstall of the installed package would write ([`reinstall_keeps`]),
/// all read-only (dpkg-query, dpkg-statoverride, rpm, pacman, and dpkg's
/// and dnf's configuration files). With the command that installs that
/// package, or reinstalls it when it is installed.
pub(crate) async fn id_map_tools(
    host: &crate::onboard::host::Host,
    root: &Path,
    os: &Os,
) -> podman_report::IdMapTools {
    let manager = os.release().and_then(|release| release.package_manager());
    let package = manager.and_then(|manager| manager.id_map_package());
    let mut tools = podman_report::IdMapTools {
        package: package.map(str::to_string),
        ..Default::default()
    };
    for name in ["newuidmap", "newgidmap"] {
        match host.find_on_path(name) {
            None => tools.missing.push(name.to_string()),
            Some(path) if !may_map_ids(&path) => {
                tools.unprivileged.push(path.display().to_string())
            }
            Some(_) => {}
        }
    }
    if !tools.usable() {
        let on_path: Vec<PathBuf> = std::env::split_paths(&host.path).collect();
        for name in &tools.missing {
            let found = SYSTEM_BIN_DIRECTORIES
                .iter()
                .filter(|directory| !on_path.iter().any(|path| path == Path::new(directory)))
                .map(|directory| format!("{directory}/{name}"))
                .find(|path| {
                    root.join(path.trim_start_matches('/'))
                        .metadata()
                        .is_ok_and(|metadata| metadata.is_file())
                });
            tools.off_path.extend(found);
        }
        if let (Some(manager), Some(package)) = (manager, package) {
            let (program, args) = manager.package_query(package);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            tools.package_installed = host
                .probe(program, &args)
                .await
                .and_then(|probe| manager.package_installed(probe.code, &probe.stdout));
        }
        // A helper that cannot map is reinstalled only when it is the
        // package's own file. A copy earlier on PATH, or one no package
        // installed, stays as it is: it is to be removed or fixed.
        for path in tools.unprivileged.clone() {
            let real = std::fs::canonicalize(&path)
                .map(|real| real.display().to_string())
                .unwrap_or_else(|_| path.clone());
            let owner = match manager {
                Some(manager) => {
                    let (program, args) = manager.owner_query(&real);
                    let args: Vec<&str> = args.iter().map(String::as_str).collect();
                    host.probe(program, &args).await.map(|probe| {
                        match (package, tools.package_installed) {
                            (Some(package), Some(true)) => {
                                manager.owns(package, probe.code, &probe.stdout)
                            }
                            // Not installed, or not known: only "no package
                            // owns it" (exit 1) tells.
                            _ => (probe.code == Some(1)).then_some(false),
                        }
                    })
                }
                None => None,
            }
            .flatten()
            // Not asked: no distribution package installs a file under
            // /usr/local, /opt or a home directory (a HOME of `/`, as a
            // service account may have, is no home directory).
            .or_else(|| {
                let local = ["/usr/local/", "/opt/"]
                    .iter()
                    .any(|prefix| real.starts_with(prefix))
                    || (host.home != Path::new("/") && Path::new(&real).starts_with(&host.home));
                local.then_some(false)
            });
            if owner == Some(false) {
                tools.foreign.push(path);
            }
        }
    }
    tools.install = manager
        .and_then(|manager| manager.id_map_install(tools.package_installed))
        .map(|install| install.display());
    if let (Some(manager), Some(package), Some(install), Some(true)) = (
        manager,
        package,
        tools.install.clone(),
        tools.package_installed,
    ) {
        tools.reinstall_keeps =
            reinstall_keeps(host, root, manager, package, &install, &tools).await;
        // What the same reinstall did when onboarding ran it, while the
        // helpers are the files they were then: a hook or policy that
        // changes them afterwards shows in no package manager's records.
        if !tools.usable() {
            if let Some(attempt) = crate::onboard::id_map_attempt::recorded(host, &install) {
                tools
                    .reinstall_keeps
                    .push(attempt.words(&tools.unprivileged));
            }
        }
    }
    tools
}

/// Why reinstalling `package` (installed, by `install`) would leave the
/// helpers `tools` found missing or unable to map as they are, from what
/// the package manager records and is set to do, each in words with what
/// to do first (nothing when it would restore them, or when its records
/// were not read). A reinstall writes the package's file where the package
/// manager puts it, with the mode and file capabilities it gives it: dpkg
/// writes a diverted file elsewhere, applies a stat override's mode, skips
/// a path its `path-exclude` filters leave out, and gives the helpers their
/// setuid bit (uidmap ships them setuid root on Debian and Ubuntu); rpm
/// gives the mode and file capabilities in its records, without the
/// capabilities when dnf is set to `nocaps`; pacman skips a path its
/// `NoExtract` patterns name (removing the file there: pacman 7.1 on Arch
/// Linux ARM) and writes a file `NoUpgrade` names next to it as `.pacnew`, and gives the file capabilities in the package
/// (Arch's shadow records newuidmap as mode 755, so its mode says nothing).
/// The same reason for both helpers is said once, naming both.
async fn reinstall_keeps(
    host: &crate::onboard::host::Host,
    root: &Path,
    manager: detect::PackageManager,
    package: &str,
    install: &str,
    tools: &podman_report::IdMapTools,
) -> Vec<String> {
    use detect::PackageManager;
    // The helpers a reinstall was to fix: missing ones that are nowhere
    // here, and the package's own files that cannot map.
    let off_path: Vec<&str> = tools
        .off_path
        .iter()
        .filter_map(|path| Path::new(path).file_name().and_then(|name| name.to_str()))
        .collect();
    let mut wanted: Vec<(&str, bool)> = tools
        .missing
        .iter()
        .filter(|name| !off_path.contains(&name.as_str()))
        .map(|name| (name.as_str(), true))
        .collect();
    for path in tools
        .unprivileged
        .iter()
        .filter(|path| !tools.foreign.contains(path))
    {
        if let Some(name) = Path::new(path).file_name().and_then(|name| name.to_str()) {
            if ["newuidmap", "newgidmap"].contains(&name) {
                wanted.push((name, false));
            }
        }
    }
    if wanted.is_empty() {
        return Vec::new();
    }
    let Some((program, args)) = manager.files_query(package) else {
        return Vec::new();
    };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // An installed package has files: no list says nothing.
    let files = match host.probe(program, &args).await {
        Some(probe) if probe.success => manager.package_files(&probe.stdout),
        _ => Vec::new(),
    };
    if files.is_empty() {
        return Vec::new();
    }
    let then = format!("then reinstall {package} (`{install}`)");
    let mut keeps = Vec::new();
    let mut not_shipped: Vec<&str> = Vec::new();
    let mut no_caps: Vec<String> = Vec::new();
    let mut unprivileged: Vec<String> = Vec::new();
    // dpkg's and pacman's reasons, each kind once for every path it holds.
    let mut diverted: Vec<(String, String, String)> = Vec::new();
    let mut excluded: Vec<(String, String, String)> = Vec::new();
    let mut overridden: Vec<(String, u32)> = Vec::new();
    let mut no_extract: Vec<(String, String, String)> = Vec::new();
    let mut no_upgrade: Vec<(String, String, String)> = Vec::new();
    for (name, missing) in wanted {
        let Some(file) = detect::program_file(&files, name) else {
            not_shipped.push(name);
            continue;
        };
        let path = file.path.as_str();
        match manager {
            PackageManager::Apt => {
                let diversion = host
                    .probe("dpkg-query", &["-S", path])
                    .await
                    .filter(|probe| probe.success)
                    .and_then(|probe| detect::dpkg_diversion(&probe.stdout, path));
                if let Some((to, by)) = diversion {
                    let whose = match by {
                        Some(by) => format!("a diversion by the package {by}"),
                        None => "a local diversion, added by hand".to_string(),
                    };
                    diverted.push((path.to_string(), to, whose));
                    continue;
                }
                if missing {
                    if let Some((line, config)) = detect::dpkg_excluded(root, path) {
                        excluded.push((path.to_string(), line, config));
                        continue;
                    }
                }
                let mode = host
                    .probe("dpkg-statoverride", &["--list", path])
                    .await
                    .filter(|probe| probe.success)
                    .and_then(|probe| detect::dpkg_override_mode(&probe.stdout, path));
                if let Some(mode) = mode.filter(|mode| mode & 0o4000 == 0) {
                    overridden.push((path.to_string(), mode));
                }
            }
            PackageManager::Dnf => {
                let setuid = file.mode.is_some_and(|mode| mode & 0o4000 != 0);
                let caps = file.caps.as_deref().is_some_and(|caps| !caps.is_empty());
                if !setuid && !caps {
                    unprivileged.push(path.to_string());
                } else if !setuid {
                    no_caps.push(path.to_string());
                }
            }
            PackageManager::Pacman => {
                if let Some((pattern, config)) = detect::pacman_kept(root, "NoExtract", path) {
                    no_extract.push((path.to_string(), pattern, config));
                } else if !missing {
                    if let Some((pattern, config)) = detect::pacman_kept(root, "NoUpgrade", path) {
                        no_upgrade.push((path.to_string(), pattern, config));
                    }
                }
            }
            PackageManager::Zypper => {}
        }
    }
    let both = |count: usize| match count {
        1 => ("it", "its", "that"),
        _ => ("them", "their", "those"),
    };
    let paths = |entries: &[(String, String, String)]| -> String {
        entries
            .iter()
            .map(|(path, _, _)| path.as_str())
            .collect::<Vec<_>>()
            .join(" and ")
    };
    if !diverted.is_empty() {
        let each = diverted
            .iter()
            .map(|(path, to, whose)| format!("{path} to {to} ({whose})"))
            .collect::<Vec<_>>()
            .join(" and ");
        let (them, _, those) = both(diverted.len());
        let list = paths(&diverted).replace(" and ", " ");
        let diversion = if diverted.len() == 1 {
            "diversion"
        } else {
            "diversions"
        };
        keeps.push(format!(
            "dpkg diverts {each}, so reinstalling {package} writes its {} there, not at {}: \
             remove {those} {diversion} if nothing needs {them} (`dpkg-divert --list {list}` \
             shows {them}), {then}",
            if diverted.len() == 1 { "file" } else { "files" },
            paths(&diverted)
        ));
    }
    if !excluded.is_empty() {
        let (them, _, those) = both(excluded.len());
        let filters = excluded
            .iter()
            .map(|(_, line, config)| format!("{line} in {config}"))
            .collect::<Vec<_>>();
        let mut filters_said = filters.clone();
        filters_said.dedup();
        keeps.push(format!(
            "{package}'s {} {} removed, and dpkg is set to leave {them} out ({}), so \
             reinstalling {package} does not write {them}: remove {those} {}, {then}",
            paths(&excluded),
            if excluded.len() == 1 { "was" } else { "were" },
            filters_said.join("; "),
            if filters_said.len() == 1 {
                "line"
            } else {
                "lines"
            }
        ));
    }
    if !overridden.is_empty() {
        let list = overridden
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>();
        let modes = match overridden.as_slice() {
            [(_, mode)] => format!("it mode {mode:o}"),
            _ => overridden
                .iter()
                .map(|(path, mode)| format!("{path} mode {mode:o}"))
                .collect::<Vec<_>>()
                .join(" and "),
        };
        let lists = list
            .iter()
            .map(|path| format!("`dpkg-statoverride --list {path}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let removes = list
            .iter()
            .map(|path| format!("sudo dpkg-statoverride --remove {path}"))
            .collect::<Vec<_>>()
            .join(" && ");
        let one = overridden.len() == 1;
        keeps.push(format!(
            "{package} installs {} setuid root, but {} {modes} ({lists}), and reinstalling \
             {package} applies {}: remove {} (`{removes}`), {then}",
            list.join(" and "),
            if one {
                "a dpkg stat override gives"
            } else {
                "dpkg stat overrides give"
            },
            if one { "that mode" } else { "those modes" },
            if one { "the override" } else { "the overrides" },
        ));
    }
    if !no_extract.is_empty() {
        let (them, _, _) = both(no_extract.len());
        let mut patterns: Vec<String> = no_extract
            .iter()
            .map(|(_, pattern, config)| format!("NoExtract = {pattern} in {config}"))
            .collect();
        patterns.dedup();
        keeps.push(format!(
            "pacman is set not to extract {} ({}), so reinstalling {package} does not write \
             {them} (it removes any copy there): take {} out of NoExtract, {then}",
            paths(&no_extract),
            patterns.join("; "),
            if patterns.len() == 1 {
                "that pattern"
            } else {
                "those patterns"
            }
        ));
    }
    if !no_upgrade.is_empty() {
        let (them, _, _) = both(no_upgrade.len());
        let mut patterns: Vec<String> = no_upgrade
            .iter()
            .map(|(_, pattern, config)| format!("NoUpgrade = {pattern} in {config}"))
            .collect();
        patterns.dedup();
        let pacnew = no_upgrade
            .iter()
            .map(|(path, _, _)| format!("{path}.pacnew"))
            .collect::<Vec<_>>()
            .join(" and ");
        keeps.push(format!(
            "pacman is set to keep {} as {} {} ({}), so reinstalling {package} writes its \
             files as {pacnew} and leaves {them} unable to map: take {} out of NoUpgrade, \
             {then}",
            paths(&no_upgrade),
            if no_upgrade.len() == 1 { "it" } else { "they" },
            if no_upgrade.len() == 1 { "is" } else { "are" },
            patterns.join("; "),
            if patterns.len() == 1 {
                "that pattern"
            } else {
                "those patterns"
            }
        ));
    }
    if !no_caps.is_empty() {
        if let Some(config) = detect::dnf_nocaps(root) {
            let (them, their) = match no_caps.len() {
                1 => ("it", "its"),
                _ => ("them", "their"),
            };
            keeps.push(format!(
                "{package} gives {} {their} file capabilities, and dnf is set to install files \
                 without them (nocaps in tsflags, in {config}), so reinstalling {package} \
                 leaves {them} unable to map: take nocaps out of tsflags, {then}",
                no_caps.join(" and ")
            ));
        }
    }
    if !unprivileged.is_empty() {
        let (them, their) = match unprivileged.len() {
            1 => ("it", "its"),
            _ => ("them", "their"),
        };
        keeps.push(format!(
            "{package} installs {} neither setuid nor with file capabilities, so reinstalling \
             it leaves {them} unable to map: make {them} setuid root, or give {them} {their} \
             file capability (cap_setuid for newuidmap, cap_setgid for newgidmap)",
            unprivileged.join(" and ")
        ));
    }
    if !not_shipped.is_empty() {
        let them = if not_shipped.len() == 1 { "it" } else { "them" };
        keeps.push(format!(
            "{package} is installed but does not ship {}, so reinstalling it does not bring \
             {them} back: install the package that ships {them} on this distribution",
            not_shipped.join(" or ")
        ));
    }
    keeps
}

/// `path` may map another user's ids: it is setuid, or (Fedora, RHEL) has
/// file capabilities (`security.capability`). The owner is not checked: a
/// setuid helper is root's.
fn may_map_ids(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o4000 != 0)
        {
            return true;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(name) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: both strings are valid and NUL-terminated; a null buffer
        // of size 0 asks only for the attribute's size.
        let size = unsafe {
            libc::getxattr(
                name.as_ptr(),
                c"security.capability".as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        size > 0
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// The Landlock line for a Linux kernel with `abi` (`None`: no Landlock).
pub(crate) fn landlock_item(
    abi: Option<i64>,
    release: Option<&detect::OsRelease>,
    wsl: bool,
) -> Item {
    let kernel_fix = if wsl {
        "Run `wsl --update` in PowerShell, then restart WSL".to_string()
    } else if release.is_some_and(|release| release.id == "debian") {
        "Install a newer kernel from bookworm-backports (`sudo apt-get install -t \
         bookworm-backports linux-image-$(dpkg --print-architecture)`), then reboot"
            .to_string()
    } else {
        "Upgrade to Linux 6.2 or later (6.7 or later for read-only helper shells)".to_string()
    };
    match abi {
        None | Some(0) => Item::fail(
            "Landlock",
            "this kernel has no Landlock; the Session supervisor's filter needs it",
            kernel_fix,
        ),
        Some(1) => Item::fail(
            "Landlock",
            "Landlock ABI 1: fix, fix-claude-code, fix-codex and audit need Linux 6.2 or later \
             (Landlock ABI 3) for their read-only reviewer, and on ABI 1 (Linux 5.13 to 5.18) the \
             hardened sandbox also refuses every rename into another directory, so Rust builds \
             (cargo, rustc) fail with \"Invalid cross-device link\"",
            kernel_fix,
        ),
        Some(2) => Item::fail(
            "Landlock",
            "Landlock ABI 2: fix, fix-claude-code, fix-codex and audit need Linux 6.2 or later \
             (Landlock ABI 3) for their read-only reviewer",
            kernel_fix,
        ),
        Some(3) => Item::warn(
            "Landlock",
            "Landlock ABI 3: read-only helper shells need Linux 6.7 or later (ABI 4) for their \
             network restriction, so they do not run here",
            Some(kernel_fix),
        ),
        Some(abi) => Item::ok("Landlock", format!("Landlock ABI {abi}")),
    }
}

/// What the release gate verifies: macOS on Apple silicon and Ubuntu
/// 24.04. Anything else is said at run time to be not yet verified (an
/// Intel Mac, another Linux release, WSL2, SELinux enforcing, local models
/// on less than 48 GB of memory), with what is known to matter there.
/// `local_models`: the configuration runs models on Ollama.
pub(crate) fn platform_items(
    os: &Os,
    selinux_enforcing: bool,
    memory_bytes: Option<u64>,
    local_models: bool,
) -> Vec<Item> {
    let mut items = Vec::new();
    match os {
        Os::MacOs {
            apple_silicon: true,
            ..
        } => items.push(Item::ok(
            "Platform",
            "macOS on Apple silicon, as the release gate verifies",
        )),
        Os::MacOs { .. } => items.push(Item::warn(
            "Platform",
            "an Intel Mac: Axocoatl has not been verified on Intel Macs yet (the release gate \
             runs on Apple silicon)",
            None,
        )),
        Os::Linux { release, wsl, .. } => {
            if wsl.is_some() {
                items.push(Item::warn(
                    "Platform",
                    format!(
                        "{} in WSL2 on Windows: Axocoatl has not been verified in WSL2 yet",
                        release.name()
                    ),
                    Some(
                        "Keep your repositories in the Linux file system (such as ~/src), not \
                         under /mnt/c: Windows' drives are slow there and do not keep Linux file \
                         permissions"
                            .to_string(),
                    ),
                ));
            } else if release.id == "ubuntu" && release.version_id == "24.04" {
                items.push(Item::ok(
                    "Platform",
                    format!("{}, as the release gate verifies", release.name()),
                ));
            } else {
                items.push(Item::warn(
                    "Platform",
                    format!(
                        "{}: Axocoatl has not been verified on it yet (Ubuntu 24.04 is)",
                        release.name()
                    ),
                    None,
                ));
            }
            if selinux_enforcing {
                items.push(Item::warn(
                    "Platform",
                    "SELinux is enforcing: Sessions under SELinux enforcing have not been \
                     verified yet",
                    None,
                ));
            }
        }
    }
    if let Some(memory) = memory_bytes
        .filter(|memory| local_models && !axocoatl_isolation::host::at_least_gb(*memory, 48))
    {
        items.push(Item::warn(
            "Platform",
            format!(
                "{} GB of memory: local models have not been verified below 48 GB",
                axocoatl_isolation::host::memory_gb(memory)
            ),
            None,
        ));
    }
    items
}

/// The line for this user's systemd manager (Linux with systemd): the
/// Always-On Service is one of its units, and rootless Podman keeps its
/// runtime directory in the user's session. `service_installed`: linger
/// matters only for an installed service.
pub(crate) fn user_systemd_item(
    systemd: &detect::UserSystemd,
    user: &str,
    service_installed: bool,
) -> Item {
    let user_arg = crate::onboard::host::shell_quote(user);
    if !systemd.reachable {
        return Item::warn(
            "systemd user session",
            format!(
                "no systemd user session for {user} (`systemctl --user` says: {}): after `su -`, \
                 `sudo -iu` or a login that started none, the Always-On Service cannot run, and \
                 rootless Podman cannot keep its runtime directory",
                systemd.state
            ),
            Some(format!(
                "Log in as {user} directly (a desktop session, or `ssh {user_arg}@localhost`) \
                 instead of switching users with su or sudo; to keep the service running after you \
                 log out, also run `sudo loginctl enable-linger {user_arg}`"
            )),
        );
    }
    match (systemd.linger, service_installed) {
        (Some(false), true) => Item::warn(
            "systemd user session",
            format!(
                "{} (systemd --user); linger is off, so the Always-On Service stops when {user} \
                 logs out",
                systemd.state
            ),
            Some(format!(
                "Keep it running: `loginctl enable-linger {user_arg}`"
            )),
        ),
        (Some(true), _) => Item::ok(
            "systemd user session",
            format!(
                "{} (systemd --user); linger is on, so the Always-On Service keeps running after \
                 {user} logs out",
                systemd.state
            ),
        ),
        _ => Item::ok(
            "systemd user session",
            format!("{} (systemd --user)", systemd.state),
        ),
    }
}

/// The line for git, which the daemon runs on every repository (`None`:
/// missing).
pub(crate) fn git_item(git: Option<&str>, os: &Os) -> Item {
    match (git, os) {
        (Some(version), _) => Item::ok(
            "git",
            format!("git {version} (the daemon runs git on your repositories)"),
        ),
        (None, Os::MacOs { .. }) => Item::fail(
            "git",
            "git is missing: macOS's git needs the Command Line Tools, which are not installed \
             (`xcode-select -p` finds none); `axocoatl run` and Sessions run git on your \
             repositories",
            "Install the Command Line Tools: `xcode-select --install` (it opens Apple's \
             installer), then run `axocoatl doctor` again",
        ),
        (None, Os::Linux { release, .. }) => Item::fail(
            "git",
            "git is missing; `axocoatl run` and Sessions run git on your repositories",
            match release.package_manager() {
                Some(manager) => format!("Install it: `{}`", manager.install(&["git"]).display()),
                None => "Install git with your distribution's package manager".to_string(),
            },
        ),
    }
}

/// The line for a configuration's `hooks:`, which this version parses and
/// checks but never runs (`None` without any).
fn hooks_item(config: &axocoatl_config::AxocoatlConfig, config_path: &Path) -> Option<Item> {
    let count = config.hooks.len();
    (count > 0).then(|| {
        Item::warn(
            "Hooks",
            format!(
                "hooks: {count} entr{} configured, which Axocoatl {} checks but does not run",
                if count == 1 { "y" } else { "ies" },
                env!("CARGO_PKG_VERSION")
            ),
            Some(format!(
                "Remove the hooks: section from {}, so nothing relies on it: Automations and \
                 webhooks react to events, and the MCP approval gate is what stops a tool call",
                config_path.display()
            )),
        )
    })
}

fn level(level: Level) -> Status {
    match level {
        Level::Ok => Status::Ok,
        Level::Warn => Status::Warn,
        Level::Fail => Status::Fail,
    }
}

/// The Ollama models the Agents run, each with whether that Agent lists
/// tools.
fn agent_ollama_models(
    config: &axocoatl_config::AxocoatlConfig,
) -> impl Iterator<Item = (String, bool)> + '_ {
    let default_model = config
        .providers
        .ollama
        .as_ref()
        .and_then(|ollama| ollama.model.clone())
        .unwrap_or_else(|| "llama3.2".to_string());
    config
        .agents
        .iter()
        .filter(|agent| agent.provider == "ollama")
        .map(move |agent| {
            let model = if agent.model.is_empty() {
                default_model.clone()
            } else {
                agent.model.clone()
            };
            (model, !agent.tools.is_empty())
        })
}

/// The models configured for native Agents on Ollama, from the Agents and
/// `run.models`.
fn ollama_models(
    config: &axocoatl_config::AxocoatlConfig,
    run: &onboard_config::RunModels,
) -> BTreeMap<String, bool> {
    let mut models = BTreeMap::new();
    for (model, tools) in agent_ollama_models(config) {
        *models.entry(model).or_default() |= tools;
    }
    for spec in [&run.writer, &run.reviewer].into_iter().flatten() {
        if let Ok(model) = ModelRef::parse(spec) {
            if model.provider == Provider::Ollama {
                // A run's writer calls tools; its reviewer reads files.
                *models.entry(model.model).or_default() |= true;
            }
        }
    }
    models
}

/// How `axocoatl onboard` changes the Ollama model `model` of a
/// configuration without asking again who writes, or switching the writer.
/// `axocoatl onboard --update` moves the Agents onboarding wrote and
/// `run.models` to the new models and keeps every other Agent as it is (one
/// edited by hand, or the person's own), so while such an Agent runs
/// `model` the command is `--replace`, which backs the file up and writes
/// the team for the new models. The writer is always named
/// (`--writer`), so Step 1 is not asked again and `--yes` does not choose
/// another; with `--replace`, which keeps nothing, a writer model that
/// stays is named too.
struct ModelChange {
    /// `--update` or `--replace`.
    mode: &'static str,
    /// Why `--replace`, for the end of the fix.
    why: String,
    /// `model` is `run.models.reviewer` and not the writer's.
    reviewer_only: bool,
    /// Who writes, from the `# Writer:` comment or `run.models.writer`;
    /// `None` when the configuration does not say (one not written by
    /// onboarding, without `run.models.writer`).
    writer: Option<String>,
    /// ` --writer W`, with ` --writer-model SPEC` when `--replace` would
    /// otherwise forget a writer model that stays.
    flags: String,
}

impl ModelChange {
    /// `kept`: the Ollama models of the Agents `--update` keeps as they are
    /// ([`onboard_config::kept_agent_models`]).
    fn new(
        model: &str,
        kept: &[String],
        run: &onboard_config::RunModels,
        writer: Option<&str>,
    ) -> Self {
        let runs = |spec: &Option<String>| {
            spec.as_deref()
                .and_then(|spec| ModelRef::parse(spec).ok())
                .is_some_and(|spec| spec.provider == Provider::Ollama && spec.model == model)
        };
        let agents = kept.iter().any(|kept| kept == model);
        let reviewer_only = runs(&run.reviewer) && !runs(&run.writer);
        let (mode, why) = if agents {
            (
                "--replace",
                format!(
                    " (`--update` would keep the Agents you edited, which run {model}; \
                     `--replace` backs this configuration up first)"
                ),
            )
        } else {
            ("--update", String::new())
        };
        let writer = writer
            .filter(|writer| matches!(*writer, "claude-code" | "codex" | "ollama" | "openrouter"))
            .map(str::to_string);
        let mut flags = writer
            .as_deref()
            .map(|writer| format!(" --writer {writer}"))
            .unwrap_or_default();
        // The writer's own model stays when only the reviewer (or, for an
        // OpenRouter writer, an Agent's local model) moves.
        let keeps_writer_model = agents
            && !runs(&run.writer)
            && (reviewer_only || writer.as_deref() == Some("openrouter"));
        if let (true, Some(writer), Some(spec)) =
            (keeps_writer_model, writer.as_deref(), run.writer.as_deref())
        {
            if spec.starts_with(&format!("{writer}:")) {
                flags.push_str(&format!(
                    " --writer-model {}",
                    crate::onboard::host::shell_quote(spec)
                ));
            }
        }
        Self {
            mode,
            why,
            reviewer_only,
            writer,
            flags,
        }
    }

    /// `axocoatl onboard MODE FLAGS` and `extra`, in backticks.
    fn command(&self, extra: &str) -> String {
        format!("`axocoatl onboard {}{}{extra}`", self.mode, self.flags)
    }
}

/// What doctor says to do about a configured Ollama model, for its role.
struct ModelFixes {
    /// It is too large for this computer ([`too_large_fix`]).
    too_large: String,
    /// Its context could not be read ([`unreadable_model_fix`]).
    unreadable: String,
}

/// When the default configuration does not say who writes: name the
/// model to change, or the writer to write a new configuration for. The
/// writer is a placeholder with the choices in words: `--writer
/// claude-code|codex|…` pasted into a shell is a pipeline that replaces
/// the configuration with a Claude Code one.
fn unknown_writer_fix(model: &str, what: &str) -> String {
    format!(
        "{what} in place of {model} in this configuration, or write a new one (this one is \
         backed up first) with `axocoatl onboard --replace --writer WRITER`, where WRITER is \
         claude-code, codex, ollama or openrouter"
    )
}

/// The configuration doctor checks, when it is not the default one
/// (`doctor --config PATH`): `axocoatl onboard` has no `--config` and
/// writes only the default configuration, so no `axocoatl onboard` command
/// changes this one, and a fix says what to change in the file instead.
#[derive(Debug, Clone, Copy)]
struct OtherConfig<'a> {
    path: &'a Path,
    default: &'a Path,
}

impl<'a> OtherConfig<'a> {
    /// `Some` when `config_path` is not `default`, the default
    /// configuration ([`same_file`]).
    fn of(config_path: &'a Path, default: &'a Path) -> Option<Self> {
        (!same_file(config_path, default)).then_some(Self {
            path: config_path,
            default,
        })
    }

    /// `in PATH (`axocoatl onboard` writes only the default configuration,
    /// DEFAULT)`, to end a fix that edits the file.
    fn in_file(&self) -> String {
        format!(
            "in {} (`axocoatl onboard` writes only the default configuration, {})",
            self.path.display(),
            self.default.display()
        )
    }
}

/// What to do about a configuration doctor was given with `--config` that
/// does not exist: copy the default one there (written first by `axocoatl
/// onboard` when there is none), or check the default one.
fn missing_other_config_fix(other: &OtherConfig<'_>) -> String {
    let quote = |path: &Path| crate::onboard::host::shell_quote(&path.display().to_string());
    let copy = format!("cp {} {}", quote(other.default), quote(other.path));
    if other.default.is_file() {
        format!(
            "Copy the default configuration there and change what this one needs (`{copy}`), \
             or check the default one with `axocoatl doctor`"
        )
    } else {
        format!(
            "Write the default configuration with `axocoatl onboard` (it writes only {}), then \
             copy it there and change what this one needs (`{copy}`)",
            other.default.display()
        )
    }
}

/// Whether `a` and `b` name one file: as written, or once resolved (a
/// relative path, `..`, or a symbolic link to the same file), or, for a file
/// that does not exist yet, in the same resolved directory.
pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    fn resolved(path: &Path) -> Option<PathBuf> {
        std::fs::canonicalize(path).ok().or_else(|| {
            let parent = match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                _ => Path::new("."),
            };
            Some(std::fs::canonicalize(parent).ok()?.join(path.file_name()?))
        })
    }
    a == b || matches!((resolved(a), resolved(b)), (Some(a), Some(b)) if a == b)
}

/// The fix for `model`, configured on Ollama and too large for this
/// computer: a smaller model, or OpenRouter, for the role it has. A model
/// that is only the reviewer (`run.models.reviewer` and not the writer, as
/// onboarding sets up Claude Code, Codex, or a local writer with a hosted
/// reviewer) moves the reviewer, never the writer ([`ModelChange`]). For a
/// configuration that is not the default one, in words: onboarding would
/// change the default one ([`OtherConfig`]).
fn too_large_fix(
    model: &str,
    kept: &[String],
    run: &onboard_config::RunModels,
    writer: Option<&str>,
    other: Option<OtherConfig<'_>>,
) -> String {
    if let Some(other) = other {
        return format!(
            "Name a smaller model, or an OpenRouter one, in place of {model} {}",
            other.in_file()
        );
    }
    let change = ModelChange::new(model, kept, run, writer);
    let hosted = format!(
        " --reviewer openrouter:{}",
        crate::onboard::openrouter::DEFAULT_REVIEWER
    );
    let why = &change.why;
    match change.writer.as_deref() {
        None => unknown_writer_fix(model, "Name a smaller model (or an OpenRouter one)"),
        // An OpenRouter writer's reviewer runs on OpenRouter too
        // (onboarding offers no local one with it).
        Some("openrouter") if change.reviewer_only => format!(
            "Use OpenRouter for the reviewer ({}){why}",
            change.command(&hosted)
        ),
        Some("openrouter") => format!("Use OpenRouter for it ({}){why}", change.command("")),
        Some(_) if change.reviewer_only => format!(
            "Use OpenRouter for the reviewer ({}), or choose a smaller reviewer ({}){why}",
            change.command(&hosted),
            change.command("")
        ),
        Some("ollama") => format!(
            "Choose a smaller model ({}), or use OpenRouter for it (`axocoatl onboard {} --writer \
             openrouter`){why}",
            change.command(""),
            change.mode
        ),
        // Claude Code or Codex: the Agents run on the reviewer's model.
        Some(_) => format!(
            "Choose a smaller model ({}), or use OpenRouter for it ({}){why}",
            change.command(""),
            change.command(&hosted)
        ),
    }
}

/// The fix for `model` when its context could not be read: check it, or
/// choose another for its role ([`ModelChange`]; in words for a
/// configuration that is not the default one, [`OtherConfig`]).
fn unreadable_model_fix(
    model: &str,
    kept: &[String],
    run: &onboard_config::RunModels,
    writer: Option<&str>,
    other: Option<OtherConfig<'_>>,
) -> String {
    if let Some(other) = other {
        return format!(
            "Check the model with `ollama run {model}`, or name another model in place of {model} \
             {}",
            other.in_file()
        );
    }
    let change = ModelChange::new(model, kept, run, writer);
    let another = match change.writer.as_deref() {
        None => {
            return format!(
                "Check the model with `ollama run {model}`, or {}",
                unknown_writer_fix(model, "name another model")
            )
        }
        Some(_) if change.reviewer_only => "choose another reviewer",
        // An Agent's local model under an OpenRouter writer: `--replace`
        // puts the team on OpenRouter.
        Some("openrouter") => "use OpenRouter for it",
        Some(_) => "choose another",
    };
    format!(
        "Check the model with `ollama run {model}`, or {another} ({}){}",
        change.command(""),
        change.why
    )
}

/// Collect the report for `config_path`.
pub(crate) async fn collect(config_path: &Path, env: &Environment) -> Report {
    // `doctor --config PATH` for another configuration than the default
    // one, which no `axocoatl onboard` command changes.
    let other = OtherConfig::of(config_path, &env.default_config);
    let mut sections = Sections::default();
    let mut model_states: BTreeMap<String, ModelState> = BTreeMap::new();
    let mut openrouter_ready = false;

    // ── Daemon: the configuration first; the rest needs it ──
    let text = std::fs::read_to_string(config_path);
    let config = match &text {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            sections.daemon.push(Item::fail(
                "Config",
                format!(
                    "Config missing: no configuration at {}",
                    config_path.display()
                ),
                match other {
                    None => "Run `axocoatl onboard`".to_string(),
                    Some(other) => missing_other_config_fix(&other),
                },
            ));
            None
        }
        _ => match load(config_path, env).await {
            Ok(config) => {
                sections.daemon.push(Item::ok(
                    "Config",
                    format!("Config valid: {}", config_path.display()),
                ));
                for warning in axocoatl_config::no_tools_warnings(&config) {
                    sections.daemon.push(Item::warn(
                        "Config",
                        warning.problem(),
                        Some(axocoatl_config::NoToolsWarning::HINT.to_string()),
                    ));
                }
                for warning in axocoatl_config::network_warnings(&config) {
                    sections.daemon.push(Item::warn(
                        "Config",
                        warning.field,
                        Some(warning.message),
                    ));
                }
                Some(config)
            }
            Err(error) => {
                sections.daemon.push(Item::fail(
                    "Config",
                    format!("Config invalid: {}", config_path.display()),
                    error,
                ));
                None
            }
        },
    };
    let run = text
        .as_deref()
        .map(onboard_config::run_models)
        .unwrap_or_default();
    // Who writes, as onboarding recorded it; else what `run.models.writer`
    // implies.
    let marker = text.as_deref().ok().and_then(onboard_config::writer_marker);
    let chosen_writer = marker
        .as_ref()
        .map(|marker| marker.writer.clone())
        .or_else(|| {
            run.writer
                .as_deref()
                .and_then(|spec| ModelRef::parse(spec).ok())
                .map(|model| model.provider.id().to_string())
        });

    // ── Sandbox ──
    let mut report =
        podman_report::inspect_through(&env.podman, env.podman_connection.clone()).await;
    read_id_ranges(&mut report, &env.host, &env.id_root, &env.os, &env.user).await;
    let e2b = config
        .as_ref()
        .is_some_and(|config| config.sandbox.backend == "e2b");
    // `axocoatl run` always needs Podman, whatever runs workbench Sessions:
    // whether it is ready is decided before an E2B setup softens the lines.
    let mut podman_ok = true;
    for finding in podman_report::findings(&report, &env.platform()) {
        let mut status = level(finding.level);
        let mut detail = finding.detail;
        if status == Status::Fail {
            podman_ok = false;
            if e2b {
                status = Status::Warn;
                detail.push_str(" (workbench Sessions use E2B; axocoatl run still needs Podman)");
            }
        }
        sections.sandbox.push(Item {
            name: finding.name.to_string(),
            status,
            detail,
            fix: finding.fix,
        });
    }
    if let Os::Linux { release, wsl, .. } = &env.os {
        sections.sandbox.push(landlock_item(
            env.landlock_abi,
            Some(release),
            wsl.is_some(),
        ));
    }
    let local_models = config
        .as_ref()
        .is_some_and(|config| !ollama_models(config, &run).is_empty());
    sections.sandbox.extend(platform_items(
        &env.os,
        env.selinux_enforcing,
        env.memory_bytes,
        local_models,
    ));
    let sandbox_ok = podman_ok
        && !sections
            .sandbox
            .iter()
            .any(|item| item.status == Status::Fail);
    if let Some(config) = &config {
        if e2b {
            sections.sandbox.push(Item::ok(
                "Sandbox backend",
                "e2b: workbench Sessions run in E2B, and `axocoatl run` still runs in rootless \
                 Podman on this computer",
            ));
        }
        sections.sandbox.push(Item::ok(
            "Sandbox network",
            crate::sandbox_network_doctor_line(&config.sandbox),
        ));
        match crate::sandbox_workload_doctor_line(&config.sandbox, report.rootless()) {
            Ok(line) => sections.sandbox.push(Item::ok("Sandbox workload", line)),
            Err((line, hint)) => {
                sections
                    .sandbox
                    .push(Item::warn("Sandbox workload", line, Some(hint)))
            }
        }
        for line in crate::credentials_doctor_lines(config) {
            sections.sandbox.push(match line.hint {
                None => Item::ok("Credentials", line.text),
                Some(hint) => Item::warn("Credentials", line.text, Some(hint.to_string())),
            });
        }
        if let Some(browser) = &config.browser {
            let image = browser
                .image
                .clone()
                .unwrap_or_else(|| axocoatl_config::DEFAULT_BROWSER_IMAGE.to_string());
            sections.sandbox.push(
                match axocoatl_daemon::browser_install::image_present(&image).await {
                    Ok(true) => Item::ok("Browser", format!("image {image} present")),
                    Ok(false) => Item::warn(
                        "Browser",
                        format!("image {image} missing"),
                        Some(
                            "Run `axocoatl browser install` before Agents use the browser tools."
                                .into(),
                        ),
                    ),
                    Err(error) => {
                        Item::warn("Browser", format!("image {image} not checked"), Some(error))
                    }
                },
            );
        }
    }

    // The data directory: its secret store holds keys `api_key_secret`
    // names, and the writers' sign-ins and images.
    let data_dir = match &env.data_dir {
        Some(data_dir) => Some(data_dir.clone()),
        None => crate::configure_data_dir(config_path).ok(),
    };

    // A running daemon may be running a model doctor would load: doctor
    // then reads a loaded model's context without loading it again.
    let daemon_client = axocoatl_daemon::ipc::IpcClient::connect(&env.socket_path)
        .await
        .ok();

    // ── Models ──
    if let Some(config) = &config {
        if let Some(ollama) = &config.providers.ollama {
            let wanted = ollama_models(config, &run);
            let cli = env.host.find("ollama");
            let cli_version = match &cli {
                Some(path) => env
                    .host
                    .probe_env(
                        &path.to_string_lossy(),
                        &["--version"],
                        &[("OLLAMA_HOST", ollama.base_url.as_str())],
                    )
                    .await
                    .and_then(|probe| {
                        detect::parse_ollama_version(&format!("{}\n{}", probe.stdout, probe.stderr))
                            .0
                    }),
                None => None,
            };
            let service = detect::ollama_service(
                &env.host,
                env.xdg_config_home.as_deref(),
                detect::url_port(&ollama.base_url),
            );
            let host = OllamaHost {
                service,
                macos: env.os.is_macos(),
                brew_env_file: Some(
                    detect::homebrew_services_dir(&env.home, env.xdg_config_home.as_deref())
                        .join("ollama.env"),
                ),
                cli_version,
            };
            // The models of the Agents `onboard --update` keeps as they are.
            let kept = text
                .as_deref()
                .ok()
                .and_then(|text| {
                    onboard_config::kept_agent_models(
                        &onboard_config::read_existing(text).ok()?,
                        text,
                    )
                })
                .unwrap_or_default();
            let fixes: BTreeMap<String, ModelFixes> = wanted
                .keys()
                .map(|model| {
                    let writer = chosen_writer.as_deref();
                    let fixes = ModelFixes {
                        too_large: too_large_fix(model, &kept, &run, writer, other),
                        unreadable: unreadable_model_fix(model, &kept, &run, writer, other),
                    };
                    (model.clone(), fixes)
                })
                .collect();
            let server_ready = check_ollama(
                &ollama.base_url,
                &host,
                &wanted,
                &fixes,
                env,
                // Loading a model may take as long as a first answer may.
                ollama
                    .first_token_timeout_secs
                    .map_or(axocoatl_llm_ollama::LOCAL_FIRST_TOKEN_TIMEOUT, |secs| {
                        std::time::Duration::from_secs(secs)
                    }),
                daemon_client.is_some(),
                &mut sections.models,
                &mut model_states,
            )
            .await;
            if !server_ready {
                for model in wanted.keys() {
                    model_states.entry(model.clone()).or_insert(ModelState {
                        ok: false,
                        why: format!("the Ollama server at {} is not ready", ollama.base_url),
                    });
                }
            }
        }
        let uses_openrouter = config
            .agents
            .iter()
            .any(|agent| agent.provider == "openrouter")
            || [&run.writer, &run.reviewer]
                .into_iter()
                .flatten()
                .any(|spec| spec.starts_with("openrouter:"));
        if let Some(openrouter) = &config.providers.openrouter {
            match provider_key(
                "OpenRouter",
                openrouter,
                data_dir.as_deref(),
                config_path,
                &env.default_config,
            ) {
                Ok(line) => {
                    openrouter_ready = true;
                    sections.models.push(Item::ok("OpenRouter", line));
                }
                Err((line, hint)) if uses_openrouter => sections.models.push(Item::fail(
                    "OpenRouter",
                    format!("{line}, and an Agent or run default uses OpenRouter"),
                    hint,
                )),
                Err((line, hint)) => {
                    sections
                        .models
                        .push(Item::warn("OpenRouter", line, Some(hint)))
                }
            }
        } else if uses_openrouter {
            // The browser sign-in adds `providers.openrouter` and changes
            // nothing else in the file: not the writer, not the models.
            sections.models.push(Item::fail(
                "OpenRouter",
                "an Agent or run default uses OpenRouter, which is not configured",
                format!(
                    "Run `{}`: it signs you in to OpenRouter in the browser, stores the key it \
                     issues and adds providers.openrouter to this configuration, changing nothing \
                     else in it",
                    connect_openrouter_command(config_path, &env.default_config)
                ),
            ));
        }
        for (name, credentials) in [
            ("OpenAI", &config.providers.openai),
            ("Anthropic", &config.providers.anthropic),
            ("Gemini", &config.providers.gemini),
            ("Mistral", &config.providers.mistral),
        ] {
            if let Some(credentials) = credentials {
                sections.models.push(
                    match provider_key(
                        name,
                        credentials,
                        data_dir.as_deref(),
                        config_path,
                        &env.default_config,
                    ) {
                        Ok(line) => Item::ok(name, line),
                        Err((line, hint)) => Item::warn(name, line, Some(hint)),
                    },
                );
            }
        }
    }

    // ── Writers ──
    let mut secrets = BTreeSet::new();
    let mut images = BTreeSet::new();
    if let Some(data_dir) = data_dir.as_ref().filter(|dir| dir.is_dir()) {
        if let Ok(names) = axocoatl_daemon::secret_store::list_secrets(data_dir) {
            secrets.extend(names);
        }
        if let Ok(records) =
            axocoatl_daemon::external_agent::recipe_images::recorded_images_at(data_dir)
        {
            images.extend(records.into_iter().map(|record| record.recipes.join(" ")));
        }
    }
    let claude_code = writer_items(
        &WriterPath {
            name: "Claude Code",
            recipe: "claude-code",
            credentials: &[axocoatl_daemon::external_agent::claude_code::CLAUDE_CODE_SECRET],
            connect: "axocoatl connect claude-code",
            set_up: "axocoatl recipe build claude-code && axocoatl connect claude-code",
            chosen: chosen_writer.as_deref() == Some("claude-code"),
        },
        images.contains("claude-code"),
        &secrets,
        &mut sections.writers,
    );
    // A Codex setup names its credential: that one must be stored.
    let codex_credential = marker
        .as_ref()
        .filter(|marker| marker.writer == "codex")
        .and_then(|marker| marker.credential.clone());
    let codex_credentials: Vec<&str> = match codex_credential.as_deref() {
        Some(credential) => vec![credential],
        None => vec![
            axocoatl_daemon::external_agent::codex::CODEX_CHATGPT_SECRET,
            axocoatl_daemon::external_agent::codex::CODEX_SECRET,
        ],
    };
    let codex_connect = if codex_credential.as_deref()
        == Some(axocoatl_daemon::external_agent::codex::CODEX_SECRET)
    {
        "axocoatl connect codex --api-key-only"
    } else {
        "axocoatl connect codex"
    };
    let codex = writer_items(
        &WriterPath {
            name: "Codex",
            recipe: "codex",
            credentials: &codex_credentials,
            connect: codex_connect,
            // Codex's sign-in builds its image the first time.
            set_up: codex_connect,
            chosen: chosen_writer.as_deref() == Some("codex"),
        },
        images.contains("codex"),
        &secrets,
        &mut sections.writers,
    );
    match crate::connect_cmd::find_claude(
        None,
        Some(env.host.run_path().as_os_str()),
        Some(&env.home),
    ) {
        Ok(path) => {
            let version = env
                .host
                .probe(&path.to_string_lossy(), &["--version"])
                .await
                .filter(|probe| probe.success)
                .map(|probe| probe.stdout.trim().to_string())
                .unwrap_or_default();
            sections.writers.push(Item::ok(
                "claude CLI",
                format!("{} {}", path.display(), version).trim().to_string(),
            ));
        }
        Err(_) => sections.writers.push(Item::ok(
            "claude CLI",
            "not found; needed only to sign in with `axocoatl connect claude-code`",
        )),
    }
    sections.writers.push(match env.host.find("codex") {
        Some(path) => Item::ok("codex CLI", path.display().to_string()),
        None => Item::ok(
            "codex CLI",
            "not found; not needed: `axocoatl connect codex` runs the pinned Codex in a container",
        ),
    });
    sections.writers.push(match env.host.find("gh") {
        None => Item::ok("gh", "not found; needed only for `axocoatl run --keep pr`"),
        Some(path) => match env
            .host
            .probe(&path.to_string_lossy(), &["auth", "status"])
            .await
        {
            Some(probe) if probe.success => Item::ok("gh", "signed in (for `--keep pr`)"),
            _ => Item::warn(
                "gh",
                "not signed in; `axocoatl run --keep pr` needs it",
                Some("gh auth login".to_string()),
            ),
        },
    });
    if !secrets.is_empty() {
        sections.writers.push(Item::ok(
            "Stored credentials",
            secrets.iter().cloned().collect::<Vec<_>>().join(", "),
        ));
    }

    // ── Daemon ──
    if let Some(data_dir) = data_dir.as_ref().filter(|_| config.is_some()) {
        match crate::probe_data_dir(data_dir) {
            Ok(()) => {
                sections.daemon.push(Item::ok(
                    "Data dir",
                    format!("Data dir writable: {}", data_dir.display()),
                ));
                let ownership =
                    axocoatl_session::execution_ownership::inspect_format_ownership(data_dir);
                sections
                    .daemon
                    .push(match crate::format_ownership_doctor_line(&ownership) {
                        Ok(line) => Item::ok("Data dir ownership", line),
                        Err((line, hint)) => Item::fail("Data dir ownership", line, hint),
                    });
            }
            Err(error) => sections.daemon.push(Item::fail(
                "Data dir",
                format!("Data dir not writable: {}", data_dir.display()),
                error.to_string(),
            )),
        }
    }
    sections.daemon.push(git_item(env.git.as_deref(), &env.os));
    if let Some(config) = &config {
        sections.daemon.extend(hooks_item(config, config_path));
    }
    sections.daemon.extend(proxy_item(&env.proxy_variables));
    let service_program = installed_service_program(&env.home, env.os.is_macos());
    if let Some(systemd) = &env.user_systemd {
        sections.daemon.push(user_systemd_item(
            systemd,
            &env.user,
            service_program.is_some(),
        ));
    }
    let (server_host, port) = config
        .as_ref()
        .map(|config| (config.server.host.clone(), config.server.port))
        .unwrap_or_else(|| ("127.0.0.1".to_string(), 8080));
    match daemon_client {
        Some(mut client) => {
            let version = daemon_version(&server_host, port).await;
            let cli = env!("CARGO_PKG_VERSION");
            sections.daemon.push(match &version {
                Some(version) if version != cli => Item::warn(
                    "Daemon",
                    format!("the running daemon is Axocoatl {version}; this CLI is {cli}"),
                    Some(
                        "Restart it so it runs this version: `axocoatl service stop && axocoatl \
                         service start`, or stop `axocoatl dev` and start it again"
                            .to_string(),
                    ),
                ),
                Some(version) => Item::ok("Daemon", format!("running (Axocoatl {version})")),
                None => Item::ok("Daemon", "running (IPC reachable)"),
            });
            let check = match client
                .request(&axocoatl_daemon::ipc::IpcRequest::RuntimeVolumes)
                .await
            {
                Ok(axocoatl_daemon::ipc::IpcResponse::RuntimeVolumes { check }) => Some(check),
                _ => None,
            };
            sections
                .daemon
                .push(match crate::runtime_volume_doctor_line(check.as_ref()) {
                    Ok(line) => Item::ok("Session volumes", line),
                    Err((line, hint)) => Item::warn("Session volumes", line, Some(hint)),
                });
        }
        None => {
            sections.daemon.push(Item::warn(
                "Daemon",
                "no daemon running; `axocoatl run` and the workbench need one",
                Some("Start it with `axocoatl dev` (or `axocoatl service start`)".to_string()),
            ));
            // Without a configuration there is no port to ask about.
            if config.is_some()
                && is_loopback(&server_host)
                && !crate::onboard::host::port_free(port)
            {
                sections.daemon.push(Item::warn(
                    "Port",
                    format!(
                        "port {port} is in use by another program, so `axocoatl dev` would not \
                         start on it"
                    ),
                    Some({
                        let free = crate::onboard::host::first_free_port(
                            port.saturating_add(1),
                            crate::onboard::host::port_free,
                        );
                        match other {
                            None => format!(
                                "Choose another port: `axocoatl onboard --update --port {free}`, \
                                 or set server.port in {}",
                                config_path.display()
                            ),
                            Some(other) => format!(
                                "Choose another port, such as {free}: set server.port {}",
                                other.in_file()
                            ),
                        }
                    }),
                ));
            }
        }
    }
    if let Some(program) = service_program {
        let current = std::env::current_exe()
            .and_then(|path| path.canonicalize())
            .ok();
        sections
            .daemon
            .push(if current.as_deref() == Some(program.as_path()) {
                Item::ok(
                    "Always-On Service",
                    format!("installed; runs {}", program.display()),
                )
            } else {
                Item::warn(
                    "Always-On Service",
                    format!(
                        "installed; runs {}, not this axocoatl ({})",
                        program.display(),
                        current
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "unknown".into())
                    ),
                    Some(
                        "Install it again from this axocoatl: `axocoatl service install`, then \
                     `axocoatl service stop && axocoatl service start`"
                            .to_string(),
                    ),
                )
            });
    }
    if let Some(config) = &config {
        if config.webhooks.is_empty() {
            sections
                .daemon
                .push(Item::ok("Webhooks", "none (no outbound event egress)"));
        } else {
            let names: Vec<&str> = config
                .webhooks
                .iter()
                .filter(|webhook| webhook.enabled)
                .map(|webhook| webhook.name.as_str())
                .collect();
            sections.daemon.push(Item::ok(
                "Webhooks",
                format!(
                    "{} active; Skill events leave the box to: {}",
                    names.len(),
                    names.join(", ")
                ),
            ));
        }
    }

    // ── What is ready ──
    let model_ok = |spec: &str| -> Result<(), String> {
        let model = ModelRef::parse(spec)?;
        match model.provider {
            Provider::Ollama => match model_states.get(&model.model) {
                Some(state) if state.ok => Ok(()),
                Some(state) => Err(format!("{spec}: {}", state.why)),
                None => Err(format!("{spec}: Ollama is not configured")),
            },
            Provider::OpenRouter if openrouter_ready => Ok(()),
            Provider::OpenRouter => Err(format!("{spec}: OpenRouter is not configured")),
        }
    };
    let sandbox_blocker = (!sandbox_ok).then(|| "the sandbox is not ready".to_string());
    let mut ready_for = Vec::new();
    let reviewer = run.reviewer.as_deref();
    let mut path =
        |command: &str, problems: Vec<String>, detail: String, set_up: bool, required: bool| {
            ready_for.push(RunPath {
                command: command.to_string(),
                ok: problems.is_empty(),
                detail: if problems.is_empty() {
                    detail
                } else {
                    problems.join("; ")
                },
                set_up,
                required,
            });
        };
    let reviewer_problem = || -> Option<String> {
        match reviewer {
            None => Some(match other {
                None => "no reviewer model: run `axocoatl onboard`, or pass --model \
                         reviewer=PROVIDER:MODEL"
                    .to_string(),
                Some(other) => format!(
                    "no reviewer model: set run.models.reviewer {}, or pass --model \
                     reviewer=PROVIDER:MODEL",
                    other.in_file()
                ),
            }),
            Some(spec) => model_ok(spec).err(),
        }
    };
    let native_writer =
        run.writer.is_some() || matches!(chosen_writer.as_deref(), Some("ollama" | "openrouter"));
    if native_writer {
        let mut problems: Vec<String> = sandbox_blocker.iter().cloned().collect();
        match run.writer.as_deref() {
            // Only an Ollama or OpenRouter writer gets here: `--update`
            // with that writer asks for its model and writes it to
            // `run.models.writer`.
            None => problems.push(match other {
                None => format!(
                    "no writer model: pass --model writer=PROVIDER:MODEL, or run `axocoatl \
                     onboard --update --writer {}`",
                    chosen_writer.as_deref().unwrap_or("ollama")
                ),
                Some(other) => format!(
                    "no writer model: pass --model writer=PROVIDER:MODEL, or set \
                     run.models.writer {}",
                    other.in_file()
                ),
            }),
            Some(spec) => problems.extend(model_ok(spec).err()),
        }
        problems.extend(reviewer_problem());
        path(
            "axocoatl run fix",
            problems,
            format!(
                "writer {}, reviewer {}",
                run.writer.as_deref().unwrap_or("-"),
                reviewer.unwrap_or("-")
            ),
            true,
            true,
        );
    }
    let codex_command = match &codex_credential {
        Some(credential) => format!("axocoatl run fix-codex --param credential={credential}"),
        None => "axocoatl run fix-codex".to_string(),
    };
    // Claude Code and Codex are listed even when not set up, with the
    // command that sets them up.
    for (command, writer) in [
        ("axocoatl run fix-claude-code", &claude_code),
        (codex_command.as_str(), &codex),
    ] {
        let mut problems = Vec::new();
        let set_up = writer.set_up_started || writer.chosen;
        if set_up {
            problems.extend(sandbox_blocker.iter().cloned());
            problems.extend(writer.missing.clone());
            problems.extend(reviewer_problem());
        } else {
            problems.extend(writer.missing.clone());
        }
        path(
            command,
            problems,
            format!("reviewer {}", reviewer.unwrap_or("-")),
            set_up,
            writer.chosen,
        );
    }
    if let Some(config) = &config {
        if let Some(lead) = config.agents.iter().find(|agent| agent.id == "lead") {
            let spec = format!("{}:{}", lead.provider, lead.model);
            let mut problems: Vec<String> = if e2b {
                Vec::new()
            } else {
                sandbox_blocker.iter().cloned().collect()
            };
            if lead.provider == "ollama" || lead.provider == "openrouter" {
                problems.extend(model_ok(&spec).err());
            }
            path(
                "workbench Sessions",
                problems,
                format!("Lead {spec}"),
                true,
                true,
            );
        }
    }

    let steps_left: Vec<String> = sections
        .all()
        .filter(|item| item.status == Status::Fail)
        .map(Item::step)
        .collect();
    Report {
        ready: steps_left.is_empty(),
        config: config_path.display().to_string(),
        sections,
        ready_for,
        steps_left,
    }
}

/// Whether the key of the provider `name` (OpenRouter, OpenAI, …) is
/// there: the secret `api_key_secret` names, stored in `data_dir`'s secret
/// store (where `axocoatl connect openrouter` keeps OpenRouter's key; the
/// daemon reads it when it starts), else a value in `api_key` (written in
/// the file, or from a `${VARIABLE}` that is set). `Ok` with the line, or
/// the line and what stores the key: a command for the configuration at
/// `config_path`, which selects the data directory the key is stored in.
fn provider_key(
    name: &str,
    credentials: &axocoatl_config::ProviderCredentials,
    data_dir: Option<&Path>,
    config_path: &Path,
    default_config: &Path,
) -> Result<String, (String, String)> {
    if let Some(line) = data_dir.and_then(|data_dir| {
        crate::connect_cmd::openrouter::doctor_key_line(name, credentials, data_dir)
    }) {
        return line
            .map_err(|(line, hint)| (line, with_config_flag(&hint, config_path, default_config)));
    }
    let key = credentials.api_key.expose_secret();
    if key.is_empty() || key.contains("your-key") || key.starts_with("${") {
        Err((
            format!("{name} API key not set"),
            crate::missing_provider_key_hint(name),
        ))
    } else {
        Ok(format!("{name} API key present"))
    }
}

/// ` --config PATH` when doctor checks another configuration than the
/// default one, else nothing: a command that stores a key or adds a
/// provider then acts on that configuration and its data directory.
fn config_flag(config_path: &Path, default: &Path) -> String {
    if same_file(config_path, default) {
        String::new()
    } else {
        format!(
            " --config {}",
            crate::onboard::host::shell_quote(&config_path.display().to_string())
        )
    }
}

/// `axocoatl connect openrouter`, with `--config` when doctor checks
/// another configuration than the default one.
fn connect_openrouter_command(config_path: &Path, default: &Path) -> String {
    format!(
        "axocoatl connect openrouter{}",
        config_flag(config_path, default)
    )
}

/// `hint` (how to store a provider's key) with [`config_flag`] added to
/// each `axocoatl connect openrouter` and `axocoatl secret set` command in
/// it, so the key goes to the data directory of the configuration doctor
/// checks.
fn with_config_flag(hint: &str, config_path: &Path, default: &Path) -> String {
    let flag = config_flag(config_path, default);
    if flag.is_empty() {
        return hint.to_string();
    }
    let mut text = String::new();
    for (index, part) in hint.split('`').enumerate() {
        if index > 0 {
            text.push('`');
        }
        text.push_str(part);
        let command = index % 2 == 1;
        if command
            && (part.starts_with("axocoatl connect openrouter")
                || part.starts_with("axocoatl secret set "))
        {
            text.push_str(&flag);
        }
    }
    text
}

/// Load the configuration the way the daemon does, selecting its data
/// directory unless the environment names one.
async fn load(
    config_path: &Path,
    env: &Environment,
) -> Result<axocoatl_config::AxocoatlConfig, String> {
    if env.data_dir.is_some() {
        axocoatl_config::load_config(config_path)
            .await
            .map_err(|error| error.to_string())
    } else {
        crate::load_cli_config(config_path).await
    }
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

/// What `GET /health` of the daemon on `host:port` says its version is.
pub(crate) async fn daemon_version(host: &str, port: u16) -> Option<String> {
    let url = crate::onboard::smoke::health_url(host, port);
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?
        .get(url)
        .send()
        .await
        .ok()?;
    let health: serde_json::Value = response.json().await.ok()?;
    health
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// The program the installed Always-On Service runs, read from its
/// definition (the LaunchAgent plist or the systemd user unit).
pub(crate) fn installed_service_program(home: &Path, macos: bool) -> Option<PathBuf> {
    if macos {
        let plist =
            std::fs::read_to_string(home.join("Library/LaunchAgents/ai.axocoatl.daemon.plist"))
                .ok()?;
        let after = plist.split_once("<key>ProgramArguments</key>")?.1;
        let start = after.find("<string>")? + "<string>".len();
        let end = after[start..].find("</string>")?;
        Some(PathBuf::from(
            after[start..start + end]
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&"),
        ))
    } else {
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        let unit =
            std::fs::read_to_string(config_home.join("systemd/user/axocoatl.service")).ok()?;
        let line = unit
            .lines()
            .find_map(|line| line.strip_prefix("ExecStart="))?;
        let program = if let Some(quoted) = line.strip_prefix('"') {
            quoted.split('"').next()?.to_string()
        } else {
            line.split_whitespace().next()?.to_string()
        };
        Some(PathBuf::from(program))
    }
}

/// An external writer: Claude Code or Codex.
struct WriterPath<'a> {
    name: &'a str,
    recipe: &'a str,
    /// The credentials it runs with (any one of them).
    credentials: &'a [&'a str],
    connect: &'a str,
    /// The commands that set it up from nothing (the image and the
    /// sign-in), which leave the configuration as it is. (`axocoatl onboard
    /// --update --writer …` would also rewrite `run`: it records that
    /// writer and drops `run.models.writer`.)
    set_up: &'a str,
    /// The configuration says this is the writer (onboarding chose it), so
    /// what it lacks fails instead of warning.
    chosen: bool,
}

/// What a writer path has and lacks.
struct WriterState {
    /// Something of it is set up (an image or a credential), so doctor
    /// reports it.
    set_up_started: bool,
    chosen: bool,
    missing: Vec<String>,
}

fn writer_items(
    path: &WriterPath,
    image: bool,
    stored: &BTreeSet<String>,
    items: &mut Vec<Item>,
) -> WriterState {
    let WriterPath {
        name,
        recipe,
        credentials,
        connect,
        set_up,
        chosen,
    } = *path;
    let credential = credentials
        .iter()
        .find(|credential| stored.contains(**credential));
    let started = image || credential.is_some();
    let mut missing = Vec::new();
    if !started && !chosen {
        items.push(Item::ok(
            name,
            format!("not set up (`{set_up}` sets it up, and leaves your configuration as it is)"),
        ));
        return WriterState {
            set_up_started: false,
            chosen,
            missing: vec![format!("not set up: run `{set_up}`")],
        };
    }
    let lacking = |detail: String, fix: String| {
        if chosen {
            Item::fail(name, detail, fix)
        } else {
            Item::warn(name, detail, Some(fix))
        }
    };
    items.push(if image {
        Item::ok(name, format!("{recipe} image built"))
    } else {
        missing.push(format!(
            "its image is not built: run `axocoatl recipe build {recipe}`"
        ));
        lacking(
            format!("{recipe} image not built"),
            format!("axocoatl recipe build {recipe}"),
        )
    });
    items.push(match credential {
        Some(credential) => Item::ok(name, format!("signed in ({credential} stored)")),
        None => {
            missing.push(format!("not connected: run `{connect}`"));
            lacking(
                match credentials {
                    [only] => format!("not connected ({only} is not stored)"),
                    _ => "not connected".to_string(),
                },
                connect.to_string(),
            )
        }
    });
    WriterState {
        set_up_started: true,
        chosen,
        missing,
    }
}

/// The Ollama server and each model; returns whether the server is ready.
/// `fixes`: each model's fixes for its role. With a daemon running
/// (`daemon_running`), a model Ollama already holds is not loaded or probed
/// again, since a run may be using it (a probe would wait for the run's
/// request, and the run's next request for the probe): its context is read
/// from `GET /api/ps` instead, and a run's admission probes it before the
/// run uses it.
/// Loading a model to read its context may take `load_timeout`.
#[allow(clippy::too_many_arguments)]
async fn check_ollama(
    base_url: &str,
    host: &OllamaHost,
    wanted: &BTreeMap<String, bool>,
    fixes: &BTreeMap<String, ModelFixes>,
    env: &Environment,
    load_timeout: std::time::Duration,
    daemon_running: bool,
    items: &mut Vec<Item>,
    states: &mut BTreeMap<String, ModelState>,
) -> bool {
    let installed = match ollama_setup::installed_models(base_url).await {
        Ok(models) => models,
        Err(error) => {
            items.push(Item::fail(
                "Ollama",
                format!("Ollama not reachable at {base_url} ({error})"),
                format!("Start it: {}", host.start()),
            ));
            return false;
        }
    };
    let check = ollama_setup::native_check(base_url).await;
    let mut ready = true;
    for finding in ollama_setup::native_findings(base_url, &check, crate::DOCTOR_OTHER_OLLAMA, host)
    {
        let fix = (!finding.hint.is_empty()).then(|| finding.hint.join(" "));
        items.push(if !finding.ok {
            ready = false;
            Item {
                name: "Ollama".into(),
                status: Status::Fail,
                detail: finding.label,
                fix,
            }
        } else if finding.warn {
            Item::warn("Ollama", finding.label, fix)
        } else {
            Item::ok("Ollama", finding.label)
        });
    }
    if !ready {
        return false;
    }
    for (model, needs_tools) in wanted {
        let name = format!("Model {model}");
        let size = installed
            .iter()
            .find(|candidate| candidate.name() == model)
            .map(|candidate| candidate.size_bytes());
        if !ollama_setup::is_installed(model, &installed) {
            let known = ollama_setup::known_size(model);
            let size = known
                .map(|size| format!(" ({})", ollama_setup::size_text(size)))
                .unwrap_or_default();
            let too_large_here = match (known, env.memory_bytes) {
                (Some(size), Some(memory)) if !ollama_setup::fits_memory(size, memory) => Some((
                    ollama_setup::size_text(size),
                    axocoatl_isolation::host::memory_gb(memory),
                )),
                _ => None,
            };
            // A pull would not help a model too large for this computer:
            // the fix is a smaller model or OpenRouter, for its role.
            items.push(match too_large_here {
                Some((size, gb)) => Item::fail(
                    &name,
                    format!(
                        "{model} is not pulled, and at {size} it is too large for this \
                         computer's {gb} GB"
                    ),
                    fixes
                        .get(model)
                        .map(|fixes| fixes.too_large.clone())
                        .unwrap_or_default(),
                ),
                // Another tag of the same model does not count (Ollama
                // reads an untagged name as `:latest`): the hint names it
                // exactly.
                None => Item::fail(
                    &name,
                    format!("{model} is not pulled"),
                    ollama_setup::not_pulled_hint(
                        model,
                        &format!("ollama pull {model}{size}"),
                        &installed,
                    ),
                ),
            });
            states.insert(
                model.clone(),
                ModelState {
                    ok: false,
                    why: "not pulled".into(),
                },
            );
            continue;
        }
        let finding = ollama_setup::model_finding(base_url, model, *needs_tools).await;
        if !finding.ok {
            items.push(Item::fail(
                &name,
                finding.label.clone(),
                finding.hint.join(" "),
            ));
            states.insert(
                model.clone(),
                ModelState {
                    ok: false,
                    why: finding.label,
                },
            );
            continue;
        }
        let mut parts = vec!["GGUF".to_string()];
        let mut status = Status::Ok;
        let mut fix = None;
        if let (Some(size), Some(memory)) = (size.filter(|size| *size > 0), env.memory_bytes) {
            if ollama_setup::fits_memory(size, memory) {
                parts.push(format!("{}, fits in memory", ollama_setup::size_text(size)));
            } else {
                status = Status::Warn;
                parts.push(format!(
                    "{}, too large for this computer's {} GB",
                    ollama_setup::size_text(size),
                    axocoatl_isolation::host::memory_gb(memory)
                ));
                fix = fixes.get(model).map(|fixes| fixes.too_large.clone());
            }
        }
        let mut model_ok = true;
        let mut why = String::new();
        let read = env.contexts_read.get(model).copied();
        // Held by Ollama while a daemon runs: read, not loaded again.
        let held = match (read, daemon_running && env.load_models) {
            (None, true) => ollama_setup::loaded_context(base_url, model).await,
            _ => None,
        };
        if env.load_models {
            let observed = match (read, held) {
                (Some(tokens), _) => Ok((tokens, String::new())),
                (None, Some(tokens)) => Ok((tokens, String::new())),
                (None, None) => {
                    match &env.on_load {
                        Some(say) => say(model),
                        None => eprintln!("  (loading {model} once to read its context…)"),
                    }
                    ollama_setup::observed_context(base_url, model, load_timeout).await
                }
            };
            match observed {
                Ok((context, _)) if context < ollama_setup::MIN_CONTEXT_TOKENS => {
                    status = Status::Fail;
                    model_ok = false;
                    why = format!(
                        "loaded with a {context}-token context; a fix run needs at least {}",
                        ollama_setup::MIN_CONTEXT_TOKENS
                    );
                    parts.push(why.clone());
                    fix = Some(format!(
                        "Give Ollama a {}-token context: {}; or use OpenRouter for this model",
                        ollama_setup::CONTEXT_FIX_TOKENS,
                        host.context()
                    ));
                }
                Ok((context, _)) if held.is_some() => {
                    parts.push(format!(
                        "{context}-token context, read from Ollama without loading or probing \
                         it again (a daemon is running and may be using it; a run probes it \
                         before it uses it)"
                    ));
                    if let Some((loaded, vram)) = ollama_setup::loaded_memory(base_url, model).await
                    {
                        parts.push(ollama_setup::loaded_memory_text(loaded, vram));
                    }
                }
                Ok((context, _)) => {
                    parts.push(format!("{context}-token context, contract probes passed"));
                    if let Some((loaded, vram)) = ollama_setup::loaded_memory(base_url, model).await
                    {
                        parts.push(ollama_setup::loaded_memory_text(loaded, vram));
                    }
                }
                Err(error) => {
                    status = Status::Fail;
                    model_ok = false;
                    why = error.clone();
                    parts.push(format!("its context could not be read: {error}"));
                    fix = fixes.get(model).map(|fixes| fixes.unreadable.clone());
                }
            }
        }
        items.push(Item {
            name: name.clone(),
            status,
            detail: format!("{model}: {}", parts.join("; ")),
            fix,
        });
        states.insert(model.clone(), ModelState { ok: model_ok, why });
    }
    true
}

/// The report as `doctor` prints it.
pub(crate) fn text(report: &Report) -> String {
    let mut out = format!("Axocoatl doctor (config: {})\n", report.config);
    for (title, items) in [
        ("Sandbox", &report.sections.sandbox),
        ("Models", &report.sections.models),
        ("Writers", &report.sections.writers),
        ("Daemon", &report.sections.daemon),
    ] {
        if items.is_empty() {
            continue;
        }
        out.push_str(&format!("\n{title}\n"));
        for item in items {
            let tag = match item.status {
                Status::Ok => "[ OK ]",
                Status::Warn => "[WARN]",
                Status::Fail => "[FAIL]",
            };
            out.push_str(&format!("  {tag} {}\n", item.line()));
            if let Some(fix) = &item.fix {
                if item.status != Status::Ok {
                    out.push_str(&format!("         → {fix}\n"));
                }
            }
        }
    }
    out.push('\n');
    out.push_str(&summary(report));
    out
}

/// "Ready for" or "Not ready yet", then `Ready: yes|no` as the last line.
pub(crate) fn summary(report: &Report) -> String {
    let mut out = String::new();
    let ready: Vec<&RunPath> = report.ready_for.iter().filter(|path| path.ok).collect();
    if report.ready && !ready.is_empty() {
        out.push_str(&format!(
            "Ready for: {}\n",
            ready
                .iter()
                .map(|path| path.command.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        for path in &report.ready_for {
            // An optional writer not set up is not a failure: neutral; one
            // partly set up is a warning.
            out.push_str(&format!(
                "  {} {} ({})\n",
                match (path.ok, path.set_up, path.required) {
                    (true, _, _) => "✓",
                    (false, _, true) => "✗",
                    (false, true, false) => "!",
                    (false, false, false) => "·",
                },
                path.command,
                path.detail
            ));
        }
    } else if !report.ready {
        let count = report.steps_left.len();
        out.push_str(&format!(
            "Not ready yet, {count} step{} left:\n",
            if count == 1 { "" } else { "s" }
        ));
        for (index, step) in report.steps_left.iter().enumerate() {
            out.push_str(&format!("  {}) {step}\n", index + 1));
        }
    } else {
        out.push_str("Nothing to run yet: no run defaults and no workbench Lead are configured.\n");
    }
    out.push_str(&format!(
        "Ready: {}\n",
        if report.ready { "yes" } else { "no" }
    ));
    out
}

/// `axocoatl doctor [--json]`: exit 0 when nothing failed, 1 otherwise.
pub(crate) async fn cmd_doctor(config_path: &Path, json: bool) -> i32 {
    let env = match Environment::current().await {
        Ok(env) => env,
        Err(error) => {
            eprintln!("✗ {error}");
            return 1;
        }
    };
    let report = collect(config_path, &env).await;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("the report serializes")
        );
    } else {
        print!("{}", text(&report));
    }
    if report.ready {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(
        writer: &'static str,
        run_writer: Option<ModelRef>,
        reviewer: ModelRef,
    ) -> onboard_config::ConfigPlan {
        onboard_config::ConfigPlan {
            ollama_base_url: Some("http://localhost:11434".into()),
            openrouter_key: (reviewer.provider == Provider::OpenRouter)
                .then(|| onboard_config::KeyRef::Env("OPENROUTER_API_KEY".into())),
            lead: run_writer.clone().unwrap_or_else(|| reviewer.clone()),
            reviewer: reviewer.clone(),
            run_writer,
            run_reviewer: Some(reviewer),
            port: 8080,
            note: "test".into(),
            writer,
            codex_credential: None,
        }
    }

    /// The configuration in `text`, the models of the Agents `onboard
    /// --update` keeps as they are, and its run defaults.
    fn parsed(
        text: &str,
    ) -> (
        axocoatl_config::AxocoatlConfig,
        Vec<String>,
        onboard_config::RunModels,
    ) {
        (
            axocoatl_config::parse_config(text, Path::new("config.yaml")).unwrap(),
            onboard_config::kept_agent_models(&onboard_config::read_existing(text).unwrap(), text)
                .unwrap_or_default(),
            onboard_config::run_models(text),
        )
    }

    /// A Claude Code (or Codex) setup whose local reviewer is too large for
    /// this computer: the fix moves the reviewer to OpenRouter and keeps
    /// the writer (it used to say `--writer openrouter`, which replaces
    /// Claude Code). It says `--update`, which moves the Agents onboarding
    /// wrote with `run.models`, so doctor no longer checks the model: shown
    /// here with the configuration the command writes.
    #[test]
    fn a_too_large_reviewer_moves_only_the_reviewer() {
        let local = ModelRef::new(Provider::Ollama, "gpt-oss:20b");
        let hosted = ModelRef::new(Provider::OpenRouter, "openai/gpt-oss-120b");
        for writer in ["claude-code", "codex"] {
            let written = onboard_config::render(&plan(writer, None, local.clone()));
            let (_, kept, run) = parsed(&written);
            assert!(kept.is_empty(), "{kept:?}");
            assert_eq!(
                too_large_fix("gpt-oss:20b", &kept, &run, Some(writer), None),
                format!(
                    "Use OpenRouter for the reviewer (`axocoatl onboard --update --writer \
                     {writer} --reviewer openrouter:openai/gpt-oss-120b`), or choose a smaller \
                     reviewer (`axocoatl onboard --update --writer {writer}`)"
                )
            );
            let moved = plan(writer, None, hosted.clone());
            // --update: run.models and the Agents move off gpt-oss:20b.
            let updated =
                onboard_config::update(&written, &moved, Path::new("config.yaml")).unwrap();
            assert!(updated.kept.is_empty(), "{:?}", updated.kept);
            let (config, _, run) = parsed(&updated.text);
            assert_eq!(
                run.reviewer.as_deref(),
                Some("openrouter:openai/gpt-oss-120b")
            );
            assert!(ollama_models(&config, &run).is_empty());
            // The Agents as --replace writes them.
            assert_eq!(
                onboard_config::read_existing(&onboard_config::render(&moved))
                    .unwrap()
                    .agents,
                onboard_config::read_existing(&updated.text).unwrap().agents
            );
        }

        // A local writer that fits, with a reviewer that does not.
        let writer = ModelRef::new(Provider::Ollama, "qwen3-coder:30b");
        let written = onboard_config::render(&plan("ollama", Some(writer), local));
        let (_, kept, run) = parsed(&written);
        assert_eq!(
            too_large_fix("gpt-oss:20b", &kept, &run, Some("ollama"), None),
            "Use OpenRouter for the reviewer (`axocoatl onboard --update --writer ollama \
             --reviewer openrouter:openai/gpt-oss-120b`), or choose a smaller reviewer \
             (`axocoatl onboard --update --writer ollama`)"
        );
        // The writer (Lead) model keeps the writer's fix, naming the
        // writer so `--update` does not ask Step 1 again.
        assert_eq!(
            too_large_fix("qwen3-coder:30b", &kept, &run, Some("ollama"), None),
            "Choose a smaller model (`axocoatl onboard --update --writer ollama`), or use \
             OpenRouter for it (`axocoatl onboard --update --writer openrouter`)"
        );
        assert_eq!(
            unreadable_model_fix("qwen3-coder:30b", &kept, &run, Some("ollama"), None),
            "Check the model with `ollama run qwen3-coder:30b`, or choose another (`axocoatl \
             onboard --update --writer ollama`)"
        );
        assert_eq!(
            unreadable_model_fix("gpt-oss:20b", &kept, &run, Some("ollama"), None),
            "Check the model with `ollama run gpt-oss:20b`, or choose another reviewer \
             (`axocoatl onboard --update --writer ollama`)"
        );
        // A Reviewer edited by hand (its prompt) stays as it is with
        // --update: then the fix is --replace, which names the writer model.
        let edited = written.replace(
            "Report each defect with file:line",
            "Report each defect with its file and line",
        );
        let (_, kept, run) = parsed(&edited);
        assert_eq!(kept, ["gpt-oss:20b"]);
        assert_eq!(
            too_large_fix("gpt-oss:20b", &kept, &run, Some("ollama"), None),
            "Use OpenRouter for the reviewer (`axocoatl onboard --replace --writer ollama \
             --writer-model ollama:qwen3-coder:30b --reviewer openrouter:openai/gpt-oss-120b`), or \
             choose a smaller reviewer (`axocoatl onboard --replace --writer ollama --writer-model \
             ollama:qwen3-coder:30b`) (`--update` would keep the Agents you edited, which run \
             gpt-oss:20b; `--replace` backs this configuration up first)"
        );
    }

    /// A reviewer named only in `run.models` (no Agent runs it): `--update`
    /// changes it, and keeps the OpenRouter writer, whose reviewer runs on
    /// OpenRouter too.
    #[test]
    fn a_too_large_reviewer_no_agent_runs_is_updated() {
        let text = "agents:\n  - id: lead\n    name: Lead\n    provider: openrouter\n    model: \
                    \"qwen/qwen3-coder\"\nproviders:\n  ollama:\n    base_url: \
                    \"http://localhost:11434\"\n  openrouter:\n    api_key: \
                    \"${OPENROUTER_API_KEY}\"\nrun:\n  models:\n    writer: \
                    \"openrouter:qwen/qwen3-coder\"\n    reviewer: \"ollama:gpt-oss:20b\"\n";
        let (_, kept, run) = parsed(text);
        assert_eq!(
            too_large_fix("gpt-oss:20b", &kept, &run, Some("openrouter"), None),
            "Use OpenRouter for the reviewer (`axocoatl onboard --update --writer openrouter \
             --reviewer openrouter:openai/gpt-oss-120b`)"
        );
    }

    /// A configuration with an OpenRouter writer, or none onboarding
    /// recorded: the command never asks Step 1 again nor switches the
    /// writer. `--replace` keeps the OpenRouter writer's model, and with no
    /// writer known the person is told to name one.
    #[test]
    fn a_too_large_model_keeps_an_openrouter_or_unknown_writer() {
        // Hand-edited: the OpenRouter setup's Reviewer Agent on a local model.
        let hosted = ModelRef::new(Provider::OpenRouter, "qwen/qwen3-coder");
        let local = ModelRef::new(Provider::Ollama, "gpt-oss:20b");
        let text = onboard_config::render(&plan(
            "openrouter",
            Some(hosted),
            ModelRef::new(Provider::OpenRouter, "openai/gpt-oss-120b"),
        ))
        .replace(
            "provider: openrouter\n    model: \"openai/gpt-oss-120b\"",
            "provider: ollama\n    model: \"gpt-oss:20b\"",
        );
        let (config, kept, run) = parsed(&text);
        assert!(ollama_models(&config, &run).contains_key("gpt-oss:20b"));
        let fix = too_large_fix("gpt-oss:20b", &kept, &run, Some("openrouter"), None);
        assert_eq!(
            fix,
            "Use OpenRouter for it (`axocoatl onboard --replace --writer openrouter \
             --writer-model openrouter:qwen/qwen3-coder`) (`--update` would keep the Agents you \
             edited, which run gpt-oss:20b; `--replace` backs this configuration up first)"
        );
        // That command writes the team on OpenRouter: no Ollama model is left.
        let (config, _, run) = parsed(&onboard_config::render(&plan(
            "openrouter",
            Some(ModelRef::new(Provider::OpenRouter, "qwen/qwen3-coder")),
            ModelRef::new(Provider::OpenRouter, "openai/gpt-oss-120b"),
        )));
        assert!(ollama_models(&config, &run).is_empty());

        // An OpenRouter writer whose run reviewer and Reviewer Agent are
        // local, as onboarding writes them: the reviewer moves with
        // --update, which keeps the writer model.
        let (_, kept, run) = parsed(&onboard_config::render(&plan(
            "openrouter",
            Some(ModelRef::new(Provider::OpenRouter, "qwen/qwen3-coder")),
            local,
        )));
        assert!(kept.is_empty(), "{kept:?}");
        assert_eq!(
            too_large_fix("gpt-oss:20b", &kept, &run, Some("openrouter"), None),
            "Use OpenRouter for the reviewer (`axocoatl onboard --update --writer openrouter \
             --reviewer openrouter:openai/gpt-oss-120b`)"
        );

        // No `# Writer:` comment and no run.models.writer: who writes is not
        // known, so no command that would ask or guess it.
        let text = "agents:\n  - id: lead\n    name: Lead\n    provider: ollama\n    model: \
                    \"gpt-oss:20b\"\nproviders:\n  ollama:\n    base_url: \
                    \"http://localhost:11434\"\nrun:\n  models:\n    reviewer: \
                    \"ollama:gpt-oss:20b\"\n";
        let (_, kept, run) = parsed(text);
        assert_eq!(onboard_config::writer_marker(text), None);
        let expected = "Name a smaller model (or an OpenRouter one) in place of gpt-oss:20b in \
                        this configuration, or write a new one (this one is backed up first) \
                        with `axocoatl onboard --replace --writer WRITER`, where WRITER is \
                        claude-code, codex, ollama or openrouter";
        assert_eq!(
            too_large_fix("gpt-oss:20b", &kept, &run, None, None),
            expected
        );
        crate::paste_safe::assert_paste_safe(expected);
        crate::paste_safe::assert_paste_safe(&unreadable_model_fix(
            "gpt-oss:20b",
            &kept,
            &run,
            None,
            None,
        ));
        assert!(
            unreadable_model_fix("gpt-oss:20b", &kept, &run, None, None).starts_with(
                "Check the model with `ollama run gpt-oss:20b`, or name another model in place \
                 of gpt-oss:20b in this configuration"
            )
        );
        // `doctor --config` for another configuration: `axocoatl onboard`
        // has no --config and would back up and replace the default one,
        // so no onboarding command is offered for this one, whoever writes.
        let path = Path::new("/srv/team a/axocoatl.yaml");
        let default = PathBuf::from("/home/alex/.config/axocoatl/config.yaml");
        let other = OtherConfig::of(path, &default);
        for writer in [
            None,
            Some("ollama"),
            Some("claude-code"),
            Some("openrouter"),
        ] {
            let too_large = too_large_fix("gpt-oss:20b", &kept, &run, writer, other);
            assert_eq!(
                too_large,
                format!(
                    "Name a smaller model, or an OpenRouter one, in place of gpt-oss:20b in \
                     /srv/team a/axocoatl.yaml (`axocoatl onboard` writes only the default \
                     configuration, {})",
                    default.display()
                )
            );
            let unreadable = unreadable_model_fix("gpt-oss:20b", &kept, &run, writer, other);
            assert_eq!(
                unreadable,
                format!(
                    "Check the model with `ollama run gpt-oss:20b`, or name another model in \
                     place of gpt-oss:20b in /srv/team a/axocoatl.yaml (`axocoatl onboard` \
                     writes only the default configuration, {})",
                    default.display()
                )
            );
            for fix in [too_large, unreadable] {
                assert!(
                    !fix.contains("--replace") && !fix.contains("--update"),
                    "{fix}"
                );
                assert!(!fix.contains("this one"), "{fix}");
                crate::paste_safe::assert_paste_safe(&fix);
            }
        }
        assert!(OtherConfig::of(&default, &default).is_none());
        // No comment, but run.models.writer says Ollama writes (doctor's
        // chosen writer): named, with its model, which `--replace` keeps.
        let text = text.replace(
            "    reviewer: \"ollama:gpt-oss:20b\"\n",
            "    writer: \"ollama:qwen3-coder:30b\"\n    reviewer: \"ollama:gpt-oss:20b\"\n",
        );
        let (_, kept, run) = parsed(&text);
        assert!(
            too_large_fix("gpt-oss:20b", &kept, &run, Some("ollama"), None).starts_with(
                "Use OpenRouter for the reviewer (`axocoatl onboard --replace --writer ollama \
             --writer-model ollama:qwen3-coder:30b --reviewer openrouter:openai/gpt-oss-120b`)"
            )
        );

        // A Claude Code setup with an Agent on a local model that is not the
        // reviewer (hand-edited): the writer stays Claude Code.
        let (_, kept, run) = parsed(
            &onboard_config::render(&plan(
                "claude-code",
                None,
                ModelRef::new(Provider::OpenRouter, "openai/gpt-oss-120b"),
            ))
            .replacen(
                "provider: openrouter\n    model: \"openai/gpt-oss-120b\"",
                "provider: ollama\n    model: \"qwen3-coder:30b\"",
                1,
            ),
        );
        assert_eq!(
            too_large_fix("qwen3-coder:30b", &kept, &run, Some("claude-code"), None),
            "Choose a smaller model (`axocoatl onboard --replace --writer claude-code`), or use \
             OpenRouter for it (`axocoatl onboard --replace --writer claude-code --reviewer \
             openrouter:openai/gpt-oss-120b`) (`--update` would keep the Agents you edited, which \
             run qwen3-coder:30b; `--replace` backs this configuration up first)"
        );
    }

    /// Linux: a helper with file capabilities (Fedora 42 and RHEL 9 ship
    /// newuidmap with `cap_setuid=ep`, as `rpm -q --filecaps shadow-utils`
    /// shows) may map ids, as a setuid one may; a plain one may not.
    /// Writing `security.capability` needs CAP_SETFCAP, so without it the
    /// test says it was skipped.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_helper_with_file_capabilities_may_map_ids() {
        use std::os::unix::ffi::OsStrExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("newuidmap");
        std::fs::write(&path, "").unwrap();
        assert!(!may_map_ids(&path));
        // What `setcap cap_setuid=ep` writes: VFS_CAP_REVISION_2 with the
        // effective flag, then CAP_SETUID (7) permitted, nothing
        // inheritable, and the empty upper words.
        let mut value = Vec::new();
        for word in [0x0200_0001u32, 1 << 7, 0, 0, 0] {
            value.extend_from_slice(&word.to_le_bytes());
        }
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: both strings are NUL-terminated and `value` is valid for
        // its length.
        let written = unsafe {
            libc::setxattr(
                name.as_ptr(),
                c"security.capability".as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        if written != 0 {
            eprintln!(
                "skipped: writing security.capability needs CAP_SETFCAP ({})",
                std::io::Error::last_os_error()
            );
            return;
        }
        assert!(may_map_ids(&path));
    }

    fn item(name: &str, status: Status) -> Item {
        Item {
            name: name.into(),
            status,
            detail: format!("{name} detail"),
            fix: (status != Status::Ok).then(|| format!("fix {name}")),
        }
    }

    fn report(fail: bool) -> Report {
        let mut sections = Sections::default();
        sections
            .sandbox
            .push(item("Podman", if fail { Status::Fail } else { Status::Ok }));
        sections.models.push(item("Ollama", Status::Warn));
        Report {
            ready: !fail,
            config: "/c.yaml".into(),
            steps_left: if fail {
                vec!["Podman detail: fix Podman".into()]
            } else {
                Vec::new()
            },
            sections,
            ready_for: vec![
                RunPath {
                    command: "axocoatl run fix".into(),
                    ok: !fail,
                    detail: "writer ollama:qwen3-coder:30b, reviewer ollama:gpt-oss:20b".into(),
                    set_up: true,
                    required: true,
                },
                RunPath {
                    command: "axocoatl run fix-codex".into(),
                    ok: false,
                    detail: "not connected: run `axocoatl connect codex`".into(),
                    set_up: true,
                    required: false,
                },
                RunPath {
                    command: "axocoatl run fix-claude-code".into(),
                    ok: false,
                    detail: "not set up: run `axocoatl recipe build claude-code && axocoatl \
                             connect claude-code`"
                        .into(),
                    set_up: false,
                    required: false,
                },
            ],
        }
    }

    /// The JSON contract other tools read: four sections, items with
    /// name/status/detail/fix, and `ready`.
    #[test]
    fn the_json_report_has_the_documented_shape() {
        let value = serde_json::to_value(report(true)).unwrap();
        assert_eq!(value["ready"], false);
        for section in ["sandbox", "models", "writers", "daemon"] {
            assert!(value["sections"][section].is_array(), "{section}");
        }
        let podman = &value["sections"]["sandbox"][0];
        assert_eq!(podman["name"], "Podman");
        assert_eq!(podman["status"], "fail");
        assert_eq!(podman["fix"], "fix Podman");
        assert_eq!(value["sections"]["models"][0]["status"], "warn");
        assert_eq!(value["steps_left"][0], "Podman detail: fix Podman");
        assert_eq!(value["ready_for"][0]["command"], "axocoatl run fix");
    }

    /// Never a ✓ banner after a failure: the summary lists the steps left
    /// and ends with `Ready: no`; a ready report names what it is ready for.
    #[test]
    fn the_summary_says_ready_only_without_failures() {
        let not_ready = text(&report(true));
        assert!(not_ready.contains("[FAIL] Podman detail\n         → fix Podman"));
        assert!(not_ready.contains("Not ready yet, 1 step left:\n  1) Podman detail: fix Podman"));
        assert!(!not_ready.contains("Ready for"));
        assert!(not_ready.ends_with("Ready: no\n"));
        let ready = text(&report(false));
        assert!(ready.contains("Ready for: axocoatl run fix\n"));
        assert!(ready.contains(
            "  ✓ axocoatl run fix (writer ollama:qwen3-coder:30b, reviewer ollama:gpt-oss:20b)"
        ));
        // An optional writer partly set up is a warning: a ready setup
        // lists no ✗.
        assert!(ready
            .contains("  ! axocoatl run fix-codex (not connected: run `axocoatl connect codex`)"));
        assert!(!ready.contains('✗'), "{ready}");
        // An optional writer nothing of is set up is neutral, not a failure.
        assert!(
            ready.contains(
                "  · axocoatl run fix-claude-code (not set up: run `axocoatl recipe build \
                 claude-code && axocoatl connect claude-code`)"
            ),
            "{ready}"
        );
        assert!(!ready.contains("✗ axocoatl run fix-claude-code"), "{ready}");
        assert_eq!(
            serde_json::to_value(report(false)).unwrap()["ready_for"][2]["set_up"],
            false
        );
        assert!(ready.ends_with("Ready: yes\n"));
    }

    /// A line names what it is about once: the item's name only when the
    /// detail does not start with it.
    #[test]
    fn a_line_names_its_subject_once() {
        let line = |name: &str, detail: &str| Item::ok(name, detail).line();
        assert_eq!(
            line(
                "Codex",
                "not set up (`axocoatl connect codex` sets it up, and leaves your configuration \
                 as it is)"
            ),
            "Codex: not set up (`axocoatl connect codex` sets it up, and leaves your \
             configuration as it is)"
        );
        assert_eq!(line("Landlock", "Landlock ABI 6"), "Landlock ABI 6");
        assert_eq!(
            line("Model gpt-oss:20b", "gpt-oss:20b: GGUF"),
            "gpt-oss:20b: GGUF"
        );
        assert_eq!(
            line(
                "Model qwen3-coder:30b",
                "Model 'qwen3-coder:30b' cannot run in native Sessions"
            ),
            "Model 'qwen3-coder:30b' cannot run in native Sessions"
        );
        assert_eq!(
            line("Daemon", "running (Axocoatl 1.5.0)"),
            "Daemon: running (Axocoatl 1.5.0)"
        );
    }

    #[test]
    fn landlock_abi_decides_what_runs() {
        let debian = detect::OsRelease::parse("ID=debian\nVERSION_ID=12\n");
        let old = landlock_item(Some(2), Some(&debian), false);
        assert_eq!(old.status, Status::Fail);
        assert!(old.detail.contains("Linux 6.2"));
        assert!(old.fix.unwrap().contains("bookworm-backports"));
        let wsl = landlock_item(Some(1), None, true);
        assert!(wsl.fix.unwrap().contains("wsl --update"));
        assert_eq!(landlock_item(None, None, false).status, Status::Fail);
        // ABI 1 also refuses the cross-directory renames Rust builds make.
        let one = landlock_item(Some(1), None, false);
        assert_eq!(one.status, Status::Fail);
        assert!(
            one.detail
                .contains("Rust builds (cargo, rustc) fail with \"Invalid cross-device link\""),
            "{}",
            one.detail
        );
        assert!(!old.detail.contains("cross-device"), "{}", old.detail);
        let three = landlock_item(Some(3), None, false);
        assert_eq!(three.status, Status::Warn);
        assert!(three.detail.contains("6.7"));
        assert_eq!(landlock_item(Some(6), None, false).status, Status::Ok);
    }

    /// A key-storing hint for a configuration other than the default one
    /// names it, so `connect openrouter` and `secret set` store the key in
    /// its data directory; for the default one it is unchanged.
    #[test]
    fn key_hints_name_a_configuration_that_is_not_the_default() {
        let other = Path::new("/tmp/team a/axocoatl.yaml");
        let default = crate::default_config_path_for_clap();
        assert_eq!(
            with_config_flag(
                "Run `axocoatl connect openrouter --secret work`, which signs you in",
                other,
                &default
            ),
            "Run `axocoatl connect openrouter --secret work --config '/tmp/team a/axocoatl.yaml'`, \
             which signs you in"
        );
        assert_eq!(
            with_config_flag(
                "Pipe the key into `axocoatl secret set mistral` (it reads standard input) or \
                 name a variable with `--from-env`",
                other,
                &default
            ),
            "Pipe the key into `axocoatl secret set mistral --config '/tmp/team a/axocoatl.yaml'` \
             (it reads standard input) or name a variable with `--from-env`"
        );
        let hint = "Run `axocoatl connect openrouter`, which signs you in";
        assert_eq!(with_config_flag(hint, &default, &default), hint);
        assert_eq!(
            connect_openrouter_command(other, &default),
            "axocoatl connect openrouter --config '/tmp/team a/axocoatl.yaml'"
        );
        assert_eq!(
            connect_openrouter_command(&default, &default),
            "axocoatl connect openrouter"
        );
    }

    #[test]
    fn podman_install_fixes_follow_the_platform() {
        let intel_mac = Os::MacOs {
            version: None,
            apple_silicon: false,
            chip: None,
        };
        for homebrew in [true, false] {
            let intel = podman_install_fix(&intel_mac, homebrew);
            assert!(intel.contains("Podman 6 no longer supports Intel Macs"));
            assert!(!intel.contains("brew install"));
        }
        let mac = Os::MacOs {
            version: None,
            apple_silicon: true,
            chip: None,
        };
        let arm = podman_install_fix(&mac, true);
        assert!(arm.contains("it installs Podman after asking"), "{arm}");
        assert!(arm.contains("brew install podman"));
        // Without Homebrew onboarding cannot install Podman: the installer.
        let no_brew = podman_install_fix(&mac, false);
        assert!(no_brew.starts_with("Install Podman with the installer from https://podman.io"));
        assert!(
            no_brew.ends_with("then run `axocoatl onboard`"),
            "{no_brew}"
        );
        assert!(!no_brew.contains("it installs Podman"), "{no_brew}");
        let ubuntu = Os::Linux {
            release: detect::OsRelease::parse("ID=ubuntu\nID_LIKE=debian\n"),
            wsl: None,
            systemd: true,
        };
        for homebrew in [true, false] {
            let fix = podman_install_fix(&ubuntu, homebrew);
            assert!(fix.ends_with("`sudo apt-get install -y podman`"), "{fix}");
            assert!(!fix.contains("brew"), "{fix}");
        }
        // Onboarding refuses Ubuntu 22.04's Podman, so doctor does not say it
        // installs it there.
        let jammy = Os::Linux {
            release: detect::OsRelease::parse("ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"22.04\"\n"),
            wsl: None,
            systemd: true,
        };
        let fix = podman_install_fix(&jammy, false);
        assert!(fix.contains("use Ubuntu 24.04 or newer"), "{fix}");
        assert!(!fix.contains("it installs Podman"), "{fix}");
    }

    /// The daemon decides the route from `/etc/os-release` itself
    /// (`InstallRoute::for_linux`, for the workbench's readiness item); it
    /// must decide what onboarding and doctor decide.
    #[test]
    fn the_daemon_reads_the_same_podman_install_route_as_onboarding() {
        for os_release in [
            "ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\n",
            "ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"22.04\"\n",
            "ID=ubuntu\nVERSION_ID=\"20.04\"\n",
            "ID=debian\nVERSION_ID=\"13\"\n",
            "ID=linuxmint\nID_LIKE=\"ubuntu debian\"\nVERSION_ID=\"22\"\n",
            "ID=pop\nID_LIKE=\"ubuntu debian\"\nVERSION_ID=\"24.04\"\n",
            "ID=fedora\nVERSION_ID=42\n",
            "ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\nVERSION_ID=\"9.4\"\n",
            "ID=centos\n",
            "ID=arch\n",
            "ID=manjaro\nID_LIKE=arch\n",
            "ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n",
            "ID=\"sles\"\nVERSION_ID=\"15.6\"\n",
            "ID=nixos\n",
            "ID=alpine\n",
            "",
        ] {
            let os = Os::Linux {
                release: detect::OsRelease::parse(os_release),
                wsl: None,
                systemd: true,
            };
            assert_eq!(
                podman_install_route(&os, false),
                axocoatl_isolation::podman::InstallRoute::for_linux(os_release),
                "{os_release}"
            );
        }
    }

    #[test]
    fn the_installed_service_program_is_read_from_its_definition() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(installed_service_program(home.path(), true), None);
        let agents = home.path().join("Library/LaunchAgents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("ai.axocoatl.daemon.plist"),
            "<dict><key>ProgramArguments</key>\n  <array>\n    <string>/opt/a &amp; b/axocoatl</string>\n    <string>serve</string>",
        )
        .unwrap();
        assert_eq!(
            installed_service_program(home.path(), true),
            Some(PathBuf::from("/opt/a & b/axocoatl"))
        );
    }

    fn writer(image: bool, stored: &[&str], chosen: bool) -> (WriterState, Vec<Item>) {
        let mut items = Vec::new();
        let stored: BTreeSet<String> = stored.iter().map(|s| s.to_string()).collect();
        let state = writer_items(
            &WriterPath {
                name: "Codex",
                recipe: "codex",
                credentials: &["codex-chatgpt", "codex-openai"],
                connect: "axocoatl connect codex",
                set_up: "axocoatl connect codex",
                chosen,
            },
            image,
            &stored,
            &mut items,
        );
        (state, items)
    }

    #[test]
    fn a_writer_path_is_reported_once_part_of_it_is_set_up() {
        let (state, items) = writer(false, &[], false);
        assert!(!state.set_up_started);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, Status::Ok);
        // The commands that set it up without rewriting the configuration
        // (`onboard --update --writer codex` would also make it the writer
        // and drop run.models.writer).
        assert_eq!(
            items[0].line(),
            "Codex: not set up (`axocoatl connect codex` sets it up, and leaves your \
             configuration as it is)"
        );
        assert_eq!(state.missing, ["not set up: run `axocoatl connect codex`"]);
        let (state, items) = writer(true, &[], false);
        assert!(state.set_up_started);
        assert_eq!(
            state.missing,
            ["not connected: run `axocoatl connect codex`"]
        );
        assert_eq!(items[1].status, Status::Warn);
        let (state, _) = writer(true, &["codex-openai"], false);
        assert!(state.missing.is_empty());
    }

    /// The writer the configuration names fails when its image or its
    /// sign-in is missing: doctor never says ready for a Claude Code or
    /// Codex setup that cannot run.
    #[test]
    fn the_chosen_writer_fails_without_its_image_or_sign_in() {
        let (state, items) = writer(false, &[], true);
        assert!(state.chosen);
        assert_eq!(
            items.iter().map(|item| item.status).collect::<Vec<_>>(),
            [Status::Fail, Status::Fail]
        );
        assert_eq!(items[0].fix.as_deref(), Some("axocoatl recipe build codex"));
        assert_eq!(items[1].fix.as_deref(), Some("axocoatl connect codex"));
        let (_, items) = writer(true, &["codex-chatgpt"], true);
        assert!(items.iter().all(|item| item.status == Status::Ok));
    }

    /// End-to-end on Linux: what doctor finds about this computer's
    /// `newuidmap` and `newgidmap` with its real package manager (dpkg,
    /// rpm, pacman) and its configuration, then the same after a recorded
    /// reinstall that did not help. Run it in a distribution's container,
    /// after changing a helper (`setcap -r /usr/bin/newuidmap`), with
    /// `AXOCOATL_EXPECT_ID_MAP` set to words the result must contain:
    ///
    /// ```text
    /// AXOCOATL_EXPECT_ID_MAP='pacman -S --noconfirm shadow' \
    ///   axocoatl-<hash> doctor::tests::this_linux_computers_id_map_helpers \
    ///   --ignored --exact --nocapture
    /// ```
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "reads this Linux computer's package manager; run it explicitly"]
    async fn this_linux_computers_id_map_helpers() {
        let data = tempfile::tempdir().unwrap();
        let mut host = crate::onboard::host::Host::from_env(None).unwrap();
        host.data_home = data.path().to_path_buf();
        let release = detect::OsRelease::parse(
            &std::fs::read_to_string("/etc/os-release").unwrap_or_default(),
        );
        let os = Os::Linux {
            release,
            wsl: None,
            systemd: false,
        };
        let tools = id_map_tools(&host, Path::new("/"), &os).await;
        let said = format!("{tools:#?}\n{}", tools.describe());
        println!("{said}");
        if let Ok(expected) = std::env::var("AXOCOATL_EXPECT_ID_MAP") {
            assert!(said.contains(&expected), "{expected:?} not in:\n{said}");
        }
        let Some(install) = tools.install.clone().filter(|_| !tools.usable()) else {
            return;
        };
        // A reinstall that ran and did not help is kept, and doctor then
        // says it instead of offering it again, while the helpers are the
        // files they were.
        crate::onboard::id_map_attempt::record(
            &host,
            &install,
            crate::onboard::id_map_attempt::Outcome::DidNotFix,
            "",
        );
        let again = id_map_tools(&host, Path::new("/"), &os).await;
        println!("after a recorded reinstall: {:#?}", again.reinstall_keeps);
        assert!(
            again
                .reinstall_keeps
                .iter()
                .any(|keep| keep.starts_with(&format!("`{install}` ran on "))),
            "{again:#?}"
        );
    }

    fn linux(release: &str, wsl: Option<detect::Wsl>) -> Os {
        Os::Linux {
            release: detect::OsRelease::parse(release),
            wsl,
            systemd: true,
        }
    }

    /// Doctor names a platform the release gate does not verify, with what
    /// matters there; the verified ones are OK.
    #[test]
    fn an_unverified_platform_is_named_at_run_time() {
        let lines =
            |os: &Os, selinux: bool, memory_gb: u64, local: bool| -> Vec<(Status, String)> {
                platform_items(os, selinux, Some(memory_gb << 30), local)
                    .into_iter()
                    .map(|item| (item.status, item.step()))
                    .collect()
            };
        let apple = Os::MacOs {
            version: Some("26.6".into()),
            apple_silicon: true,
            chip: None,
        };
        assert_eq!(
            lines(&apple, false, 48, true),
            [(
                Status::Ok,
                "Platform: macOS on Apple silicon, as the release gate verifies".to_string()
            )]
        );
        let intel = Os::MacOs {
            version: None,
            apple_silicon: false,
            chip: None,
        };
        assert_eq!(lines(&intel, false, 64, true)[0].0, Status::Warn);
        assert!(lines(&intel, false, 64, true)[0]
            .1
            .contains("Intel Macs yet"));
        // Local models below 48 GB, said only when the setup runs them.
        let small = lines(&apple, false, 32, true);
        assert_eq!(
            small[1],
            (
                Status::Warn,
                "Platform: 32 GB of memory: local models have not been verified below 48 GB"
                    .to_string()
            )
        );
        assert_eq!(lines(&apple, false, 32, false).len(), 1);
        let ubuntu = linux(
            "ID=ubuntu\nVERSION_ID=\"24.04\"\nPRETTY_NAME=\"Ubuntu 24.04.3 LTS\"\n",
            None,
        );
        assert_eq!(lines(&ubuntu, false, 48, false)[0].0, Status::Ok);
        let debian = linux(
            "ID=debian\nVERSION_ID=\"12\"\nPRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\n",
            None,
        );
        assert_eq!(
            lines(&debian, false, 48, false),
            [(
                Status::Warn,
                "Platform: Debian GNU/Linux 12 (bookworm): Axocoatl has not been verified on it \
                 yet (Ubuntu 24.04 is)"
                    .to_string()
            )]
        );
        let fedora = linux(
            "ID=fedora\nVERSION_ID=42\nPRETTY_NAME=\"Fedora Linux 42\"\n",
            None,
        );
        let fedora = lines(&fedora, true, 48, false);
        assert_eq!(fedora.len(), 2);
        assert!(fedora[1].1.contains("SELinux is enforcing"), "{fedora:?}");
        let wsl = linux("ID=ubuntu\nVERSION_ID=\"24.04\"\n", Some(detect::Wsl::Two));
        let wsl = lines(&wsl, false, 32, false);
        assert_eq!(wsl[0].0, Status::Warn);
        assert!(wsl[0].1.contains("WSL2 on Windows"), "{wsl:?}");
        assert!(wsl[0].1.contains("not under /mnt/c"), "{wsl:?}");
    }

    /// A login without a systemd user session (`su -`, `sudo -iu`) cannot run
    /// the Always-On Service, and an installed service stops at logout
    /// without linger: each says how to fix it, naming the user.
    #[test]
    fn the_systemd_user_session_and_linger_are_checked() {
        let none = detect::UserSystemd {
            reachable: false,
            state: "Failed to connect to bus: No medium found".into(),
            linger: None,
        };
        let item = user_systemd_item(&none, "alex", false);
        assert_eq!(item.status, Status::Warn);
        assert!(
            item.detail.contains("no systemd user session for alex")
                && item
                    .detail
                    .contains("Failed to connect to bus: No medium found"),
            "{}",
            item.detail
        );
        let fix = item.fix.unwrap();
        assert!(fix.contains("`ssh alex@localhost`"), "{fix}");
        assert!(fix.contains("`sudo loginctl enable-linger alex`"), "{fix}");
        crate::paste_safe::assert_paste_safe(&fix);
        let session = |linger| detect::UserSystemd {
            reachable: true,
            state: "running".into(),
            linger,
        };
        let off = user_systemd_item(&session(Some(false)), "alex", true);
        assert_eq!(off.status, Status::Warn);
        assert_eq!(
            off.fix.as_deref(),
            Some("Keep it running: `loginctl enable-linger alex`")
        );
        assert_eq!(
            user_systemd_item(&session(Some(false)), "alex", false).status,
            Status::Ok
        );
        assert!(user_systemd_item(&session(Some(true)), "alex", true)
            .detail
            .contains("linger is on"));
        // A user name a shell would split is quoted.
        let odd = user_systemd_item(&none, "a b", false).fix.unwrap();
        assert!(odd.contains("enable-linger 'a b'"), "{odd}");
    }

    /// git is checked: missing on a Mac, it is the Command Line Tools.
    #[test]
    fn git_and_the_command_line_tools_are_checked() {
        let mac = Os::MacOs {
            version: None,
            apple_silicon: true,
            chip: None,
        };
        assert_eq!(git_item(Some("2.39.5"), &mac).status, Status::Ok);
        let missing = git_item(None, &mac);
        assert_eq!(missing.status, Status::Fail);
        assert!(missing
            .fix
            .as_deref()
            .unwrap()
            .contains("`xcode-select --install`"));
        let ubuntu = linux("ID=ubuntu\nVERSION_ID=\"24.04\"\n", None);
        assert_eq!(
            git_item(None, &ubuntu).fix.as_deref(),
            Some("Install it: `sudo apt-get install -y git`")
        );
        let nixos = linux("ID=nixos\n", None);
        assert!(git_item(None, &nixos)
            .fix
            .unwrap()
            .contains("your distribution's package manager"));
    }

    /// `hooks:` is parsed and checked but never run: doctor says so.
    #[test]
    fn configured_hooks_are_said_to_not_run() {
        let text = "agents: []\nhooks:\n  - name: audit\n    type: http\n    url: \
                    \"https://example.test/hook\"\n    phase: [pre_tool]\n";
        let config = axocoatl_config::parse_config(text, Path::new("c.yaml")).unwrap();
        let item = hooks_item(&config, Path::new("/home/alex/c.yaml")).unwrap();
        assert_eq!(item.status, Status::Warn);
        assert_eq!(
            item.detail,
            format!(
                "hooks: 1 entry configured, which Axocoatl {} checks but does not run",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert!(item
            .fix
            .unwrap()
            .starts_with("Remove the hooks: section from /home/alex/c.yaml"));
        let none = axocoatl_config::parse_config("agents: []\n", Path::new("c.yaml")).unwrap();
        assert!(hooks_item(&none, Path::new("c.yaml")).is_none());
    }

    /// A proxy variable is named, never its value (it may hold a password),
    /// and doctor says a Session's network does not use it.
    #[test]
    fn a_proxy_in_the_environment_is_said_to_be_unused() {
        let set = |name: &str| {
            matches!(name, "HTTPS_PROXY" | "http_proxy")
                .then(|| "http://user:secret@proxy.example:3128".to_string())
        };
        let names = proxy_variables(set);
        assert_eq!(names, ["HTTPS_PROXY", "http_proxy"]);
        let item = proxy_item(&names).unwrap();
        assert_eq!(item.status, Status::Warn);
        assert!(item.detail.starts_with(
            "HTTPS_PROXY and http_proxy are set, but Axocoatl does not support a proxy for \
             Sessions and runs: a Session's network"
        ));
        assert!(!item.step().contains("secret"), "{}", item.step());
        assert!(proxy_item(&proxy_variables(|_| Some("  ".to_string()))).is_none());
        assert!(proxy_item(&[]).is_none());
    }

    /// An Ollama 0.40.6 whose `qwen3-coder:30b` is `loaded` (in `/api/ps`)
    /// or not; loading or probing it answers, and is counted.
    async fn ollama_holding(loaded: bool) -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let model = "qwen3-coder:30b";
        let digest = "0".repeat(64);
        let ps = if loaded {
            serde_json::json!({"models": [{"name": model, "model": model, "digest": digest,
                "details": {"format": "gguf"}, "context_length": 32768, "runner": "llamacpp",
                "size": 20_000_000_000u64, "size_vram": 20_000_000_000u64}]})
        } else {
            serde_json::json!({"models": []})
        };
        for (route, body) in [
            ("/api/version", serde_json::json!({"version": "0.40.6"})),
            (
                "/api/status",
                serde_json::json!({"cloud": {"disabled": true, "source": "env"}}),
            ),
            (
                "/api/tags",
                serde_json::json!({"models": [{"name": model, "model": model,
                    "size": 18_556_700_761u64, "digest": digest,
                    "details": {"format": "gguf", "parameter_size": "30.5B"}}]}),
            ),
            ("/api/ps", ps),
        ] {
            Mock::given(method("GET"))
                .and(path(route))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
        }
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "details": {"format": "gguf"}, "capabilities": ["completion", "tools"]})))
            .mount(&server)
            .await;
        // Loading and probing: an answer that fails the observation, so a
        // test sees the load was tried.
        for route in ["/api/generate", "/api/chat"] {
            Mock::given(method("POST"))
                .and(path(route))
                .respond_with(ResponseTemplate::new(500))
                .named(route)
                .mount(&server)
                .await;
        }
        server
    }

    fn test_env(shims: &crate::onboard::host::testing::Shims) -> Environment {
        let home = shims.home.path();
        Environment {
            os: Os::MacOs {
                version: None,
                apple_silicon: true,
                chip: None,
            },
            user: "alex".into(),
            memory_bytes: Some(48 << 30),
            cpus: 18,
            home: home.to_path_buf(),
            xdg_config_home: None,
            host: shims.host(true),
            load_models: true,
            contexts_read: BTreeMap::new(),
            socket_path: home.join("no-daemon.sock"),
            podman: "podman".into(),
            podman_connection: ConnectionOverride::None,
            id_root: shims.root.path().to_path_buf(),
            data_dir: Some(home.join("data")),
            landlock_abi: None,
            user_systemd: None,
            selinux_enforcing: false,
            git: Some("2.39.5".into()),
            proxy_variables: Vec::new(),
            on_load: None,
            default_config: home.join("config.yaml"),
        }
    }

    async fn model_lines(
        server: &wiremock::MockServer,
        env: &Environment,
        daemon_running: bool,
    ) -> Vec<Item> {
        let wanted = BTreeMap::from([("qwen3-coder:30b".to_string(), true)]);
        let host = OllamaHost {
            service: detect::OllamaService::Manual,
            macos: true,
            brew_env_file: None,
            cli_version: None,
        };
        let mut items = Vec::new();
        let mut states = BTreeMap::new();
        assert!(
            check_ollama(
                &server.uri(),
                &host,
                &wanted,
                &BTreeMap::new(),
                env,
                std::time::Duration::from_secs(120),
                daemon_running,
                &mut items,
                &mut states,
            )
            .await
        );
        items
    }

    fn loads(server_requests: &[wiremock::Request]) -> usize {
        server_requests
            .iter()
            .filter(|request| ["/api/generate", "/api/chat"].contains(&request.url.path()))
            .count()
    }

    /// While a daemon runs, a model Ollama already holds may be serving a
    /// run: doctor reads its context from `/api/ps` and neither loads nor
    /// probes it. Without a daemon, or for a model not loaded, it loads it
    /// to read its context, saying so as the caller asks (onboarding says
    /// it as its own line).
    #[tokio::test]
    async fn a_model_held_while_a_daemon_runs_is_not_loaded_again() {
        let shims = crate::onboard::host::testing::Shims::new();
        let env = test_env(&shims);
        let held = ollama_holding(true).await;
        let items = model_lines(&held, &env, true).await;
        let line = items.last().unwrap();
        assert_eq!(line.status, Status::Ok, "{line:?}");
        assert!(
            line.detail.contains(
                "32768-token context, read from Ollama without loading or probing it again (a \
                 daemon is running and may be using it"
            ),
            "{}",
            line.detail
        );
        assert_eq!(loads(&held.received_requests().await.unwrap()), 0);

        // No daemon: loaded and probed (this server's load fails, so the
        // line fails), and the caller is told before the load.
        let said = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let env = Environment {
            on_load: Some({
                let said = said.clone();
                std::sync::Arc::new(move |model: &str| said.lock().unwrap().push(model.into()))
            }),
            ..test_env(&shims)
        };
        let items = model_lines(&held, &env, false).await;
        assert_eq!(items.last().unwrap().status, Status::Fail);
        assert!(loads(&held.received_requests().await.unwrap()) > 0);
        assert_eq!(*said.lock().unwrap(), ["qwen3-coder:30b"]);

        // A daemon runs, but the model is not loaded: it is loaded.
        let idle = ollama_holding(false).await;
        model_lines(&idle, &env, true).await;
        assert!(loads(&idle.received_requests().await.unwrap()) > 0);
    }
}
