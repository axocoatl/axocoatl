//! What `axocoatl doctor` and `axocoatl onboard` say about Podman: one read
//! of the client, the runtime (`podman info --format json`) and, on macOS,
//! the machine (`podman machine list`, `inspect` and `info`), and the
//! findings both commands print from it, each with its fix.
//!
//! Every built-in loadout runs hardened, which needs rootless Podman with
//! `--userns=keep-id:uid=…,gid=…` (Podman 4.3 or later) and, rootless on
//! Linux, a range of subordinate user and group ids. A missing Podman, a
//! missing machine, a rootful runtime, a Podman older than 4.3 and missing id
//! ranges are failures; a stopped machine is not (Sessions and runs start
//! it); an untested Podman version, machine provider or a small machine are
//! warnings.

use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

use crate::podman::{
    gib_text, preflight_through, ConnectionOverride, MachineSpec, PodmanReadiness,
    MACHINE_PROVIDER, MACHINE_START_COMMAND, MIN_MACHINE_MEMORY_MIB,
};
use crate::session_sandbox::SessionSandbox;

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// The oldest Podman Axocoatl runs work with: `keep-id` with `uid=` and
/// `gid=` arrived in Podman 4.3.
pub const MIN_PODMAN_VERSION: (u64, u64) = (4, 3);
/// The Podman Axocoatl is tested with on Linux (Ubuntu 24.04's).
pub const TESTED_LINUX_PODMAN: (u64, u64) = (4, 9);
/// The Podman minor releases Axocoatl is tested with on macOS.
pub const TESTED_MACOS_PODMAN: &[(u64, u64)] = &[(5, 8)];

/// What `podman info --format json` said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeInfo {
    /// The Podman that runs containers (inside the machine on macOS).
    pub server_version: Option<String>,
    pub rootless: Option<bool>,
    pub selinux: Option<bool>,
    pub kernel: Option<String>,
    /// `fedora 44`, `ubuntu 24.04`.
    pub distribution: Option<String>,
    /// How many subordinate ranges map user and group ids (`None`: not
    /// reported, as for a rootful runtime). One means only the user's own id.
    pub uid_ranges: Option<usize>,
    pub gid_ranges: Option<usize>,
    /// The runtime is reached remotely (a machine, `CONTAINER_HOST`).
    pub remote: Option<bool>,
}

/// One machine `podman machine list` and `inspect` report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MachineInfo {
    pub name: String,
    pub default: bool,
    pub running: bool,
    pub vm_type: Option<String>,
    pub cpus: Option<u64>,
    pub memory_mib: Option<u64>,
    pub rootful: Option<bool>,
}

impl MachineInfo {
    /// `applehv, rootless, 6 CPUs / 8 GiB`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(vm_type) = &self.vm_type {
            parts.push(vm_type.clone());
        }
        match self.rootful {
            Some(true) => parts.push("rootful".to_string()),
            Some(false) => parts.push("rootless".to_string()),
            None => {}
        }
        match (self.cpus, self.memory_mib) {
            (Some(cpus), Some(memory)) => parts.push(format!("{cpus} CPUs / {}", gib_text(memory))),
            (None, Some(memory)) => parts.push(gib_text(memory)),
            (Some(cpus), None) => parts.push(format!("{cpus} CPUs")),
            (None, None) => {}
        }
        parts.join(", ")
    }
}

/// Everything one read of Podman found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodmanReport {
    pub readiness: PodmanReadiness,
    /// `podman --version` of the client on this computer.
    pub client_version: Option<String>,
    /// `None` unless the runtime answered.
    pub runtime: Option<RuntimeInfo>,
    /// macOS and Windows: the machines, the default one first.
    pub machines: Vec<MachineInfo>,
    /// macOS and Windows: the provider a new machine gets
    /// (`podman machine info`), such as `applehv` or `libkrun`.
    pub default_provider: Option<String>,
    /// The connection `CONTAINER_HOST` or `CONTAINER_CONNECTION` selects.
    pub connection: ConnectionOverride,
}

impl PodmanReport {
    /// Podman is not installed: nothing else is known.
    pub fn not_installed() -> Self {
        Self {
            readiness: PodmanReadiness::NotInstalled,
            client_version: None,
            runtime: None,
            machines: Vec::new(),
            default_provider: None,
            connection: ConnectionOverride::default(),
        }
    }

    /// The machine Podman uses: the one `CONTAINER_CONNECTION` names, else
    /// the default one, else the running one, else the first.
    pub fn machine(&self) -> Option<&MachineInfo> {
        if let ConnectionOverride::Connection(name) = &self.connection {
            if let Some(machine) = self.machines.iter().find(|machine| machine.name == *name) {
                return Some(machine);
            }
        }
        self.machines
            .iter()
            .find(|machine| machine.default)
            .or_else(|| self.machines.iter().find(|machine| machine.running))
            .or_else(|| self.machines.first())
    }

    /// The machine Podman reaches, when it reaches one: `None` when
    /// `CONTAINER_HOST` is set, or `CONTAINER_CONNECTION` names a connection
    /// that is not a listed machine. Commands that change a machine
    /// (`podman machine set`, `start`, `stop`) are given this one's name.
    pub fn machine_in_use(&self) -> Option<&MachineInfo> {
        match &self.connection {
            ConnectionOverride::Host(_) => None,
            ConnectionOverride::Connection(name) => {
                self.machines.iter().find(|machine| machine.name == *name)
            }
            ConnectionOverride::None => self.machine(),
        }
    }

    /// Podman runs containers rootless, as far as the runtime or the
    /// machine said.
    pub fn rootless(&self) -> Option<bool> {
        self.runtime
            .as_ref()
            .and_then(|runtime| runtime.rootless)
            .or_else(|| {
                self.machine()
                    .and_then(|m| m.rootful)
                    .map(|rootful| !rootful)
            })
    }

    /// The client version as numbers.
    pub fn client_numbers(&self) -> Option<(u64, u64, u64)> {
        self.client_version.as_deref().and_then(version_numbers)
    }
}

/// Read Podman with the client on PATH.
pub async fn inspect() -> PodmanReport {
    inspect_with("podman").await
}

/// Read Podman with the client at `program`. Only read-only commands run:
/// `--version`, `info`, `machine list`, `machine inspect` and `machine info`.
pub async fn inspect_with(program: &str) -> PodmanReport {
    inspect_through(program, ConnectionOverride::from_env()).await
}

/// [`inspect_with`] given the connection the environment selects.
pub async fn inspect_through(program: &str, connection: ConnectionOverride) -> PodmanReport {
    let readiness = preflight_through(program, &connection).await;
    let mut report = PodmanReport {
        readiness,
        connection,
        ..PodmanReport::not_installed()
    };
    if report.readiness == PodmanReadiness::NotInstalled {
        return report;
    }
    report.client_version = output(program, &["--version"])
        .await
        .and_then(|text| parse_client_version(&text));
    if report.readiness == PodmanReadiness::Ready {
        report.runtime = output(program, &["info", "--format", "json"])
            .await
            .and_then(|text| parse_runtime_info(text.as_bytes()).ok());
    }
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        if let Some(text) = output(program, &["machine", "list", "--format", "json"]).await {
            report.machines = parse_machine_list(text.as_bytes()).unwrap_or_default();
        }
        let names: Vec<String> = report
            .machine()
            .map(|m| m.name.clone())
            .into_iter()
            .collect();
        for name in names {
            if let Some(text) = output(program, &["machine", "inspect", &name]).await {
                merge_machine_inspect(&mut report.machines, text.as_bytes());
            }
        }
        report.default_provider = output(program, &["machine", "info", "--format", "json"])
            .await
            .and_then(|text| parse_default_provider(text.as_bytes()));
    }
    report
}

async fn output(program: &str, args: &[&str]) -> Option<String> {
    let mut command = Command::new(program);
    command.args(args);
    let output = SessionSandbox::run_bounded_command(command, PROBE_TIMEOUT)
        .await
        .ok()?;
    (!output.timed_out && output.status.success())
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `podman version 5.8.2` → `5.8.2`.
pub fn parse_client_version(text: &str) -> Option<String> {
    let version = text.split_whitespace().last()?;
    version_numbers(version).map(|_| version.to_string())
}

/// `4.9.3`, `5.8.2-dev` → `(4, 9, 3)`, `(5, 8, 2)`.
pub fn version_numbers(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim().trim_start_matches('v').split('.');
    let mut number = || -> Option<u64> {
        let part = parts.next()?;
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    };
    let major = number()?;
    let minor = number()?;
    let patch = number().unwrap_or(0);
    Some((major, minor, patch))
}

/// Read `podman info --format json`.
pub fn parse_runtime_info(stdout: &[u8]) -> Result<RuntimeInfo, String> {
    let info: Value = serde_json::from_slice(stdout)
        .map_err(|error| format!("podman info returned invalid JSON: {error}"))?;
    if !info.is_object() {
        return Err("podman info did not return a JSON object".to_string());
    }
    let text = |pointer: &str| {
        info.pointer(pointer)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let ranges = |pointer: &str| match info.pointer(pointer) {
        Some(Value::Array(entries)) => Some(entries.len()),
        Some(Value::Null) => Some(0),
        _ => None,
    };
    let distribution = match (
        text("/host/distribution/distribution"),
        text("/host/distribution/version"),
    ) {
        (Some(name), Some(version)) => Some(format!("{name} {version}")),
        (Some(name), None) => Some(name),
        _ => None,
    };
    Ok(RuntimeInfo {
        server_version: text("/version/Version"),
        rootless: info
            .pointer("/host/security/rootless")
            .and_then(Value::as_bool),
        selinux: info
            .pointer("/host/security/selinuxEnabled")
            .and_then(Value::as_bool),
        kernel: text("/host/kernel"),
        distribution,
        uid_ranges: ranges("/host/idMappings/uidmap"),
        gid_ranges: ranges("/host/idMappings/gidmap"),
        remote: info
            .pointer("/host/serviceIsRemote")
            .and_then(Value::as_bool),
    })
}

/// A byte count Podman reports as a number or a string of digits, in MiB.
fn mib(value: &Value) -> Option<u64> {
    let bytes = match value {
        Value::Number(number) => number.as_u64()?,
        Value::String(text) => text.trim().parse().ok()?,
        _ => return None,
    };
    Some(bytes / (1024 * 1024))
}

/// Read `podman machine list --format json`.
pub fn parse_machine_list(stdout: &[u8]) -> Result<Vec<MachineInfo>, String> {
    let machines: Vec<Value> = serde_json::from_slice(stdout)
        .map_err(|error| format!("podman machine list returned invalid JSON: {error}"))?;
    let mut list: Vec<MachineInfo> = machines
        .iter()
        .filter_map(|machine| {
            Some(MachineInfo {
                name: machine
                    .get("Name")?
                    .as_str()?
                    .trim_end_matches('*')
                    .to_string(),
                default: machine
                    .get("Default")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                running: machine
                    .get("Running")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                vm_type: machine
                    .get("VMType")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string),
                cpus: machine.get("CPUs").and_then(Value::as_u64),
                memory_mib: machine.get("Memory").and_then(mib),
                rootful: None,
            })
        })
        .collect();
    list.sort_by_key(|machine| !machine.default);
    Ok(list)
}

/// Add what `podman machine inspect <name>` says (rootful, and the size in
/// MiB) to the listed machine of that name.
pub fn merge_machine_inspect(machines: &mut [MachineInfo], stdout: &[u8]) {
    let Ok(Value::Array(inspected)) = serde_json::from_slice::<Value>(stdout) else {
        return;
    };
    for entry in inspected {
        let Some(name) = entry.get("Name").and_then(Value::as_str) else {
            continue;
        };
        let Some(machine) = machines.iter_mut().find(|machine| machine.name == name) else {
            continue;
        };
        if let Some(rootful) = entry.get("Rootful").and_then(Value::as_bool) {
            machine.rootful = Some(rootful);
        }
        if let Some(memory) = entry.pointer("/Resources/Memory").and_then(Value::as_u64) {
            machine.memory_mib = Some(memory);
        }
        if let Some(cpus) = entry.pointer("/Resources/CPUs").and_then(Value::as_u64) {
            machine.cpus = Some(cpus);
        }
    }
}

/// `Host.VMType` of `podman machine info --format json`: the provider a new
/// machine gets.
pub fn parse_default_provider(stdout: &[u8]) -> Option<String> {
    let info: Value = serde_json::from_slice(stdout).ok()?;
    info.pointer("/Host/VMType")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// How bad a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

/// One line `doctor` and onboarding print about Podman.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    pub name: &'static str,
    pub detail: String,
    pub fix: Option<String>,
}

impl Finding {
    fn new(level: Level, name: &'static str, detail: String, fix: Option<String>) -> Self {
        Self {
            level,
            name,
            detail,
            fix,
        }
    }
}

/// What the findings need to know about this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    /// Podman runs containers in a machine (macOS).
    pub machine: bool,
    /// The user whose subordinate ids Podman maps.
    pub user: String,
    /// How to install Podman here, in words with the exact command.
    pub install_fix: String,
    /// The Linux distribution's own note for a Podman that is too old (for
    /// Ubuntu 22.04: it ships 3.4.4).
    pub too_old_note: Option<String>,
    /// The machine onboarding would create here.
    pub spec: MachineSpec,
}

/// `podman machine <verb>` for the machine `name`: the name is left out
/// for `podman-machine-default` (or `None`), which Podman uses without one.
pub fn machine_command(verb: &str, name: Option<&str>) -> String {
    match name.filter(|name| *name != "podman-machine-default") {
        Some(name) => format!("podman machine {verb} {name}"),
        None => format!("podman machine {verb}"),
    }
}

/// The command that switches the machine `name` to rootless.
pub fn rootless_switch_command(name: &str) -> String {
    format!(
        "{} && {} && {}",
        machine_command("stop", Some(name)),
        machine_command("set --rootful=false", Some(name)),
        machine_command("start", Some(name))
    )
}

/// The command that adds a range of subordinate ids for `user`.
pub fn subordinate_ids_command(user: &str) -> String {
    format!(
        "sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 {user} && podman \
         system migrate"
    )
}

/// The findings for `report` on `platform`, the main line first.
pub fn findings(report: &PodmanReport, platform: &Platform) -> Vec<Finding> {
    const NEEDED: &str = "needed for every Session and every axocoatl run";
    let version = report
        .client_version
        .clone()
        .unwrap_or_else(|| "(version unknown)".to_string());
    let machine = report.machine();
    let create = format!(
        "Run `axocoatl onboard` (it asks first), or: {} && {MACHINE_START_COMMAND}",
        platform.spec.init_command()
    );
    let mut list = Vec::new();
    match &report.readiness {
        PodmanReadiness::NotInstalled => {
            list.push(Finding::new(
                Level::Fail,
                "Podman",
                format!("Podman is not installed; it is {NEEDED}"),
                Some(platform.install_fix.clone()),
            ));
            return list;
        }
        PodmanReadiness::MachineMissing => list.push(Finding::new(
            Level::Fail,
            "Podman",
            format!("Podman {version} is installed but has no machine; it is {NEEDED}"),
            Some(create),
        )),
        PodmanReadiness::MachineStopped => list.push(Finding::new(
            Level::Ok,
            "Podman",
            format!(
                "Podman {version}, machine {} stopped ({}); it starts on first use, when a Session \
                 or an axocoatl run needs it",
                machine.map(|m| m.name.as_str()).unwrap_or("podman-machine-default"),
                machine.map(MachineInfo::describe).unwrap_or_default()
            ),
            None,
        )),
        PodmanReadiness::Unavailable(error) => list.push(Finding::new(
            Level::Fail,
            "Podman",
            format!(
                "Podman {version} does not answer: {}",
                error.lines().next().unwrap_or_default()
            ),
            Some(if platform.machine {
                format!(
                    "Check `podman machine list` and `podman system connection list`; start the \
                     machine with `{}`",
                    machine_command("start", report.machine_in_use().map(|m| m.name.as_str()))
                )
            } else {
                "Run `podman info` to see why it fails".to_string()
            }),
        )),
        PodmanReadiness::Ready => {
            let runtime = report.runtime.clone().unwrap_or_default();
            let mut parts = Vec::new();
            if let (Some(server), true) = (
                &runtime.server_version,
                runtime.server_version != report.client_version,
            ) {
                parts.push(format!("runs {server}"));
            }
            if platform.machine {
                if let Some(machine) = machine {
                    parts.push(format!("machine {}: {}", machine.name, machine.describe()));
                }
            } else if let Some(rootless) = runtime.rootless {
                parts.push(if rootless { "rootless" } else { "rootful" }.to_string());
            }
            if let Some(kernel) = &runtime.kernel {
                parts.push(format!("kernel {kernel}"));
            }
            list.push(Finding::new(
                Level::Ok,
                "Podman",
                format!(
                    "Podman {version} ready{}, {NEEDED}",
                    if parts.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", parts.join("; "))
                    }
                ),
                None,
            ));
        }
    }

    if let Some((major, minor, _)) = report.client_numbers() {
        if (major, minor) < MIN_PODMAN_VERSION {
            list.push(Finding::new(
                Level::Fail,
                "Podman version",
                format!(
                    "Podman {version} is too old: Axocoatl runs need Podman 4.3 or later (4.9 \
                     tested){}",
                    platform
                        .too_old_note
                        .as_deref()
                        .map(|note| format!(". {note}"))
                        .unwrap_or_default()
                ),
                Some(if platform.machine {
                    "Upgrade Podman: brew upgrade podman".to_string()
                } else {
                    "Install Podman 4.9 or later from your distribution (Ubuntu 24.04 or newer, \
                     Debian 13, Fedora)"
                        .to_string()
                }),
            ));
        } else if !platform.machine && (major, minor) < TESTED_LINUX_PODMAN {
            list.push(Finding::new(
                Level::Warn,
                "Podman version",
                format!("Podman {version} has not been tested with Axocoatl (4.9 is)"),
                None,
            ));
        } else if platform.machine && !TESTED_MACOS_PODMAN.contains(&(major, minor)) {
            list.push(Finding::new(
                Level::Warn,
                "Podman version",
                format!(
                    "Podman {version} has not been tested with Axocoatl on macOS yet ({} is)",
                    TESTED_MACOS_PODMAN
                        .iter()
                        .map(|(major, minor)| format!("{major}.{minor}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                None,
            ));
        }
    }

    if report.rootless() == Some(false) {
        let fix = match (platform.machine, report.machine_in_use()) {
            (true, Some(machine)) => format!(
                "{} (this also affects other tools that use this machine)",
                rootless_switch_command(&machine.name)
            ),
            (true, None) => format!(
                "Podman is reached through {}; point it at a rootless Podman machine, or unset \
                 it to use your own machine",
                report.connection.describe().unwrap_or_default()
            ),
            (false, _) => "Run Podman and Axocoatl as your normal user, not as root".to_string(),
        };
        list.push(Finding::new(
            Level::Fail,
            "Rootless",
            "Podman runs containers as root; every axocoatl run (fix, qa, audit) needs rootless \
             Podman and stops with exit 5"
                .to_string(),
            Some(fix),
        ));
    }

    if let Some(runtime) = &report.runtime {
        let ranges = [runtime.uid_ranges, runtime.gid_ranges];
        if runtime.rootless == Some(true) && ranges.iter().any(|r| matches!(r, Some(0 | 1))) {
            list.push(Finding::new(
                Level::Fail,
                "User ID ranges",
                format!(
                    "Rootless Podman has no range of subordinate user and group IDs for {}, so it \
                     cannot unpack images or start Sessions",
                    platform.user
                ),
                Some(subordinate_ids_command(&platform.user)),
            ));
        }
        if !platform.machine && runtime.selinux == Some(true) {
            list.push(Finding::new(
                Level::Warn,
                "SELinux",
                "SELinux is enabled; Sessions under SELinux enforcing have not been verified yet"
                    .to_string(),
                None,
            ));
        }
    }

    if platform.machine {
        if let Some(machine) = machine {
            if machine
                .memory_mib
                .is_some_and(|memory| memory < MIN_MACHINE_MEMORY_MIB)
            {
                list.push(Finding::new(
                    Level::Warn,
                    "Machine memory",
                    format!(
                        "machine {} has {}; Sessions take up to 2 GiB each, so it holds few",
                        machine.name,
                        gib_text(machine.memory_mib.unwrap_or_default())
                    ),
                    Some(format!(
                        "{} && {} && {}",
                        machine_command("stop", Some(&machine.name)),
                        machine_command(
                            &format!("set --memory {}", platform.spec.memory_mib),
                            Some(&machine.name)
                        ),
                        machine_command("start", Some(&machine.name)),
                    )),
                ));
            }
            if let Some(vm_type) = &machine.vm_type {
                if vm_type != MACHINE_PROVIDER {
                    list.push(Finding::new(
                        Level::Warn,
                        "Machine provider",
                        format!(
                            "machine {} uses the {vm_type} provider, which Axocoatl has not been \
                             tested with ({MACHINE_PROVIDER} is)",
                            machine.name
                        ),
                        None,
                    ));
                }
            }
        }
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::podman::machine_spec;

    /// `podman info --format json` of Podman 5.8 in a macOS machine.
    const MAC_INFO: &str = r#"{"host":{"arch":"arm64","kernel":"7.2.5-200.fc44.aarch64",
        "distribution":{"distribution":"fedora","variant":"coreos","version":"44"},
        "security":{"rootless":true,"selinuxEnabled":true},"serviceIsRemote":true,
        "idMappings":{"gidmap":[{"container_id":0,"host_id":1000,"size":1},
        {"container_id":1,"host_id":100000,"size":1000000}],
        "uidmap":[{"container_id":0,"host_id":501,"size":1},
        {"container_id":1,"host_id":100000,"size":1000000}]}},
        "version":{"APIVersion":"5.8.9","Version":"5.8.9"},
        "Client":{"Version":"5.8.2"}}"#;

    /// Podman 5.8.2's `machine list --format json`.
    const MAC_LIST: &str = r#"[{"Name":"podman-machine-default","Default":true,"Running":true,
        "VMType":"applehv","CPUs":6,"Memory":"8589934592","DiskSize":"107374182400"}]"#;

    fn platform(machine: bool) -> Platform {
        Platform {
            machine,
            user: "alex".to_string(),
            install_fix: "Run `axocoatl onboard`, or: brew install podman".to_string(),
            too_old_note: None,
            spec: machine_spec(48 << 30, 18),
        }
    }

    fn mac_report() -> PodmanReport {
        let mut machines = parse_machine_list(MAC_LIST.as_bytes()).unwrap();
        merge_machine_inspect(
            &mut machines,
            br#"[{"Name":"podman-machine-default","Rootful":false}]"#,
        );
        PodmanReport {
            readiness: PodmanReadiness::Ready,
            client_version: Some("5.8.2".to_string()),
            runtime: Some(parse_runtime_info(MAC_INFO.as_bytes()).unwrap()),
            machines,
            default_provider: Some("applehv".to_string()),
            connection: ConnectionOverride::None,
        }
    }

    #[test]
    fn info_list_and_inspect_are_read() {
        let info = parse_runtime_info(MAC_INFO.as_bytes()).unwrap();
        assert_eq!(info.server_version.as_deref(), Some("5.8.9"));
        assert_eq!(info.rootless, Some(true));
        assert_eq!(info.uid_ranges, Some(2));
        assert_eq!(info.distribution.as_deref(), Some("fedora 44"));
        let mut machines = parse_machine_list(MAC_LIST.as_bytes()).unwrap();
        assert_eq!(machines[0].memory_mib, Some(8192));
        assert_eq!(machines[0].describe(), "applehv, 6 CPUs / 8 GiB");
        merge_machine_inspect(
            &mut machines,
            br#"[{"Name":"podman-machine-default","Rootful":true,
                "Resources":{"CPUs":4,"Memory":4096}}]"#,
        );
        assert_eq!(machines[0].rootful, Some(true));
        assert_eq!(machines[0].describe(), "applehv, rootful, 4 CPUs / 4 GiB");
        // Podman 6 may report memory as a number.
        let numeric = parse_machine_list(br#"[{"Name":"m","Memory":2147483648}]"#).unwrap();
        assert_eq!(numeric[0].memory_mib, Some(2048));
        assert_eq!(
            parse_default_provider(br#"{"Host":{"VMType":"libkrun"}}"#).as_deref(),
            Some("libkrun")
        );
        assert_eq!(
            parse_client_version("podman version 6.1.4\n").as_deref(),
            Some("6.1.4")
        );
        assert_eq!(version_numbers("3.4.4"), Some((3, 4, 4)));
        assert_eq!(version_numbers("5.8.2-dev"), Some((5, 8, 2)));
        assert!(parse_runtime_info(b"[]").is_err());
    }

    #[test]
    fn a_ready_tested_mac_setup_has_one_ok_line() {
        let findings = findings(&mac_report(), &platform(true));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].level, Level::Ok);
        assert_eq!(
            findings[0].detail,
            "Podman 5.8.2 ready (runs 5.8.9; machine podman-machine-default: applehv, rootless, \
             6 CPUs / 8 GiB; kernel 7.2.5-200.fc44.aarch64), needed for every Session and every \
             axocoatl run"
        );
    }

    #[test]
    fn missing_podman_or_machine_fail_with_the_sized_create_command() {
        let mut report = mac_report();
        report.readiness = PodmanReadiness::NotInstalled;
        let list = findings(&report, &platform(true));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].level, Level::Fail);
        assert!(list[0]
            .fix
            .as_deref()
            .unwrap()
            .contains("brew install podman"));

        report.readiness = PodmanReadiness::MachineMissing;
        report.runtime = None;
        report.machines.clear();
        let list = findings(&report, &platform(true));
        assert_eq!(list[0].level, Level::Fail);
        assert!(list[0].detail.contains("has no machine"));
        assert_eq!(
            list[0].fix.as_deref(),
            Some(
                "Run `axocoatl onboard` (it asks first), or: podman machine init --cpus 6 \
                 --memory 8192 --disk-size 60 && podman machine start"
            )
        );
    }

    /// A stopped machine is not a failure: Sessions and runs start it.
    #[test]
    fn a_stopped_machine_starts_on_first_use() {
        let mut report = mac_report();
        report.readiness = PodmanReadiness::MachineStopped;
        report.runtime = None;
        report.machines[0].running = false;
        let list = findings(&report, &platform(true));
        assert_eq!(list.len(), 1, "{list:?}");
        assert_eq!(list[0].level, Level::Ok);
        assert!(list[0].detail.contains("starts on first use"), "{list:?}");
    }

    /// Podman Desktop creates rootful machines: every built-in loadout
    /// refuses them, so doctor fails and names the switch.
    #[test]
    fn a_rootful_machine_fails_with_the_switch_to_rootless() {
        let mut report = mac_report();
        report.runtime.as_mut().unwrap().rootless = Some(false);
        report.machines[0].rootful = Some(true);
        let list = findings(&report, &platform(true));
        let rootless = list.iter().find(|f| f.name == "Rootless").unwrap();
        assert_eq!(rootless.level, Level::Fail);
        assert!(rootless.detail.contains("exit 5"));
        assert_eq!(
            rootless.fix.as_deref(),
            Some(
                "podman machine stop && podman machine set --rootful=false && podman machine \
                 start (this also affects other tools that use this machine)"
            )
        );
        // A stopped rootful machine is caught from `machine inspect` alone.
        report.readiness = PodmanReadiness::MachineStopped;
        report.runtime = None;
        assert!(findings(&report, &platform(true))
            .iter()
            .any(|f| f.name == "Rootless" && f.level == Level::Fail));
    }

    /// Every machine command doctor prints names the machine in use: a
    /// machine with another name than the default, or the one
    /// `CONTAINER_CONNECTION` names. Through `CONTAINER_HOST` there is no
    /// machine to change.
    #[test]
    fn machine_commands_name_the_machine_in_use() {
        let mut report = mac_report();
        report.machines[0].name = "dev".to_string();
        report.machines[0].memory_mib = Some(2048);
        report.runtime.as_mut().unwrap().rootless = Some(false);
        let list = findings(&report, &platform(true));
        let fix = |name: &str| {
            list.iter()
                .find(|f| f.name == name)
                .and_then(|f| f.fix.clone())
                .unwrap()
        };
        assert_eq!(
            fix("Rootless"),
            "podman machine stop dev && podman machine set --rootful=false dev && podman \
             machine start dev (this also affects other tools that use this machine)"
        );
        assert_eq!(
            fix("Machine memory"),
            "podman machine stop dev && podman machine set --memory 8192 dev && podman machine \
             start dev"
        );

        report.machines.push(MachineInfo {
            name: "axocoatl".to_string(),
            ..MachineInfo::default()
        });
        report.connection = ConnectionOverride::Connection("axocoatl".to_string());
        assert_eq!(report.machine().unwrap().name, "axocoatl");
        assert_eq!(report.machine_in_use().unwrap().name, "axocoatl");
        report.connection = ConnectionOverride::Connection("remote".to_string());
        assert!(report.machine_in_use().is_none());
        report.connection = ConnectionOverride::Host("unix:///tmp/podman.sock".to_string());
        assert!(report.machine_in_use().is_none());
        let list = findings(&report, &platform(true));
        let rootless = list.iter().find(|f| f.name == "Rootless").unwrap();
        assert!(
            rootless
                .fix
                .as_deref()
                .unwrap()
                .starts_with("Podman is reached through CONTAINER_HOST=unix:///tmp/podman.sock"),
            "{rootless:?}"
        );
    }

    #[test]
    fn a_small_machine_an_untested_provider_and_version_warn() {
        let mut report = mac_report();
        report.client_version = Some("6.1.4".to_string());
        report.machines[0].memory_mib = Some(2048);
        report.machines[0].vm_type = Some("libkrun".to_string());
        let list = findings(&report, &platform(true));
        let levels: Vec<(&str, Level)> = list.iter().map(|f| (f.name, f.level)).collect();
        assert_eq!(
            levels,
            [
                ("Podman", Level::Ok),
                ("Podman version", Level::Warn),
                ("Machine memory", Level::Warn),
                ("Machine provider", Level::Warn),
            ]
        );
        assert!(list[1]
            .detail
            .contains("not been tested with Axocoatl on macOS yet (5.8 is)"));
        assert_eq!(
            list[2].fix.as_deref(),
            Some("podman machine stop && podman machine set --memory 8192 && podman machine start")
        );
    }

    /// Ubuntu 22.04's Podman 3.4.4 and a user without subordinate ids both
    /// fail on Linux, each with its fix.
    #[test]
    fn linux_floor_and_subordinate_ids() {
        let mut report = PodmanReport {
            readiness: PodmanReadiness::Ready,
            client_version: Some("3.4.4".to_string()),
            runtime: Some(
                parse_runtime_info(
                    br#"{"host":{"security":{"rootless":true},
                    "idMappings":{"gidmap":null,"uidmap":null}},"version":{"Version":"3.4.4"}}"#,
                )
                .unwrap(),
            ),
            machines: Vec::new(),
            default_provider: None,
            connection: ConnectionOverride::None,
        };
        let mut linux = platform(false);
        linux.too_old_note =
            Some("Ubuntu 22.04 ships Podman 3.4.4; use Ubuntu 24.04 or newer".to_string());
        let list = findings(&report, &linux);
        let version = list.iter().find(|f| f.name == "Podman version").unwrap();
        assert_eq!(version.level, Level::Fail);
        assert!(version.detail.contains("4.3 or later (4.9 tested)"));
        assert!(version.detail.contains("Ubuntu 22.04 ships Podman 3.4.4"));
        let ids = list.iter().find(|f| f.name == "User ID ranges").unwrap();
        assert_eq!(ids.level, Level::Fail);
        assert_eq!(
            ids.fix.as_deref(),
            Some(
                "sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 alex && \
                 podman system migrate"
            )
        );
        // One entry, the user's own id, is no range either.
        report.client_version = Some("4.9.3".to_string());
        report.runtime = Some(
            parse_runtime_info(
                br#"{"host":{"security":{"rootless":true},"idMappings":{
                "uidmap":[{"container_id":0,"host_id":1000,"size":1}],
                "gidmap":[{"container_id":0,"host_id":1000,"size":1}]}}}"#,
            )
            .unwrap(),
        );
        let list = findings(&report, &linux);
        assert!(list.iter().any(|f| f.name == "User ID ranges"));
        assert!(!list.iter().any(|f| f.name == "Podman version"));
        report.client_version = Some("4.6.2".to_string());
        assert!(findings(&report, &linux)
            .iter()
            .any(|f| f.name == "Podman version" && f.level == Level::Warn));
    }

    /// With a shim Podman: a machine that is stopped, the read-only commands
    /// only, and nothing that starts or changes it.
    #[cfg(unix)]
    #[tokio::test]
    async fn inspect_runs_only_read_only_commands() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("calls.log");
        let program = directory.path().join("podman");
        std::fs::write(
            &program,
            format!(
                r#"#!/bin/sh
echo "$*" >> "{}"
case "$*" in
  "--version") echo "podman version 5.8.2" ;;
  "machine list --format json") echo '[{{"Name":"podman-machine-default","Default":true,"Running":false,"VMType":"applehv","CPUs":6,"Memory":"8589934592"}}]' ;;
  "machine inspect podman-machine-default") echo '[{{"Name":"podman-machine-default","Rootful":false,"Resources":{{"CPUs":6,"Memory":8192}}}}]' ;;
  "machine info --format json") echo '{{"Host":{{"VMType":"applehv"}}}}' ;;
  *) echo "Cannot connect to Podman" >&2; exit 125 ;;
esac
"#,
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let report = inspect_with(program.to_str().unwrap()).await;
        let calls = std::fs::read_to_string(&log).unwrap();
        for call in calls.lines() {
            assert!(
                [
                    "info --format json",
                    "--version",
                    "machine list --format json",
                    "machine inspect podman-machine-default",
                    "machine info --format json",
                ]
                .contains(&call),
                "{call}"
            );
        }
        assert_eq!(report.client_version.as_deref(), Some("5.8.2"));
        if cfg!(target_os = "macos") {
            assert_eq!(report.readiness, PodmanReadiness::MachineStopped);
            assert_eq!(report.machine().unwrap().rootful, Some(false));
            assert_eq!(report.default_provider.as_deref(), Some("applehv"));
        }
    }
}
