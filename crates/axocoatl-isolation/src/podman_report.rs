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
//!
//! On Linux, when Podman runs on this computer ([`PodmanReport::runs_here`]),
//! the id ranges are read from `/etc/subuid` and `/etc/subgid`
//! ([`read_subordinate_ids`]), not only from `podman info`: `podman info`
//! reports the user namespace Podman's pause process already holds, which
//! keeps the ranges it was started with after a range is removed, while
//! every new `keep-id` container is mapped from the files. Without a range
//! there, crun fails with `write to /proc/sys/net/ipv4/ping_group_range:
//! Invalid argument` (the container has no group 0). The files here say
//! nothing about a Podman reached through `CONTAINER_HOST`,
//! `CONTAINER_CONNECTION` or a remote connection: for that one only its
//! `podman info` decides, and no fix runs or names a command on this
//! computer.

use std::path::Path;
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
/// The Podman minor release Axocoatl is tested with on Linux: Ubuntu
/// 24.04's 4.9, what the release gate's Linux runs and CI use. Doctor and
/// onboarding warn about any other (Fedora's 5.x and Arch's 6.x included),
/// as they do on macOS.
pub const TESTED_LINUX_PODMAN: (u64, u64) = (4, 9);
/// The Podman minor releases Axocoatl is tested with on macOS: 6.1, what
/// Homebrew installs for onboarding today and what the release gate's Mac runs
/// (`docs/RELEASE_GATE.md`). Doctor and onboarding warn about any other.
pub const TESTED_MACOS_PODMAN: &[(u64, u64)] = &[(6, 1)];

/// "6.1 is" (or "A and B are" when more than one release is listed): the
/// tested macOS releases, for a warning.
fn tested_macos_text() -> String {
    let versions: Vec<String> = TESTED_MACOS_PODMAN
        .iter()
        .map(|(major, minor)| format!("{major}.{minor}"))
        .collect();
    match versions.split_last() {
        Some((last, [])) => format!("{last} is"),
        Some((last, rest)) => format!("{} and {last} are", rest.join(", ")),
        None => String::new(),
    }
}

/// The warning for a Podman on Linux other than [`TESTED_LINUX_PODMAN`].
pub fn linux_untested(version: &str) -> String {
    let (major, minor) = TESTED_LINUX_PODMAN;
    format!("Podman {version} has not been tested with Axocoatl on Linux yet ({major}.{minor} is)")
}

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
    /// The cgroup controllers it can use for containers' resource caps
    /// (`None`: not reported).
    pub cgroup_controllers: Option<Vec<String>>,
    /// `v1` or `v2`.
    pub cgroup_version: Option<String>,
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
    /// Linux, when Podman [runs here](PodmanReport::runs_here): the user's
    /// ranges in `/etc/subuid` and `/etc/subgid`, which the caller reads
    /// with [`read_subordinate_ids`] (`None`: not read, as on macOS or for a
    /// Podman reached elsewhere, or not knowable from the files).
    pub subordinate_ids: Option<SubordinateIds>,
    /// Linux, when Podman runs here: whether `newuidmap` and `newgidmap`
    /// can map a range (`None`: not checked).
    pub id_map_tools: Option<IdMapTools>,
    /// This computer runs Podman's containers in a machine (macOS,
    /// Windows), so unsetting `CONTAINER_HOST` or `CONTAINER_CONNECTION`
    /// selects the default machine, not a Podman on this computer. Set from
    /// the build target; onboarding and doctor set it from the platform
    /// they detected.
    pub machine_platform: bool,
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
            subordinate_ids: None,
            id_map_tools: None,
            machine_platform: cfg!(any(target_os = "macos", target_os = "windows")),
        }
    }

    /// Podman runs containers on this computer, so this computer's
    /// `/etc/subuid`, `/etc/subgid`, `newuidmap` and `newgidmap` decide its
    /// ranges, and `podman system migrate` restarts it: neither
    /// `CONTAINER_HOST` nor `CONTAINER_CONNECTION` is set, and `podman info`
    /// does not say the service is remote (a machine, `podman-remote`, a
    /// default remote connection). Any override counts as elsewhere, even a
    /// socket path on this computer: it may be another user's Podman, or,
    /// inside a toolbox or distrobox container, the host's, whose files are
    /// not the ones here; and Podman refuses `system migrate` through it.
    pub fn runs_here(&self) -> bool {
        self.connection == ConnectionOverride::None
            && self.runtime.as_ref().and_then(|runtime| runtime.remote) != Some(true)
    }

    /// The range the `usermod` fix adds: one no line of `/etc/subuid` or
    /// `/etc/subgid` uses, else [`DEFAULT_SUBORDINATE_RANGE`] when the files
    /// were not read. `None` when the files leave no such range from
    /// 100000 up to the last id.
    pub fn free_subordinate_range(&self) -> Option<(u64, u64)> {
        match &self.subordinate_ids {
            Some(ids) => ids.free,
            None => Some(DEFAULT_SUBORDINATE_RANGE),
        }
    }

    /// Podman runs here, on Linux (not in a machine, as on macOS), and
    /// `podman info` failed because `newuidmap` or `newgidmap` cannot map
    /// the user's range ([`id_map_tools_failure`]): what the error says
    /// about them.
    fn id_map_failure(&self) -> Option<IdMapTools> {
        match &self.readiness {
            PodmanReadiness::Unavailable(error) if self.runs_here() && !self.machine_platform => {
                id_map_tools_failure(error)
            }
            _ => None,
        }
    }

    /// `podman info` maps only the user's own id (or nothing) for a
    /// rootless runtime: no range is in use.
    fn single_mapping(&self) -> bool {
        self.runtime.as_ref().is_some_and(|runtime| {
            runtime.rootless == Some(true)
                && [runtime.uid_ranges, runtime.gid_ranges]
                    .iter()
                    .any(|ranges| matches!(ranges, Some(0 | 1)))
        })
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
    /// `CONTAINER_HOST` selects the connection (`CONTAINER_CONNECTION`,
    /// which wins over it, is not set), or `CONTAINER_CONNECTION` names a
    /// connection that is not a listed machine. Commands that change a machine
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
        cgroup_controllers: match info.pointer("/host/cgroupControllers") {
            Some(Value::Array(controllers)) => Some(
                controllers
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
            ),
            Some(Value::Null) => Some(Vec::new()),
            _ => None,
        },
        cgroup_version: text("/host/cgroupVersion"),
    })
}

/// How to delegate the cgroup controllers rootless Podman needs for a
/// container's caps to every user's systemd manager (Podman's own advice).
pub const DELEGATE_CONTROLLERS_FIX: &str = "sudo mkdir -p /etc/systemd/system/user@.service.d && \
     printf '[Service]\\nDelegate=cpu cpuset io memory pids\\n' | sudo tee \
     /etc/systemd/system/user@.service.d/delegate.conf && sudo systemctl daemon-reload, then log \
     out and back in";

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

/// How many subordinate user ids, and as many group ids, a rootless user
/// needs: what `useradd` gives (`SUB_UID_COUNT`), and what images need for
/// `nobody` (65534) and Sessions' workload users.
pub const MIN_SUBORDINATE_IDS: u64 = 65_536;

/// The range the `usermod` fix adds when the files were not read: shadow's
/// first default range.
pub const DEFAULT_SUBORDINATE_RANGE: (u64, u64) = (100_000, 165_535);

/// What `/etc/subuid` and `/etc/subgid` give the user Podman runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubordinateIds {
    /// Subordinate user ids in the user's lines of `/etc/subuid`.
    pub uids: u64,
    /// Subordinate group ids in the user's lines of `/etc/subgid`.
    pub gids: u64,
    /// `(first, last)`: [`MIN_SUBORDINATE_IDS`] ids from 100000 on that no
    /// line of either file uses, for the `usermod` fix; `None` when the
    /// lines leave no such range below 4294967295 (which is no id).
    pub free: Option<(u64, u64)>,
}

impl SubordinateIds {
    /// Enough of both for rootless Podman and Sessions.
    pub fn enough(&self) -> bool {
        self.uids_enough() && self.gids_enough()
    }

    fn uids_enough(&self) -> bool {
        self.uids >= MIN_SUBORDINATE_IDS
    }

    fn gids_enough(&self) -> bool {
        self.gids >= MIN_SUBORDINATE_IDS
    }

    /// What the files give `user` that is too little: no range in either,
    /// or the count in the file (or both files) that is short.
    fn describe(&self, user: &str) -> String {
        let count = |count: u64, kind: &str| {
            if count == 0 {
                format!("no subordinate {kind} IDs")
            } else {
                format!("{count} subordinate {kind} IDs")
            }
        };
        match (self.uids_enough(), self.gids_enough()) {
            (false, false) if self.uids == 0 && self.gids == 0 => format!(
                "{user} has no range of subordinate user and group IDs in /etc/subuid and \
                 /etc/subgid"
            ),
            (false, false) => format!(
                "{user} has {} in /etc/subuid and {} in /etc/subgid, and needs at least \
                 {MIN_SUBORDINATE_IDS} of each",
                count(self.uids, "user"),
                count(self.gids, "group")
            ),
            (false, true) => format!(
                "{user} has {} in /etc/subuid and needs at least {MIN_SUBORDINATE_IDS} (its {} \
                 group IDs in /etc/subgid are enough)",
                count(self.uids, "user"),
                self.gids
            ),
            (true, false) => format!(
                "{user} has {} in /etc/subgid and needs at least {MIN_SUBORDINATE_IDS} (its {} \
                 user IDs in /etc/subuid are enough)",
                count(self.gids, "group"),
                self.uids
            ),
            (true, true) => format!(
                "{user} has {} subordinate user IDs and {} group IDs",
                self.uids, self.gids
            ),
        }
    }
}

/// The `owner:first:count` lines of an `/etc/subuid` or `/etc/subgid`.
fn subordinate_lines(text: &str) -> impl Iterator<Item = (&str, u64, u64)> {
    text.lines().filter_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let mut fields = line.split(':');
        let owner = fields.next()?.trim();
        let first = fields.next()?.trim().parse().ok()?;
        let count = fields.next()?.trim().parse().ok()?;
        Some((owner, first, count))
    })
}

/// The first [`MIN_SUBORDINATE_IDS`] ids from 100000 on that overlap none
/// of `used` (`[first, end)` each), as `(first, last)`. `None` when the
/// first gap that large would end at or past 4294967295: ids are 32-bit,
/// and that one is no id. (Below 100000 are the computer's own users.)
fn free_subordinate_range(used: &[(u64, u64)]) -> Option<(u64, u64)> {
    let mut first = DEFAULT_SUBORDINATE_RANGE.0;
    loop {
        let end = first.saturating_add(MIN_SUBORDINATE_IDS);
        match used
            .iter()
            .filter(|(start, stop)| *start < end && *stop > first)
            .map(|(_, stop)| *stop)
            .max()
        {
            Some(stop) => first = stop,
            None => break,
        }
    }
    let last = first.saturating_add(MIN_SUBORDINATE_IDS - 1);
    (last < u64::from(u32::MAX)).then_some((first, last))
}

/// What the text of `/etc/subuid` and `/etc/subgid` gives `user` (whose
/// lines name it or its `uid`, as shadow and Podman match them).
pub fn parse_subordinate_ids(
    subuid: &str,
    subgid: &str,
    user: &str,
    uid: Option<u32>,
) -> SubordinateIds {
    let uid = uid.map(|uid| uid.to_string());
    let owns = |owner: &str| owner == user || uid.as_deref() == Some(owner);
    let total = |text: &str| {
        subordinate_lines(text)
            .filter(|(owner, _, _)| owns(owner))
            .fold(0u64, |sum, (_, _, count)| sum.saturating_add(count))
    };
    let used: Vec<(u64, u64)> = subordinate_lines(subuid)
        .chain(subordinate_lines(subgid))
        .map(|(_, first, count)| (first, first.saturating_add(count)))
        .collect();
    SubordinateIds {
        uids: total(subuid),
        gids: total(subgid),
        free: free_subordinate_range(&used),
    }
}

/// Read `etc/subuid` and `etc/subgid` under `root` (`/`, or a test's
/// directory) for `user` with `uid`. A missing file holds no range, as
/// Podman reads it. `None` when the files cannot tell: for root (rootful
/// Podman needs no range), an unknown user, an unreadable file, or ranges
/// that `etc/nsswitch.conf` takes from another source than the files
/// (`subid: sss`).
pub fn read_subordinate_ids(root: &Path, user: &str, uid: Option<u32>) -> Option<SubordinateIds> {
    if uid == Some(0) || user.is_empty() || user == "root" || user.starts_with('$') {
        return None;
    }
    let nsswitch = std::fs::read_to_string(root.join("etc/nsswitch.conf")).unwrap_or_default();
    let other_source = nsswitch.lines().any(|line| {
        let line = line.split('#').next().unwrap_or_default().trim();
        line.strip_prefix("subid:")
            .is_some_and(|sources| sources.trim() != "files")
    });
    if other_source {
        return None;
    }
    let read = |name: &str| match std::fs::read_to_string(root.join(name)) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
        Err(_) => None,
    };
    let subuid = read("etc/subuid")?;
    let subgid = read("etc/subgid")?;
    Some(parse_subordinate_ids(&subuid, &subgid, user, uid))
}

/// Linux: `newuidmap` and `newgidmap`, the setuid (or file-capability)
/// helpers rootless Podman maps a range of subordinate ids with. Podman
/// looks them up on `PATH` only (Go's `exec.LookPath`). Without them it
/// cannot map the range `/etc/subuid` and `/etc/subgid` give the user: a
/// pause process started before the range was added keeps mapping only the
/// user's own id, and a new one fails (`podman info`: `command required for
/// rootless mode with multiple IDs: exec: "newuidmap": executable file not
/// found in $PATH`), so `podman system migrate` alone never fixes it.
/// Debian and Ubuntu ship them in `uidmap`, which Podman only recommends;
/// Fedora and RHEL in `shadow-utils`; Arch in `shadow`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdMapTools {
    /// The helpers not found on `PATH`.
    pub missing: Vec<String>,
    /// The paths of helpers found that are neither setuid nor given file
    /// capabilities, so they cannot map other ids.
    pub unprivileged: Vec<String>,
    /// Of [`IdMapTools::missing`], the ones found in a system directory that
    /// is not on `PATH` (`/usr/bin/newuidmap`): there, but Podman does not
    /// find them, and no install changes that.
    pub off_path: Vec<String>,
    /// The package that ships them here (`uidmap`), when the caller knows
    /// the distribution's package.
    pub package: Option<String>,
    /// Whether [`IdMapTools::package`] is installed (`None`: not known).
    pub package_installed: Option<bool>,
    /// Of [`IdMapTools::unprivileged`], the ones the package does not own
    /// (a copy elsewhere on `PATH`, before the package's, or one no
    /// package installed): no reinstall changes them.
    pub foreign: Vec<String>,
    /// Why reinstalling the installed package would leave the helpers as
    /// they are, each in words with what to do first (a dpkg diversion, a
    /// stat override or a `path-exclude` for the package's file, dnf set
    /// to install files without their file capabilities, a package that
    /// does not ship a helper or ships it unable to map). Read from the
    /// package manager each time, so doctor and every onboarding run see
    /// it, not only the run whose reinstall changed nothing: no reinstall
    /// is offered while it is not empty.
    pub reinstall_keeps: Vec<String>,
    /// The command that installs the package here (`sudo apt-get install -y
    /// uidmap`), or reinstalls it when it is installed (`sudo apt-get
    /// install --reinstall -y uidmap`, `sudo dnf reinstall -y
    /// shadow-utils`), when the caller knows the distribution's package.
    pub install: Option<String>,
}

impl IdMapTools {
    /// Both helpers are there and can map.
    pub fn usable(&self) -> bool {
        self.missing.is_empty() && self.unprivileged.is_empty()
    }

    /// The missing helpers that are nowhere this computer was searched:
    /// [`IdMapTools::missing`] without the ones [`IdMapTools::off_path`]
    /// found.
    fn not_found(&self) -> Vec<String> {
        self.missing
            .iter()
            .filter(|name| {
                !self.off_path.iter().any(|path| {
                    std::path::Path::new(path)
                        .file_name()
                        .is_some_and(|file| file == name.as_str())
                })
            })
            .cloned()
            .collect()
    }

    /// The directories [`IdMapTools::off_path`] found helpers in, once each.
    fn off_path_directories(&self) -> Vec<String> {
        let mut directories: Vec<String> = Vec::new();
        for path in &self.off_path {
            let directory = std::path::Path::new(path)
                .parent()
                .map(|parent| parent.display().to_string())
                .unwrap_or_default();
            if !directories.contains(&directory) {
                directories.push(directory);
            }
        }
        directories
    }

    /// `newuidmap and newgidmap are not installed`, the ones that are there
    /// but not on `PATH`, and the ones that cannot map. A helper whose
    /// package is installed is "missing" (its file was deleted), and one
    /// whose package is not known is "not found on PATH", as Podman says.
    pub fn describe(&self) -> String {
        let verb = |count: usize| if count == 1 { "is" } else { "are" };
        let mut parts = Vec::new();
        let not_found = self.not_found();
        if !not_found.is_empty() {
            let names = not_found.join(" and ");
            let are = verb(not_found.len());
            parts.push(match (&self.package, self.package_installed) {
                (Some(package), Some(true)) => format!(
                    "{names} {are} missing, though {} package {package} is installed",
                    if not_found.len() == 1 { "its" } else { "their" }
                ),
                (_, Some(false)) => format!("{names} {are} not installed"),
                _ => format!("{names} {are} not found on PATH"),
            });
        }
        if !self.off_path.is_empty() {
            let names: Vec<String> = self
                .off_path
                .iter()
                .filter_map(|path| {
                    std::path::Path::new(path)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .collect();
            let directories = self.off_path_directories();
            parts.push(match directories.as_slice() {
                [directory] => format!(
                    "{} {} in {directory}, which is not on PATH",
                    names.join(" and "),
                    verb(names.len())
                ),
                _ => format!(
                    "{} are not on PATH ({})",
                    names.join(" and "),
                    self.off_path.join(", ")
                ),
            });
        }
        if !self.unprivileged.is_empty() {
            parts.push(format!(
                "{} {} neither setuid nor given file capabilities",
                self.unprivileged.join(" and "),
                verb(self.unprivileged.len())
            ));
        }
        parts.join(", and ")
    }

    /// The package is installed and [`IdMapTools::install`] reinstalls it,
    /// which restores a helper's file, its setuid bit or its file
    /// capabilities.
    fn reinstalls(&self) -> bool {
        self.package_installed == Some(true) && self.install.is_some()
    }

    /// The command that fixes the helpers here, when one does: it installs
    /// (or reinstalls) the package for a helper that is nowhere, and
    /// reinstalls an installed package for one that cannot map. Never for a
    /// helper that is there but not on `PATH`, which no install changes,
    /// nor a reinstall that the package manager's own records say would
    /// leave the helpers as they are ([`IdMapTools::reinstall_keeps`]).
    pub fn command(&self) -> Option<String> {
        let needed = !self.not_found().is_empty()
            || (!self.packaged_unprivileged().is_empty() && self.reinstalls());
        let kept = self.reinstalls() && !self.reinstall_keeps.is_empty();
        self.install.clone().filter(|_| needed && !kept)
    }

    /// [`IdMapTools::unprivileged`] without [`IdMapTools::foreign`]: the
    /// package's own files, which a reinstall restores.
    fn packaged_unprivileged(&self) -> Vec<String> {
        self.unprivileged
            .iter()
            .filter(|path| !self.foreign.contains(path))
            .cloned()
            .collect()
    }

    /// What only the person can do about the helpers, beside
    /// [`IdMapTools::command`]: put a directory on `PATH`, remove or fix a
    /// copy the package does not own, undo what keeps a reinstall from
    /// restoring them, reinstall a package whose state is not known, or
    /// install them where no package is known.
    fn manual(&self) -> Option<String> {
        let mut words = Vec::new();
        let directories = self.off_path_directories();
        if !directories.is_empty() {
            words.push(format!(
                "Add {} to PATH where Podman and Axocoatl run (Podman looks newuidmap and \
                 newgidmap up on PATH only)",
                directories.join(" and ")
            ));
        }
        if !self.foreign.is_empty() {
            let mut directories: Vec<&std::path::Path> = self
                .foreign
                .iter()
                .filter_map(|path| std::path::Path::new(path).parent())
                .collect();
            directories.dedup();
            let (its_directory, they_are, file, them) =
                match (self.foreign.len(), directories.len()) {
                    (1, _) => ("its directory", "it is", "file", "it"),
                    (_, 1) => ("their directory", "they are", "files", "them"),
                    _ => ("their directories", "they are", "files", "them"),
                };
            let paths = self.foreign.join(" and ");
            words.push(match (&self.package, self.package_installed) {
                (Some(package), Some(true)) => format!(
                    "Delete {paths} or take {its_directory} off PATH: {they_are} not the {file} \
                     {package} installed, so no reinstall changes {them}"
                ),
                // No package installed it: a copy someone put there, which
                // only its owner can remove or fix.
                _ => {
                    let needs = match self.foreign.len() {
                        1 => "it to be setuid root or to have its file capability",
                        _ => "them to be setuid root or to have their file capabilities",
                    };
                    // Named here unless the fix runs it anyway (for a
                    // helper that is nowhere).
                    let install = match (&self.package, &self.install, self.package_installed) {
                        (Some(package), Some(install), Some(false)) if self.command().is_none() => {
                            format!(
                            "; {package}, which installs a newuidmap and newgidmap that can map, \
                             is not installed (`{install}`)"
                        )
                        }
                        _ => String::new(),
                    };
                    format!(
                        "Remove or fix {paths} (Podman needs {needs}), or take {its_directory} \
                         off PATH: no package installed {them}, so no reinstall changes \
                         {them}{install}"
                    )
                }
            });
        }
        // The package manager's records say a reinstall changes nothing:
        // what keeps it from that, and what to do first.
        if self.reinstalls() {
            words.extend(self.reinstall_keeps.iter().cloned());
        }
        let packaged = self.packaged_unprivileged();
        if !packaged.is_empty() && !self.reinstalls() {
            let (they, are, have, their, them, copy) = if packaged.len() == 1 {
                ("it", "is", "has", "its", "it", "that copy")
            } else {
                ("they", "are", "have", "their", "them", "those copies")
            };
            words.push(format!(
                "Reinstall the package that provides {} so that {they} {are} setuid root or \
                 {have} {their} file capabilities again ({ID_MAP_PACKAGES}); if no package \
                 installed {them} (a copy put there by hand), remove or fix {copy} instead",
                packaged.join(" and ")
            ));
        }
        if !self.not_found().is_empty() && self.install.is_none() {
            words.push(format!(
                "Install newuidmap and newgidmap from your distribution ({ID_MAP_PACKAGES})"
            ));
        }
        (!words.is_empty()).then(|| {
            let mut text = words.remove(0);
            for more in words {
                text.push_str("; ");
                text.push_str(&lowercase_first(&more));
            }
            text
        })
    }

    /// Add what `other` found missing or unable to map, keeping the order.
    fn merge(&mut self, other: IdMapTools) {
        for name in other.missing {
            if !self.missing.contains(&name) {
                self.missing.push(name);
            }
        }
        self.missing
            .sort_by_key(|name| name.as_str() != "newuidmap");
        for path in other.unprivileged {
            if !self.unprivileged.contains(&path) {
                self.unprivileged.push(path);
            }
        }
    }
}

/// The packages that ship `newuidmap` and `newgidmap`, as `dpkg -S`, `rpm
/// -qf` (Fedora 42, RHEL 9) and `pacman -Qo` name them. openSUSE moved them
/// between packages across releases, so it is not named.
pub const ID_MAP_PACKAGES: &str = "Debian and Ubuntu: uidmap; Fedora and RHEL: shadow-utils; \
                                   Arch: shadow";

/// What a failed `podman info` (`error`) says about `newuidmap` and
/// `newgidmap`: with a range in `/etc/subuid` and `/etc/subgid`, a new
/// rootless pause process needs them, and Podman (4.9.3, as tested) fails
/// with `command required for rootless mode with multiple IDs: exec:
/// "newuidmap": executable file not found in $PATH` for one not on `PATH`
/// (the first of the two it lacks), or `cannot set up namespace using
/// "/usr/bin/newuidmap": should have setuid or have filecaps setuid` for one
/// that may not map. Once a pause process has run, Podman logs the same
/// text through logrus (`level=error msg="invalid internal status, try
/// resetting the pause process with \"podman system migrate\": …`, exit
/// 1), with its inner quotes escaped; both forms are read. `None` for any
/// other error.
pub fn id_map_tools_failure(error: &str) -> Option<IdMapTools> {
    let error = &*unescaped_quotes(error);
    let mut tools = IdMapTools::default();
    for name in ["newuidmap", "newgidmap"] {
        if error.contains(&format!("exec: \"{name}\": executable file not found")) {
            tools.missing.push(name.to_string());
        }
    }
    for marker in [
        "cannot set up namespace using \"",
        "cannot setup namespace using \"",
    ] {
        for (index, _) in error.match_indices(marker) {
            let Some((path, after)) = error[index + marker.len()..].split_once('"') else {
                continue;
            };
            let refused = [
                ": should have setuid or have filecaps setuid",
                ": should have setgid or have filecaps setgid",
            ]
            .iter()
            .any(|reason| after.starts_with(reason));
            if refused && !tools.unprivileged.iter().any(|known| known == path) {
                tools.unprivileged.push(path.to_string());
            }
        }
    }
    (!tools.usable()).then_some(tools)
}

/// What to do about a Podman readiness `error` that names `newuidmap` or
/// `newgidmap` ([`id_map_tools_failure`]), for the workbench's readiness
/// item and the daemon's refusals, which do not know this computer's
/// package manager: `axocoatl doctor` names the exact command. `None` for
/// any other error.
pub fn id_map_tools_hint(error: &str) -> Option<String> {
    let tools = id_map_tools_failure(error)?;
    Some(format!(
        "Rootless Podman needs newuidmap and newgidmap to map your subordinate user and group \
         IDs, and {}: install (or reinstall) them ({ID_MAP_PACKAGES}), then run `podman system \
         migrate`; `axocoatl doctor` names the command for this computer",
        tools.describe()
    ))
}

/// The part of a failed `podman info` that names the helper, as Podman
/// wrote it, for the finding.
pub(crate) fn id_map_failure_reason(error: &str) -> Option<String> {
    let error = unescaped_quotes(error);
    let start = [
        "command required for rootless mode",
        "cannot set up namespace using",
        "cannot setup namespace using",
    ]
    .iter()
    .filter_map(|marker| error.find(marker))
    .min()?;
    let rest = &error[start..];
    let line = rest.lines().next().unwrap_or(rest).trim_end();
    // The logrus form ends with the closing quote of `msg="…"`.
    Some(line.strip_suffix('"').unwrap_or(line).to_string())
}

/// `error` with logrus's escaped quotes (`\"`) read as plain ones.
fn unescaped_quotes(error: &str) -> std::borrow::Cow<'_, str> {
    if error.contains("\\\"") {
        std::borrow::Cow::Owned(error.replace("\\\"", "\""))
    } else {
        std::borrow::Cow::Borrowed(error)
    }
}

/// What is wrong with the user's subordinate ids for rootless Podman.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdRangeProblem {
    /// `/etc/subuid` or `/etc/subgid` gives the user no range or too small
    /// a one (or, when the files were not read, `podman info` maps only
    /// the user's own id): `usermod` adds one.
    Missing,
    /// The files give the user a range and the helpers can map it, but the
    /// user namespace Podman's pause process holds maps only the user's own
    /// id (it started before the range was added): `podman system migrate`
    /// restarts it.
    NotApplied,
    /// `newuidmap` or `newgidmap` is missing or cannot map, so `podman info`
    /// maps only the user's own id (a pause process started before the
    /// range was added) or fails: installing them (then `podman system
    /// migrate`) fixes it, not migrate alone.
    NoIdMapTools,
    /// Podman does not run here ([`PodmanReport::runs_here`]) and its
    /// `podman info` maps only one id: the range is missing where it runs,
    /// and onboarding changes nothing for it.
    Elsewhere,
}

/// The problem with the user's subordinate ids in `report`, if any. Rootful
/// Podman needs no range. For a Podman reached elsewhere only its `podman
/// info` decides, as before Axocoatl read the files. A Podman here whose
/// `podman info` fails for want of `newuidmap` or `newgidmap` has one too.
pub fn id_range_problem(report: &PodmanReport) -> Option<IdRangeProblem> {
    if report.readiness == PodmanReadiness::NotInstalled || report.rootless() == Some(false) {
        return None;
    }
    let single_mapping = report.single_mapping();
    if !report.runs_here() {
        return single_mapping.then_some(IdRangeProblem::Elsewhere);
    }
    let no_tools = tools_unusable(report).is_some();
    match &report.subordinate_ids {
        Some(ids) if !ids.enough() => Some(IdRangeProblem::Missing),
        _ if no_tools => Some(IdRangeProblem::NoIdMapTools),
        Some(_) if single_mapping => Some(IdRangeProblem::NotApplied),
        Some(_) => None,
        None if single_mapping => Some(IdRangeProblem::Missing),
        None => None,
    }
}

/// What fixes a [local](PodmanReport::runs_here) id-range problem, in the
/// order it runs: onboarding runs exactly these commands, and the finding
/// prints them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalIdFix {
    /// What only the person can do (in words, before the commands); when
    /// set, onboarding runs nothing.
    pub manual: Option<String>,
    /// Install `newuidmap` and `newgidmap`, as shown (`sudo apt-get install
    /// -y uidmap`), or reinstall their package (`sudo dnf reinstall -y
    /// shadow-utils`).
    pub install: Option<String>,
    /// `usermod` and its arguments, run with `sudo`.
    pub usermod: Option<Vec<String>>,
    /// Then `podman system migrate`, which stops running containers.
    pub migrate: bool,
}

impl LocalIdFix {
    /// The commands as typed, in order.
    pub fn commands(&self) -> Vec<String> {
        let mut commands: Vec<String> = self.install.iter().cloned().collect();
        if let Some(usermod) = &self.usermod {
            commands.push(format!("sudo {}", usermod.join(" ")));
        }
        if self.migrate {
            commands.push("podman system migrate".to_string());
        }
        commands
    }

    /// Runs `sudo`.
    pub fn uses_sudo(&self) -> bool {
        self.install.is_some() || self.usermod.is_some()
    }

    /// The fix as one line.
    pub fn text(&self) -> String {
        let commands = self.commands();
        match &self.manual {
            Some(manual) if commands.is_empty() => manual.clone(),
            Some(manual) => format!("{manual}, then: {}", commands.join(" && ")),
            None if commands == ["podman system migrate"] => MIGRATE_FIX.to_string(),
            None => commands.join(" && "),
        }
    }
}

/// `usermod --add-subuids FIRST-LAST --add-subgids FIRST-LAST USER` for
/// the file (or files) that is short, both when the files were not read,
/// with the free range; `None` when no range is free.
fn usermod_args(report: &PodmanReport, user: &str) -> Option<Vec<String>> {
    let (first, last) = report.free_subordinate_range()?;
    let range = format!("{first}-{last}");
    let (uids, gids) = match &report.subordinate_ids {
        Some(ids) if !ids.enough() => (!ids.uids_enough(), !ids.gids_enough()),
        _ => (true, true),
    };
    let mut args = vec!["usermod".to_string()];
    if uids {
        args.extend(["--add-subuids".to_string(), range.clone()]);
    }
    if gids {
        args.extend(["--add-subgids".to_string(), range]);
    }
    args.push(user.to_string());
    Some(args)
}

/// The helpers are missing or cannot map, while `podman info` maps one id
/// or fails for want of them: what `PATH` and Podman's error say of them.
fn tools_unusable(report: &PodmanReport) -> Option<IdMapTools> {
    let failure = report.id_map_failure();
    if failure.is_none() && !report.single_mapping() {
        return None;
    }
    let mut tools = report.id_map_tools.clone().unwrap_or_default();
    if let Some(failure) = failure {
        tools.merge(failure);
    }
    (!tools.usable()).then_some(tools)
}

/// The fix for `problem` on this computer: `None` for
/// [`IdRangeProblem::Elsewhere`], which nothing here fixes.
pub fn local_fix(
    report: &PodmanReport,
    user: &str,
    problem: &IdRangeProblem,
) -> Option<LocalIdFix> {
    let mut fix = LocalIdFix {
        manual: None,
        install: None,
        usermod: None,
        migrate: true,
    };
    // What only the person can do (then onboarding runs nothing), and the
    // command that installs or reinstalls the helpers' package.
    let tools = |fix: &mut LocalIdFix, tools: &IdMapTools| {
        fix.manual = tools.manual();
        fix.install = tools.command();
    };
    match problem {
        IdRangeProblem::Elsewhere => return None,
        IdRangeProblem::NotApplied => {}
        IdRangeProblem::NoIdMapTools => {
            let found = tools_unusable(report)
                .or_else(|| report.id_map_tools.clone())
                .unwrap_or_default();
            tools(&mut fix, &found);
        }
        IdRangeProblem::Missing => {
            if let Some(found) = tools_unusable(report) {
                tools(&mut fix, &found);
            }
            match usermod_args(report, user) {
                Some(args) => fix.usermod = Some(args),
                None => {
                    // Only the file (or files) that is short gets a range.
                    let (uids, gids) = match &report.subordinate_ids {
                        Some(ids) => (!ids.uids_enough(), !ids.gids_enough()),
                        None => (true, true),
                    };
                    let mut options = String::new();
                    if uids {
                        options.push_str(" --add-subuids FIRST-LAST");
                    }
                    if gids {
                        options.push_str(" --add-subgids FIRST-LAST");
                    }
                    let choose = format!(
                        "Choose {MIN_SUBORDINATE_IDS} IDs that no line of /etc/subuid or \
                         /etc/subgid uses (none is free from 100000 up to 4294967294 here) and \
                         add them: sudo usermod{options} {user}"
                    );
                    fix.manual = Some(match fix.manual.take() {
                        Some(manual) => format!("{manual}; {}", lowercase_first(&choose)),
                        None => choose,
                    });
                }
            }
        }
    }
    Some(fix)
}

fn lowercase_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The failing finding for a Podman that does not run here and maps one
/// id: where it runs, and what to do there (in words: onboarding changes
/// nothing for it, and Podman refuses `podman system migrate` through an
/// override).
fn elsewhere_finding(report: &PodmanReport) -> Finding {
    let (reached, variable) = match &report.connection {
        ConnectionOverride::Host(host) => (
            format!("reached through CONTAINER_HOST={host}"),
            Some("CONTAINER_HOST"),
        ),
        ConnectionOverride::Connection(name) => (
            format!("reached through CONTAINER_CONNECTION={name}"),
            Some("CONTAINER_CONNECTION"),
        ),
        ConnectionOverride::None => match report.machine_in_use() {
            Some(machine) => (format!("in the machine {}", machine.name), None),
            None => (
                "reached through a remote connection (`podman system connection list`)".to_string(),
                None,
            ),
        },
    };
    let detail = format!(
        "Podman {reached} runs rootless with only one user ID mapped: the user it runs as has no \
         range of subordinate user and group IDs where that Podman runs, or that Podman has not \
         picked one up, so it cannot unpack images or start Sessions"
    );
    let unset = |name: &str| {
        if report.machine_platform {
            format!(". Or unset {name} to use your default Podman machine")
        } else {
            format!(". Or unset {name} to use your own rootless Podman on this computer")
        }
    };
    let fix = match report.machine_in_use() {
        // A machine on this computer: its own files, reached with
        // `podman machine ssh`.
        Some(machine) => format!(
            "In the machine (`{}`), give the user Podman runs as a range of subordinate user and \
             group IDs with `sudo usermod --add-subuids` and `--add-subgids` (a range no line of \
             the machine's /etc/subuid or /etc/subgid uses), then restart the machine (`{} && \
             {}`){}",
            machine_command("ssh", Some(&machine.name)),
            machine_command("stop", Some(&machine.name)),
            machine_command("start", Some(&machine.name)),
            variable.map(unset).unwrap_or_default()
        ),
        None => {
            let socket = match &report.connection {
                // On a Mac, such a socket is usually a machine's API socket
                // (`podman machine inspect` names it).
                ConnectionOverride::Host(host)
                    if host.starts_with("unix://") && report.machine_platform =>
                {
                    " (a unix:// socket here is usually a Podman machine's API socket: then it \
                     runs in that machine, reached with `podman machine ssh`)"
                }
                ConnectionOverride::Host(host) if host.starts_with("unix://") => {
                    " (a unix:// socket is on this computer, but the Podman behind it may run as \
                     another user, or be the host's when this shell runs in a toolbox or \
                     distrobox container)"
                }
                _ => "",
            };
            // A rootless Podman keeps the mapping its pause process holds,
            // and a running API service keeps its own: measured with 4.9.3,
            // neither a restarted service nor `podman system migrate` alone
            // maps the range; migrate, then restart the service, does.
            format!(
                "Where that Podman runs{socket}, give the user it runs as a range of subordinate \
                 user and group IDs (`usermod --add-subuids` and `--add-subgids`, as root there, \
                 with a range no other user there has), then, as that user there, run `podman \
                 system migrate` and then restart its API service (either one alone keeps one \
                 ID mapped); onboarding changes nothing for it{}",
                variable.map(unset).unwrap_or_default()
            )
        }
    };
    Finding::new(Level::Fail, "User ID ranges", detail, Some(fix))
}

/// The failing finding for `problem` in `report`, for `user`.
pub fn id_range_finding(report: &PodmanReport, user: &str, problem: &IdRangeProblem) -> Finding {
    const EFFECT: &str = "so rootless Podman cannot unpack images or start Sessions";
    let Some(fix) = local_fix(report, user, problem) else {
        return elsewhere_finding(report);
    };
    let unusable = tools_unusable(report);
    let tools_too = unusable
        .as_ref()
        .map(|tools| format!(", and {}", tools.describe()))
        .unwrap_or_default();
    let detail = match (problem, &report.subordinate_ids) {
        (IdRangeProblem::Missing, Some(ids)) => {
            format!("{}{tools_too}, {EFFECT}", ids.describe(user))
        }
        (IdRangeProblem::Missing, None) => format!(
            "Rootless Podman has no range of subordinate user and group IDs for {user}{tools_too}, \
             {EFFECT}"
        ),
        (IdRangeProblem::NoIdMapTools, _) => {
            let tools = unusable
                .as_ref()
                .or(report.id_map_tools.as_ref())
                .map(IdMapTools::describe)
                .unwrap_or_default();
            match report.id_map_failure().and(match &report.readiness {
                PodmanReadiness::Unavailable(error) => id_map_failure_reason(error),
                _ => None,
            }) {
                Some(reason) => format!(
                    "Rootless Podman cannot map the subordinate user and group IDs {user} has: \
                     {tools}, so `podman info` fails ({reason}) and no Session can start"
                ),
                None => format!(
                    "{tools}, so rootless Podman maps only {user}'s own user ID, whatever \
                     /etc/subuid and /etc/subgid give, and cannot unpack images or start Sessions"
                ),
            }
        }
        (IdRangeProblem::NotApplied | IdRangeProblem::Elsewhere, _) => format!(
            "/etc/subuid and /etc/subgid give {user} a range, but rootless Podman still runs \
             without it (it started before the range was added), {EFFECT}"
        ),
    };
    Finding::new(Level::Fail, "User ID ranges", detail, Some(fix.text()))
}

/// The fix for a sandbox container that failed for want of a range
/// (`ping_group_range`, `insufficient UIDs`), when `id_range_problem` saw
/// none: a range Podman has not picked up when the files give one, else
/// the range to add; for a Podman reached elsewhere, the fix there.
pub fn id_failure_fix(report: &PodmanReport, user: &str) -> String {
    let problem = if !report.runs_here() {
        IdRangeProblem::Elsewhere
    } else if report
        .subordinate_ids
        .as_ref()
        .is_some_and(SubordinateIds::enough)
    {
        IdRangeProblem::NotApplied
    } else {
        IdRangeProblem::Missing
    };
    id_range_finding(report, user, &problem)
        .fix
        .unwrap_or_default()
}

/// The fix for ranges Podman has not picked up yet.
pub const MIGRATE_FIX: &str = "podman system migrate (it stops your running containers)";

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
    let id_problem = id_range_problem(report);
    // `podman info` failed here for want of newuidmap or newgidmap: the
    // Podman line says so and carries the fix (no second line for it).
    let id_failure = id_problem
        .as_ref()
        .filter(|_| report.id_map_failure().is_some())
        .map(|problem| id_range_finding(report, &platform.user, problem));
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
        PodmanReadiness::Unavailable(error) => list.push(match &id_failure {
            Some(finding) => Finding::new(
                Level::Fail,
                "Podman",
                format!(
                    "Podman {version} does not answer: {}",
                    lowercase_first(&finding.detail)
                ),
                finding.fix.clone(),
            ),
            None => Finding::new(
                Level::Fail,
                "Podman",
                format!(
                    "Podman {version} does not answer: {}",
                    error.lines().next().unwrap_or_default()
                ),
                Some(if platform.machine {
                    format!(
                        "Check `podman machine list` and `podman system connection list`; start \
                         the machine with `{}`",
                        machine_command("start", report.machine_in_use().map(|m| m.name.as_str()))
                    )
                } else {
                    "Run `podman info` to see why it fails".to_string()
                }),
            ),
        }),
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
        } else if !platform.machine && (major, minor) != TESTED_LINUX_PODMAN {
            list.push(Finding::new(
                Level::Warn,
                "Podman version",
                linux_untested(&version),
                None,
            ));
        } else if platform.machine && !TESTED_MACOS_PODMAN.contains(&(major, minor)) {
            list.push(Finding::new(
                Level::Warn,
                "Podman version",
                format!(
                    "Podman {version} has not been tested with Axocoatl on macOS yet ({})",
                    tested_macos_text()
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

    if let (Some(problem), None) = (&id_problem, &id_failure) {
        list.push(id_range_finding(report, &platform.user, problem));
    }
    if let Some(runtime) = &report.runtime {
        if !platform.machine && runtime.selinux == Some(true) {
            list.push(Finding::new(
                Level::Ok,
                "SELinux",
                "SELinux is enabled: Session and browser containers run with SELinux labelling \
                 off (--security-opt label=disable), so they can use your repository without \
                 relabelling it; their other limits stay"
                    .to_string(),
                None,
            ));
        }
        if let Some(controllers) = runtime
            .cgroup_controllers
            .as_ref()
            .filter(|_| runtime.rootless != Some(false))
        {
            let missing =
                crate::container_host::ResourceCaps::from_controllers(controllers).not_applied();
            if !missing.is_empty() {
                let in_use = report.machine_in_use().map(|machine| machine.name.as_str());
                let fix = if platform.machine {
                    // Podman 6.1.4 sets a machine up with this drop-in
                    // (read in podman-machine-default).
                    format!(
                        "In the machine (`{}`), give its user the cgroup controllers Podman's \
                         machine setup delegates: `Delegate=memory pids cpu io` under \
                         `[Service]` in /etc/systemd/system/user@.service.d/delegate.conf, then \
                         restart the machine (`{} && {}`)",
                        machine_command("ssh", in_use),
                        machine_command("stop", in_use),
                        machine_command("start", in_use)
                    )
                } else if runtime.cgroup_version.as_deref() == Some("v1") {
                    "This computer uses cgroup v1, where rootless Podman caps no container: boot \
                     it with cgroup v2 (systemd.unified_cgroup_hierarchy=1)"
                        .to_string()
                } else {
                    DELEGATE_CONTROLLERS_FIX.to_string()
                };
                list.push(Finding::new(
                    Level::Warn,
                    "Resource caps",
                    format!(
                        "rootless Podman can use the cgroup controllers [{}], so Session \
                         containers run without their {} cap (sandbox.require_resource_limits: \
                         true refuses to start them instead)",
                        controllers.join(", "),
                        missing.join(", ")
                    ),
                    Some(fix),
                ));
            }
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

    /// `podman info --format json` of Podman 6.1 in a macOS machine, whose
    /// Podman is another patch release than the client's.
    const MAC_INFO: &str = r#"{"host":{"arch":"arm64","kernel":"7.2.5-200.fc44.aarch64",
        "distribution":{"distribution":"fedora","variant":"coreos","version":"44"},
        "security":{"rootless":true,"selinuxEnabled":true},"serviceIsRemote":true,
        "idMappings":{"gidmap":[{"container_id":0,"host_id":1000,"size":1},
        {"container_id":1,"host_id":100000,"size":1000000}],
        "uidmap":[{"container_id":0,"host_id":501,"size":1},
        {"container_id":1,"host_id":100000,"size":1000000}]}},
        "version":{"APIVersion":"6.1.2","Version":"6.1.2"},
        "Client":{"Version":"6.1.4"}}"#;

    /// `machine list --format json`, with the memory as a string (the number
    /// Podman 6 may print instead is read below too).
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
            client_version: Some("6.1.4".to_string()),
            runtime: Some(parse_runtime_info(MAC_INFO.as_bytes()).unwrap()),
            machines,
            default_provider: Some("applehv".to_string()),
            connection: ConnectionOverride::None,
            subordinate_ids: None,
            id_map_tools: None,
            machine_platform: true,
        }
    }

    #[test]
    fn info_list_and_inspect_are_read() {
        let info = parse_runtime_info(MAC_INFO.as_bytes()).unwrap();
        assert_eq!(info.server_version.as_deref(), Some("6.1.2"));
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
            "Podman 6.1.4 ready (runs 6.1.2; machine podman-machine-default: applehv, rootless, \
             6 CPUs / 8 GiB; kernel 7.2.5-200.fc44.aarch64), needed for every Session and every \
             axocoatl run"
        );
    }

    /// Only 6.1, what Homebrew installs for onboarding, is tested on macOS:
    /// Podman 5.8, which an Intel Mac is pointed to, warns.
    #[test]
    fn podman_5_8_on_a_mac_is_not_tested() {
        let mut report = mac_report();
        report.client_version = Some("5.8.2".to_string());
        let findings = findings(&report, &platform(true));
        let levels: Vec<(&str, Level)> = findings.iter().map(|f| (f.name, f.level)).collect();
        assert_eq!(
            levels,
            [("Podman", Level::Ok), ("Podman version", Level::Warn)]
        );
        assert_eq!(
            findings[1].detail,
            "Podman 5.8.2 has not been tested with Axocoatl on macOS yet (6.1 is)"
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
        report.client_version = Some("6.2.0".to_string());
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
            .contains("Podman 6.2.0 has not been tested with Axocoatl on macOS yet (6.1 is)"));
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
            subordinate_ids: None,
            id_map_tools: None,
            machine_platform: false,
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

    /// `podman info` of Podman 4.9.3 on Ubuntu 24.04 whose pause process
    /// maps the user's own id and a range of 65536.
    const LINUX_INFO: &str = r#"{"host":{"security":{"rootless":true,"selinuxEnabled":false},
        "kernel":"6.8.0","idMappings":{
        "uidmap":[{"container_id":0,"host_id":1001,"size":1},{"container_id":1,"host_id":100000,"size":65536}],
        "gidmap":[{"container_id":0,"host_id":1001,"size":1},{"container_id":1,"host_id":100000,"size":65536}]}},
        "version":{"Version":"4.9.3"}}"#;

    fn linux_report(info: &str, ids: Option<SubordinateIds>) -> PodmanReport {
        PodmanReport {
            readiness: PodmanReadiness::Ready,
            client_version: Some("4.9.3".to_string()),
            runtime: Some(parse_runtime_info(info.as_bytes()).unwrap()),
            machines: Vec::new(),
            default_provider: None,
            connection: ConnectionOverride::None,
            subordinate_ids: ids,
            id_map_tools: None,
            machine_platform: false,
        }
    }

    /// A Linux host whose user is not delegated the cpu controller hears
    /// that Session containers run without their CPU cap, and how to
    /// delegate it, and so does a Podman machine; SELinux is said as what
    /// Axocoatl does about it.
    #[test]
    fn missing_cgroup_controllers_and_selinux_are_said() {
        let linux = platform(false);
        let info = LINUX_INFO.replace(
            r#""kernel":"6.8.0","#,
            r#""kernel":"6.8.0","cgroupControllers":["memory","pids"],"cgroupVersion":"v2","#,
        );
        let report = linux_report(&info, None);
        let runtime = report.runtime.as_ref().unwrap();
        assert_eq!(
            runtime.cgroup_controllers,
            Some(vec!["memory".to_string(), "pids".to_string()])
        );
        let all = findings(&report, &linux);
        let caps = all.iter().find(|f| f.name == "Resource caps").unwrap();
        assert_eq!(caps.level, Level::Warn);
        assert!(
            caps.detail.contains("[memory, pids]")
                && caps
                    .detail
                    .contains("without their CPU (2 CPUs, cgroup controller cpu) cap"),
            "{}",
            caps.detail
        );
        assert_eq!(caps.fix.as_deref(), Some(DELEGATE_CONTROLLERS_FIX));
        assert!(!all.iter().any(|f| f.name == "SELinux"));
        // Every controller, as Ubuntu 24.04 delegates with cpu: nothing.
        let delegated = LINUX_INFO.replace(
            r#""kernel":"6.8.0","#,
            r#""kernel":"6.8.0","cgroupControllers":["cpu","memory","pids"],"#,
        );
        assert!(!findings(&linux_report(&delegated, None), &linux)
            .iter()
            .any(|f| f.name == "Resource caps"));
        // cgroup v1 delegates none.
        let v1 = LINUX_INFO.replace(
            r#""kernel":"6.8.0","#,
            r#""kernel":"6.8.0","cgroupControllers":null,"cgroupVersion":"v1","#,
        );
        let v1 = findings(&linux_report(&v1, None), &linux);
        let caps = v1.iter().find(|f| f.name == "Resource caps").unwrap();
        assert!(caps.fix.as_deref().unwrap().contains("cgroup v2"));
        // Fedora with SELinux enforcing.
        let selinux = LINUX_INFO.replace(r#""selinuxEnabled":false"#, r#""selinuxEnabled":true"#);
        let selinux = findings(&linux_report(&selinux, None), &linux);
        let line = selinux.iter().find(|f| f.name == "SELinux").unwrap();
        assert_eq!(line.level, Level::Ok);
        assert!(line.detail.contains("label=disable"), "{}", line.detail);
        // A Podman machine without one: its fix is in the machine, as
        // Podman's machine setup has it.
        let with = |controllers: &str| {
            let mut report = mac_report();
            report.runtime = Some(
                parse_runtime_info(
                    MAC_INFO
                        .replace(
                            r#""arch":"arm64","#,
                            &format!(r#""arch":"arm64","cgroupControllers":{controllers},"#),
                        )
                        .as_bytes(),
                )
                .unwrap(),
            );
            findings(&report, &platform(true))
        };
        let machine = with(r#"["memory","pids"]"#);
        let caps = machine.iter().find(|f| f.name == "Resource caps").unwrap();
        assert_eq!(caps.level, Level::Warn);
        assert_eq!(
            caps.fix.as_deref(),
            Some(
                "In the machine (`podman machine ssh`), give its user the cgroup controllers \
                 Podman's machine setup delegates: `Delegate=memory pids cpu io` under \
                 `[Service]` in /etc/systemd/system/user@.service.d/delegate.conf, then restart \
                 the machine (`podman machine stop && podman machine start`)"
            )
        );
        // podman-machine-default (Podman 6.1.4) lists them all: nothing.
        assert!(!with(r#"["cpu","io","memory","pids"]"#)
            .iter()
            .any(|f| f.name == "Resource caps"));
    }

    #[test]
    fn subordinate_ids_are_read_by_name_or_uid_and_summed() {
        let subuid = "# comment\nnewuser:100000:65536\nalex:165536:1000\n1001:200000:64536\n\
                      bad line\nother:265536:65536\n";
        let subgid = "newuser:100000:65536\nalex:165536:65536\n";
        let ids = parse_subordinate_ids(subuid, subgid, "alex", Some(1001));
        assert_eq!(ids.uids, 65_536);
        assert_eq!(ids.gids, 65_536);
        assert!(ids.enough());
        // The free range skips every line of both files.
        assert_eq!(ids.free, Some((331_072, 396_607)));
        let ids = parse_subordinate_ids(subuid, subgid, "alex", None);
        assert_eq!((ids.uids, ids.gids), (1000, 65_536));
        assert!(!ids.enough());
        // Nothing in the files: the standard first range is free.
        let none = parse_subordinate_ids("", "", "alex", Some(1001));
        assert_eq!(
            (none.uids, none.gids, none.free),
            (0, 0, Some((100_000, 165_535)))
        );
        // A range ending inside the default one moves the free range past it.
        let shifted = parse_subordinate_ids("x:50000:60000\n", "", "alex", None);
        assert_eq!(shifted.free, Some((110_000, 175_535)));
        // A line reaching past the last id leaves no free range to name:
        // none is claimed (100000-165535 is x's), and nothing overflows.
        let full = parse_subordinate_ids(
            "x:100000:18446744073709551615\n",
            "alex:1:18446744073709551615\n",
            "alex",
            None,
        );
        assert_eq!(full.gids, u64::MAX);
        assert_eq!(full.free, None);
        // The last range that fits ends at 4294967294; one more id does not.
        let edge = u64::from(u32::MAX) - MIN_SUBORDINATE_IDS;
        let fits = parse_subordinate_ids(&format!("x:100000:{}\n", edge - 100_000), "", "a", None);
        assert_eq!(fits.free, Some((edge, u64::from(u32::MAX) - 1)));
        let over = parse_subordinate_ids(&format!("x:100000:{}\n", edge - 99_999), "", "a", None);
        assert_eq!(over.free, None);
    }

    #[test]
    fn subordinate_id_files_are_read_under_a_root() {
        let root = tempfile::tempdir().unwrap();
        let etc = root.path().join("etc");
        std::fs::create_dir_all(&etc).unwrap();
        // No files: no range, as Podman reads it.
        let ids = read_subordinate_ids(root.path(), "alex", Some(1001)).unwrap();
        assert_eq!((ids.uids, ids.gids), (0, 0));
        std::fs::write(etc.join("subuid"), "alex:100000:65536\n").unwrap();
        std::fs::write(etc.join("subgid"), "alex:100000:65536\n").unwrap();
        assert!(read_subordinate_ids(root.path(), "alex", Some(1001))
            .unwrap()
            .enough());
        // Root, and an unknown user, are not judged from the files.
        assert!(read_subordinate_ids(root.path(), "root", Some(0)).is_none());
        assert!(read_subordinate_ids(root.path(), "$USER", None).is_none());
        // Ranges from SSSD or LDAP are not in the files.
        std::fs::write(etc.join("nsswitch.conf"), "passwd: files\nsubid: sss\n").unwrap();
        assert!(read_subordinate_ids(root.path(), "alex", Some(1001)).is_none());
        std::fs::write(etc.join("nsswitch.conf"), "subid: files # local\n").unwrap();
        assert!(read_subordinate_ids(root.path(), "alex", Some(1001)).is_some());
    }

    /// The CI case: the user's lines are removed from /etc/subuid and
    /// /etc/subgid while Podman's pause process still holds the old range,
    /// so `podman info` still reports it. Every new keep-id container is
    /// mapped from the files and fails in crun (ping_group_range), so the
    /// files decide: FAIL with usermod (a free range) and migrate.
    #[test]
    fn ranges_removed_from_the_files_fail_although_podman_info_shows_them() {
        let ids = parse_subordinate_ids(
            "newuser:100000:65536\n",
            "newuser:100000:65536\n",
            "alex",
            Some(1001),
        );
        let report = linux_report(LINUX_INFO, Some(ids));
        assert_eq!(id_range_problem(&report), Some(IdRangeProblem::Missing));
        let list = findings(&report, &platform(false));
        let ranges = list.iter().find(|f| f.name == "User ID ranges").unwrap();
        assert_eq!(ranges.level, Level::Fail);
        assert_eq!(
            ranges.detail,
            "alex has no range of subordinate user and group IDs in /etc/subuid and /etc/subgid, \
             so rootless Podman cannot unpack images or start Sessions"
        );
        assert_eq!(
            ranges.fix.as_deref(),
            Some(
                "sudo usermod --add-subuids 165536-231071 --add-subgids 165536-231071 alex && \
                 podman system migrate"
            )
        );
    }

    /// Only Ubuntu 24.04's Podman 4.9 is tested on Linux: an older or a
    /// newer release (Fedora's 5.x, Arch's 6.x) warns, as any release
    /// other than 6.1 does on macOS.
    #[test]
    fn a_linux_podman_other_than_4_9_is_not_tested() {
        let ids = parse_subordinate_ids("alex:100000:65536\n", "alex:100000:65536\n", "alex", None);
        let version_finding = |version: &str| {
            let mut report = linux_report(LINUX_INFO, Some(ids.clone()));
            report.client_version = Some(version.to_string());
            findings(&report, &platform(false))
                .into_iter()
                .find(|finding| finding.name == "Podman version")
        };
        assert_eq!(version_finding("4.9.3"), None);
        assert_eq!(version_finding("4.9.4-rhel"), None);
        for version in ["4.6.2", "5.4.2", "6.0.1"] {
            let finding = version_finding(version).unwrap();
            assert_eq!(finding.level, Level::Warn, "{version}");
            assert_eq!(
                finding.detail,
                format!("Podman {version} has not been tested with Axocoatl on Linux yet (4.9 is)")
            );
        }
        assert_eq!(version_finding("4.2.0").unwrap().level, Level::Fail);
    }

    #[test]
    fn a_small_range_fails_and_a_range_podman_has_not_applied_needs_migrate() {
        let small = parse_subordinate_ids("alex:100000:1000\n", "alex:100000:1000\n", "alex", None);
        let report = linux_report(LINUX_INFO, Some(small));
        let list = findings(&report, &platform(false));
        let ranges = list.iter().find(|f| f.name == "User ID ranges").unwrap();
        assert_eq!(
            ranges.detail,
            "alex has 1000 subordinate user IDs in /etc/subuid and 1000 subordinate group IDs \
             in /etc/subgid, and needs at least 65536 of each, so rootless Podman cannot unpack \
             images or start Sessions"
        );
        assert!(ranges.fix.as_deref().unwrap().starts_with(
            "sudo usermod --add-subuids 101000-166535 --add-subgids 101000-166535 alex"
        ));

        // The files are right; the pause process maps only the user's id.
        let single = r#"{"host":{"security":{"rootless":true},"idMappings":{
            "uidmap":[{"container_id":0,"host_id":1001,"size":1}],
            "gidmap":[{"container_id":0,"host_id":1001,"size":1}]}}}"#;
        let ok = parse_subordinate_ids("alex:100000:65536\n", "alex:100000:65536\n", "alex", None);
        let report = linux_report(single, Some(ok.clone()));
        assert_eq!(id_range_problem(&report), Some(IdRangeProblem::NotApplied));
        let list = findings(&report, &platform(false));
        let ranges = list.iter().find(|f| f.name == "User ID ranges").unwrap();
        assert_eq!(ranges.level, Level::Fail);
        assert_eq!(ranges.fix.as_deref(), Some(MIGRATE_FIX));

        // Both right: no finding. Rootful Podman needs no range.
        let report = linux_report(LINUX_INFO, Some(ok));
        assert_eq!(id_range_problem(&report), None);
        assert!(!findings(&report, &platform(false))
            .iter()
            .any(|f| f.name == "User ID ranges"));
        let mut rootful = linux_report(single, None);
        rootful.runtime.as_mut().unwrap().rootless = Some(false);
        assert_eq!(id_range_problem(&rootful), None);
    }

    /// `podman info` of a rootless Podman that maps only the user's own id.
    const SINGLE_INFO: &str = r#"{"host":{"security":{"rootless":true},"idMappings":{
        "uidmap":[{"container_id":0,"host_id":1001,"size":1}],
        "gidmap":[{"container_id":0,"host_id":1001,"size":1}]}}}"#;

    fn ranges_finding(report: &PodmanReport) -> Option<Finding> {
        findings(report, &platform(false))
            .into_iter()
            .find(|f| f.name == "User ID ranges")
    }

    /// When only one file is short, the finding names that file alone (65536
    /// is not "fewer than 65536"), and usermod adds a range to it alone.
    #[test]
    fn only_the_short_file_is_named_and_given_a_range() {
        let ids = parse_subordinate_ids("alex:100000:65536\n", "", "alex", None);
        let report = linux_report(LINUX_INFO, Some(ids));
        let ranges = ranges_finding(&report).unwrap();
        assert_eq!(
            ranges.detail,
            "alex has no subordinate group IDs in /etc/subgid and needs at least 65536 (its 65536 \
             user IDs in /etc/subuid are enough), so rootless Podman cannot unpack images or \
             start Sessions"
        );
        assert_eq!(
            ranges.fix.as_deref(),
            Some("sudo usermod --add-subgids 165536-231071 alex && podman system migrate")
        );

        let ids = parse_subordinate_ids("alex:100000:1000\n", "alex:100000:65536\n", "alex", None);
        let report = linux_report(LINUX_INFO, Some(ids));
        let ranges = ranges_finding(&report).unwrap();
        assert_eq!(
            ranges.detail,
            "alex has 1000 subordinate user IDs in /etc/subuid and needs at least 65536 (its \
             65536 group IDs in /etc/subgid are enough), so rootless Podman cannot unpack images \
             or start Sessions"
        );
        assert_eq!(
            ranges.fix.as_deref(),
            Some("sudo usermod --add-subuids 165536-231071 alex && podman system migrate")
        );
        let fix = local_fix(&report, "alex", &IdRangeProblem::Missing).unwrap();
        assert_eq!(
            fix.usermod.as_deref(),
            Some(
                &[
                    "usermod".to_string(),
                    "--add-subuids".to_string(),
                    "165536-231071".to_string(),
                    "alex".to_string()
                ][..]
            )
        );
        assert!(fix.uses_sudo() && fix.manual.is_none());
    }

    /// Files that leave no free range from 100000 up to the last id: the
    /// fix names no range (one it named would be another user's), and
    /// onboarding has no command to run.
    #[test]
    fn no_free_range_is_claimed_when_none_is_left() {
        let ids = parse_subordinate_ids("x:100000:18446744073709551615\n", "", "alex", None);
        assert_eq!(ids.free, None);
        let report = linux_report(LINUX_INFO, Some(ids));
        assert_eq!(report.free_subordinate_range(), None);
        let fix = local_fix(&report, "alex", &IdRangeProblem::Missing).unwrap();
        assert!(fix.usermod.is_none());
        assert_eq!(
            fix.text(),
            "Choose 65536 IDs that no line of /etc/subuid or /etc/subgid uses (none is free from \
             100000 up to 4294967294 here) and add them: sudo usermod --add-subuids FIRST-LAST \
             --add-subgids FIRST-LAST alex, then: podman system migrate"
        );
        assert_eq!(ranges_finding(&report).unwrap().fix, Some(fix.text()));
        // Only /etc/subgid is short: the command adds a group range alone.
        let ids = parse_subordinate_ids(
            "alex:1000:65536\nx:100000:18446744073709551615\n",
            "",
            "alex",
            None,
        );
        assert_eq!(
            (ids.uids_enough(), ids.gids_enough(), ids.free),
            (true, false, None)
        );
        let report = linux_report(LINUX_INFO, Some(ids));
        let fix = local_fix(&report, "alex", &IdRangeProblem::Missing).unwrap();
        assert_eq!(
            fix.text(),
            "Choose 65536 IDs that no line of /etc/subuid or /etc/subgid uses (none is free from \
             100000 up to 4294967294 here) and add them: sudo usermod --add-subgids FIRST-LAST \
             alex, then: podman system migrate"
        );
        // And only /etc/subuid.
        let ids = parse_subordinate_ids(
            "x:100000:18446744073709551615\n",
            "alex:1000:65536\n",
            "alex",
            None,
        );
        let report = linux_report(LINUX_INFO, Some(ids));
        assert!(local_fix(&report, "alex", &IdRangeProblem::Missing)
            .unwrap()
            .text()
            .ends_with(
                "add them: sudo usermod --add-subuids FIRST-LAST alex, then: podman system migrate"
            ));
        // Not read (SSSD): the default range, as before.
        let unread = linux_report(SINGLE_INFO, None);
        assert_eq!(
            unread.free_subordinate_range(),
            Some(DEFAULT_SUBORDINATE_RANGE)
        );
    }

    /// A Podman reached through CONTAINER_HOST or CONTAINER_CONNECTION is
    /// judged by its own `podman info`, as before the files were read: this
    /// computer's empty /etc/subuid says nothing about it. With one id
    /// mapped it still fails, but the fix names nothing to run here: no
    /// sudo usermod for this computer's user, and `podman system migrate`
    /// only as that Podman's user where it runs (Podman refuses it through
    /// an override, and a restarted API service joins the old pause
    /// process, keeping one id mapped).
    #[test]
    fn a_podman_reached_elsewhere_is_judged_by_its_podman_info() {
        let empty = parse_subordinate_ids("", "", "alex", Some(1001));
        for connection in [
            ConnectionOverride::Host("ssh://builder/run/user/1000/podman/podman.sock".to_string()),
            ConnectionOverride::Host("unix:///run/user/1001/podman/podman.sock".to_string()),
            ConnectionOverride::Connection("builder".to_string()),
        ] {
            let mut report = linux_report(LINUX_INFO, Some(empty.clone()));
            report.connection = connection.clone();
            assert!(!report.runs_here());
            assert_eq!(id_range_problem(&report), None, "{connection:?}");
            assert!(ranges_finding(&report).is_none(), "{connection:?}");

            let mut report = linux_report(SINGLE_INFO, Some(empty.clone()));
            report.connection = connection.clone();
            assert_eq!(id_range_problem(&report), Some(IdRangeProblem::Elsewhere));
            assert_eq!(local_fix(&report, "alex", &IdRangeProblem::Elsewhere), None);
            let ranges = ranges_finding(&report).unwrap();
            assert_eq!(ranges.level, Level::Fail);
            let fix = ranges.fix.unwrap();
            let variable = connection.describe().unwrap();
            assert!(ranges.detail.contains(&variable), "{}", ranges.detail);
            assert!(
                fix.contains(
                    "then, as that user there, run `podman system migrate` and then restart its \
                     API service (either one alone keeps one ID mapped)"
                ),
                "{fix}"
            );
            assert!(!fix.contains("restart that Podman"), "{fix}");
            assert!(!fix.contains("sudo") && !fix.contains("alex"), "{fix}");
            // Not "nothing on this computer changes it": a unix:// socket
            // may be this computer's (another user's Podman).
            assert!(!fix.contains("nothing on this computer"), "{fix}");
            assert!(
                fix.starts_with("Where that Podman runs")
                    && fix.contains("onboarding changes nothing"),
                "{fix}"
            );
            assert_eq!(
                fix.contains("a unix:// socket is on this computer"),
                matches!(&connection, ConnectionOverride::Host(host) if host.starts_with("unix://")),
                "{fix}"
            );
            // Unsetting the variable selects this computer's own Podman on
            // Linux, and the default machine on macOS.
            let name = variable.split('=').next().unwrap();
            assert!(
                fix.ends_with(&format!(
                    "Or unset {name} to use your own rootless Podman on this computer"
                )),
                "{fix}"
            );
            let failure = id_failure_fix(&report, "alex");
            assert_eq!(failure, fix);
            report.machine_platform = true;
            let fix = ranges_finding(&report).unwrap().fix.unwrap();
            assert!(
                fix.ends_with(&format!(
                    "Or unset {name} to use your default Podman machine"
                )),
                "{fix}"
            );
            // On a Mac a unix:// socket is a machine's, not a toolbox's.
            let unix = matches!(&connection, ConnectionOverride::Host(host) if host.starts_with("unix://"));
            assert!(!fix.contains("toolbox"), "{fix}");
            assert_eq!(
                fix.contains(
                    "(a unix:// socket here is usually a Podman machine's API socket: then it \
                     runs in that machine, reached with `podman machine ssh`)"
                ),
                unix,
                "{fix}"
            );
            assert!(fix.contains("run `podman system migrate` and then restart its API service"));
        }
        // podman-remote, or a default remote connection, without either
        // variable: `podman info` says the service is remote.
        let mut remote = linux_report(SINGLE_INFO, Some(empty));
        remote.runtime.as_mut().unwrap().remote = Some(true);
        assert_eq!(id_range_problem(&remote), Some(IdRangeProblem::Elsewhere));
        let ranges = ranges_finding(&remote).unwrap();
        assert!(
            ranges.detail.starts_with(
                "Podman reached through a remote connection (`podman system connection list`)"
            ),
            "{}",
            ranges.detail
        );
        let fix = ranges.fix.unwrap();
        assert!(
            fix.contains("run `podman system migrate` and then restart its API service")
                && !fix.contains("sudo"),
            "{fix}"
        );
        // On macOS the machine's Podman is remote too.
        let mut mac = mac_report();
        mac.runtime = Some(parse_runtime_info(SINGLE_INFO.as_bytes()).unwrap());
        mac.runtime.as_mut().unwrap().remote = Some(true);
        let ranges = findings(&mac, &platform(true))
            .into_iter()
            .find(|f| f.name == "User ID ranges")
            .unwrap();
        assert!(
            ranges
                .detail
                .starts_with("Podman in the machine podman-machine-default runs rootless"),
            "{}",
            ranges.detail
        );
        // The machine is fixed from this computer, inside it.
        assert_eq!(
            ranges.fix.as_deref(),
            Some(
                "In the machine (`podman machine ssh`), give the user Podman runs as a range of \
                 subordinate user and group IDs with `sudo usermod --add-subuids` and \
                 `--add-subgids` (a range no line of the machine's /etc/subuid or /etc/subgid \
                 uses), then restart the machine (`podman machine stop && podman machine start`)"
            )
        );
        // A machine CONTAINER_CONNECTION names: that machine, by name, or
        // the default machine once it is unset.
        mac.machines.push(MachineInfo {
            name: "dev".to_string(),
            ..MachineInfo::default()
        });
        mac.connection = ConnectionOverride::Connection("dev".to_string());
        let fix = findings(&mac, &platform(true))
            .into_iter()
            .find(|f| f.name == "User ID ranges")
            .and_then(|f| f.fix)
            .unwrap();
        assert!(
            fix.starts_with("In the machine (`podman machine ssh dev`)")
                && fix.contains("(`podman machine stop dev && podman machine start dev`)")
                && fix.ends_with(
                    ". Or unset CONTAINER_CONNECTION to use your default Podman machine"
                ),
            "{fix}"
        );
    }

    /// What Podman 4.9.3 prints when `newuidmap` or `newgidmap` cannot map a
    /// range: not on PATH (the first of the two it lacks), or neither
    /// setuid nor given file capabilities. Other errors are not taken for it.
    #[test]
    fn podman_info_failures_for_the_id_map_tools_are_recognised() {
        let missing = "OCI container setup failed: podman info failed: Error: command required \
                       for rootless mode with multiple IDs: exec: \"newuidmap\": executable file \
                       not found in $PATH";
        let tools = id_map_tools_failure(missing).unwrap();
        assert_eq!(tools.missing, ["newuidmap"]);
        assert!(tools.unprivileged.is_empty());
        assert_eq!(
            id_map_failure_reason(missing).as_deref(),
            Some(
                "command required for rootless mode with multiple IDs: exec: \"newuidmap\": \
                 executable file not found in $PATH"
            )
        );
        // After `podman system migrate` without the helpers, Podman 4.9.3
        // logs the failure through logrus (exit 1), its quotes escaped.
        let migrated = r#"time="2026-10-20T20:01:02-07:00" level=error msg="invalid internal status, try resetting the pause process with \"podman system migrate\": command required for rootless mode with multiple IDs: exec: \"newgidmap\": executable file not found in $PATH""#;
        assert_eq!(
            id_map_tools_failure(migrated).unwrap().missing,
            ["newgidmap"]
        );
        assert_eq!(
            id_map_failure_reason(migrated).as_deref(),
            Some(
                "command required for rootless mode with multiple IDs: exec: \"newgidmap\": \
                 executable file not found in $PATH"
            )
        );
        assert!(id_map_tools_hint(migrated).is_some());
        let logged_setuid = r#"time="2026-10-20T20:01:02-07:00" level=error msg="invalid internal status, try resetting the pause process with \"podman system migrate\": cannot set up namespace using \"/usr/bin/newuidmap\": should have setuid or have filecaps setuid: exit status 1""#;
        assert_eq!(
            id_map_tools_failure(logged_setuid).unwrap().unprivileged,
            ["/usr/bin/newuidmap"]
        );
        for (error, path) in [
            (
                "Error: cannot set up namespace using \"/usr/bin/newuidmap\": should have setuid \
                 or have filecaps setuid: exit status 1",
                "/usr/bin/newuidmap",
            ),
            (
                "Error: cannot setup namespace using \"/home/a/bin/newgidmap\": should have \
                 setgid or have filecaps setgid: exit status 1",
                "/home/a/bin/newgidmap",
            ),
        ] {
            let tools = id_map_tools_failure(error).unwrap();
            assert!(tools.missing.is_empty());
            assert_eq!(tools.unprivileged, [path]);
        }
        for other in [
            "Error: cannot set up namespace using \"/usr/bin/newuidmap\": exit status 1",
            "Cannot connect to Podman. Please verify your connection",
            "newuidmap: write to uid_map failed: Operation not permitted",
            "",
        ] {
            assert_eq!(id_map_tools_failure(other), None, "{other}");
            assert_eq!(id_map_tools_hint(other), None, "{other}");
        }
        let hint = id_map_tools_hint(missing).unwrap();
        assert_eq!(
            hint,
            "Rootless Podman needs newuidmap and newgidmap to map your subordinate user and group \
             IDs, and newuidmap is not found on PATH: install (or reinstall) them (Debian and \
             Ubuntu: uidmap; Fedora and RHEL: shadow-utils; Arch: shadow), then run `podman \
             system migrate`; `axocoatl doctor` names the command for this computer"
        );
    }

    /// The usual shape on Debian and Ubuntu without `uidmap`: the files give
    /// the user a range, and a new pause process fails instead of mapping
    /// one id, so `podman info` fails. Doctor's Podman line says why and
    /// carries the install fix (no second line); `podman system migrate`
    /// alone, which Podman's own message suggests, is never the fix.
    #[test]
    fn a_podman_info_that_fails_for_want_of_newuidmap_gets_the_install_fix() {
        let error = "OCI container setup failed: podman info failed: Error: command required for \
                     rootless mode with multiple IDs: exec: \"newuidmap\": executable file not \
                     found in $PATH";
        let ok = parse_subordinate_ids("alex:100000:65536\n", "alex:100000:65536\n", "alex", None);
        let tools = IdMapTools {
            missing: vec!["newuidmap".to_string(), "newgidmap".to_string()],
            package: Some("uidmap".to_string()),
            package_installed: Some(false),
            install: Some("sudo apt-get install -y uidmap".to_string()),
            ..IdMapTools::default()
        };
        let mut report = linux_report(SINGLE_INFO, Some(ok));
        report.readiness = PodmanReadiness::Unavailable(error.to_string());
        report.runtime = None;
        report.id_map_tools = Some(tools.clone());
        assert_eq!(
            id_range_problem(&report),
            Some(IdRangeProblem::NoIdMapTools)
        );
        let fix = local_fix(&report, "alex", &IdRangeProblem::NoIdMapTools).unwrap();
        assert_eq!(
            fix.commands(),
            ["sudo apt-get install -y uidmap", "podman system migrate"]
        );
        let list = findings(&report, &platform(false));
        assert!(!list.iter().any(|f| f.name == "User ID ranges"), "{list:?}");
        let podman = list.iter().find(|f| f.name == "Podman").unwrap();
        assert_eq!(podman.level, Level::Fail);
        assert_eq!(
            podman.detail,
            "Podman 4.9.3 does not answer: rootless Podman cannot map the subordinate user and \
             group IDs alex has: newuidmap and newgidmap are not installed, so `podman info` \
             fails (command required for rootless mode with multiple IDs: exec: \"newuidmap\": \
             executable file not found in $PATH) and no Session can start"
        );
        assert_eq!(
            podman.fix.as_deref(),
            Some("sudo apt-get install -y uidmap && podman system migrate")
        );

        // Not read on PATH (no id_map_tools): what the error says, and no
        // package known, so the fix is in words.
        report.id_map_tools = None;
        let podman = findings(&report, &platform(false))
            .into_iter()
            .find(|f| f.name == "Podman")
            .unwrap();
        assert!(
            podman
                .detail
                .contains("alex has: newuidmap is not found on PATH, so"),
            "{}",
            podman.detail
        );
        assert_eq!(
            podman.fix.as_deref(),
            Some(
                "Install newuidmap and newgidmap from your distribution (Debian and Ubuntu: \
                 uidmap; Fedora and RHEL: shadow-utils; Arch: shadow), then: podman system migrate"
            )
        );

        // A helper that is there but may not map, its package's state not
        // known: reinstalled, in words.
        report.readiness = PodmanReadiness::Unavailable(
            "Error: cannot set up namespace using \"/usr/bin/newuidmap\": should have setuid or \
             have filecaps setuid: exit status 1"
                .to_string(),
        );
        report.id_map_tools = Some(IdMapTools {
            install: tools.install.clone(),
            ..IdMapTools::default()
        });
        assert_eq!(
            findings(&report, &platform(false))
                .into_iter()
                .find(|f| f.name == "Podman")
                .and_then(|f| f.fix),
            Some(
                "Reinstall the package that provides /usr/bin/newuidmap so that it is setuid root \
                 or has its file capabilities again (Debian and Ubuntu: uidmap; Fedora and RHEL: \
                 shadow-utils; Arch: shadow); if no package installed it (a copy put there by \
                 hand), remove or fix that copy instead, then: podman system migrate"
                    .to_string()
            )
        );

        // A range too small as well: one fix installs, adds and migrates.
        report.readiness = PodmanReadiness::Unavailable(error.to_string());
        report.id_map_tools = Some(tools.clone());
        report.subordinate_ids = Some(parse_subordinate_ids(
            "alex:100000:1000\n",
            "alex:100000:1000\n",
            "alex",
            None,
        ));
        assert_eq!(id_range_problem(&report), Some(IdRangeProblem::Missing));
        let podman = findings(&report, &platform(false))
            .into_iter()
            .find(|f| f.name == "Podman")
            .unwrap();
        assert_eq!(
            podman.fix.as_deref(),
            Some(
                "sudo apt-get install -y uidmap && sudo usermod --add-subuids 101000-166535 \
                 --add-subgids 101000-166535 alex && podman system migrate"
            )
        );

        // Another error, or the same through CONTAINER_HOST: not this.
        report.subordinate_ids = Some(parse_subordinate_ids(
            "alex:100000:65536\n",
            "alex:100000:65536\n",
            "alex",
            None,
        ));
        report.readiness = PodmanReadiness::Unavailable("Cannot connect to Podman".to_string());
        assert_eq!(id_range_problem(&report), None);
        let podman = findings(&report, &platform(false)).remove(0);
        assert_eq!(
            podman.fix.as_deref(),
            Some("Run `podman info` to see why it fails")
        );
        report.readiness = PodmanReadiness::Unavailable(error.to_string());
        report.connection = ConnectionOverride::Host("unix:///run/user/2000/podman.sock".into());
        assert_eq!(id_range_problem(&report), None);
    }

    /// `podman info` maps one id while the files give a range: when
    /// newuidmap or newgidmap is missing (Debian and Ubuntu ship them in
    /// uidmap, which Podman only recommends), the fix installs them;
    /// `podman system migrate` alone would leave it the same.
    #[test]
    fn missing_id_map_tools_are_installed_instead_of_only_migrating() {
        let ok = parse_subordinate_ids("alex:100000:65536\n", "alex:100000:65536\n", "alex", None);
        let tools = IdMapTools {
            missing: vec!["newuidmap".to_string(), "newgidmap".to_string()],
            package: Some("uidmap".to_string()),
            package_installed: Some(false),
            install: Some("sudo apt-get install -y uidmap".to_string()),
            ..IdMapTools::default()
        };
        let mut report = linux_report(SINGLE_INFO, Some(ok.clone()));
        report.id_map_tools = Some(tools.clone());
        assert_eq!(
            id_range_problem(&report),
            Some(IdRangeProblem::NoIdMapTools)
        );
        let ranges = ranges_finding(&report).unwrap();
        assert_eq!(
            ranges.detail,
            "newuidmap and newgidmap are not installed, so rootless Podman maps only alex's own \
             user ID, whatever /etc/subuid and /etc/subgid give, and cannot unpack images or \
             start Sessions"
        );
        assert_eq!(
            ranges.fix.as_deref(),
            Some("sudo apt-get install -y uidmap && podman system migrate")
        );
        let fix = local_fix(&report, "alex", &IdRangeProblem::NoIdMapTools).unwrap();
        assert!(fix.uses_sudo() && fix.manual.is_none());

        // A helper that is there but cannot map, its package's state not
        // known: no command here, the reinstall in words.
        report.id_map_tools = Some(IdMapTools {
            unprivileged: vec!["/usr/bin/newuidmap".to_string()],
            install: tools.install.clone(),
            ..IdMapTools::default()
        });
        let ranges = ranges_finding(&report).unwrap();
        assert!(ranges
            .detail
            .starts_with("/usr/bin/newuidmap is neither setuid nor given file capabilities"));
        // Fedora and RHEL give the helpers file capabilities, not setuid.
        assert_eq!(
            ranges.fix.as_deref(),
            Some(
                "Reinstall the package that provides /usr/bin/newuidmap so that it is setuid root \
                 or has its file capabilities again (Debian and Ubuntu: uidmap; Fedora and RHEL: \
                 shadow-utils; Arch: shadow); if no package installed it (a copy put there by \
                 hand), remove or fix that copy instead, then: podman system migrate"
            )
        );
        // Both: "they".
        report.id_map_tools = Some(IdMapTools {
            unprivileged: vec![
                "/usr/bin/newuidmap".to_string(),
                "/usr/bin/newgidmap".to_string(),
            ],
            install: tools.install.clone(),
            ..IdMapTools::default()
        });
        assert!(ranges_finding(&report).unwrap().fix.unwrap().starts_with(
            "Reinstall the package that provides /usr/bin/newuidmap and /usr/bin/newgidmap so \
                 that they are setuid root or have their file capabilities again"
        ));
        // No known package manager (openSUSE among them): in words.
        report.id_map_tools = Some(IdMapTools {
            install: None,
            package: None,
            package_installed: None,
            ..tools.clone()
        });
        assert!(ranges_finding(&report)
            .unwrap()
            .detail
            .starts_with("newuidmap and newgidmap are not found on PATH, so"));
        assert_eq!(
            ranges_finding(&report).unwrap().fix.as_deref(),
            Some(
                "Install newuidmap and newgidmap from your distribution (Debian and Ubuntu: \
                 uidmap; Fedora and RHEL: shadow-utils; Arch: shadow), then: podman system migrate"
            )
        );

        // No range in the files either: one fix for both, in order.
        let empty = parse_subordinate_ids("", "", "alex", None);
        let mut both = linux_report(SINGLE_INFO, Some(empty));
        both.id_map_tools = Some(tools.clone());
        assert_eq!(id_range_problem(&both), Some(IdRangeProblem::Missing));
        let ranges = ranges_finding(&both).unwrap();
        assert_eq!(
            ranges.detail,
            "alex has no range of subordinate user and group IDs in /etc/subuid and /etc/subgid, \
             and newuidmap and newgidmap are not installed, so rootless Podman cannot unpack \
             images or start Sessions"
        );
        assert_eq!(
            ranges.fix.as_deref(),
            Some(
                "sudo apt-get install -y uidmap && sudo usermod --add-subuids 100000-165535 \
                 --add-subgids 100000-165535 alex && podman system migrate"
            )
        );

        // The helpers can map: the range is only not applied yet.
        report.id_map_tools = Some(IdMapTools::default());
        assert_eq!(id_range_problem(&report), Some(IdRangeProblem::NotApplied));
        // A mapped range means the helpers worked: nothing to report.
        let mut mapped = linux_report(LINUX_INFO, Some(ok));
        mapped.id_map_tools = Some(tools);
        assert_eq!(id_range_problem(&mapped), None);
    }

    /// When the package manager's records say a reinstall of the installed
    /// package would leave the helpers as they are, the reinstall is never
    /// the command: the words say why and what to do first, and nothing
    /// runs. A copy no package installed (uidmap not installed, or under
    /// /usr/local) is to be removed or fixed, never "reinstalled".
    #[test]
    fn a_reinstall_that_changes_nothing_and_a_copy_no_package_installed_get_words() {
        let ok = parse_subordinate_ids("alex:100000:65536\n", "alex:100000:65536\n", "alex", None);
        let mut report = linux_report(SINGLE_INFO, Some(ok));
        let apt = "sudo apt-get install --reinstall -y uidmap";
        let keeps = "uidmap installs /usr/bin/newuidmap setuid root, but a dpkg stat override \
                     gives it mode 755 (`dpkg-statoverride --list /usr/bin/newuidmap`), and \
                     reinstalling uidmap applies that mode: remove the override (`sudo \
                     dpkg-statoverride --remove /usr/bin/newuidmap`), then reinstall uidmap \
                     (`sudo apt-get install --reinstall -y uidmap`)";
        let tools = IdMapTools {
            unprivileged: vec!["/usr/bin/newuidmap".to_string()],
            package: Some("uidmap".to_string()),
            package_installed: Some(true),
            install: Some(apt.to_string()),
            reinstall_keeps: vec![keeps.to_string()],
            ..IdMapTools::default()
        };
        assert_eq!(tools.command(), None);
        report.id_map_tools = Some(tools.clone());
        let fix = local_fix(&report, "alex", &IdRangeProblem::NoIdMapTools).unwrap();
        assert_eq!(fix.install, None);
        assert_eq!(fix.manual.as_deref(), Some(keeps));
        assert_eq!(
            ranges_finding(&report).unwrap().fix,
            Some(format!("{keeps}, then: podman system migrate"))
        );
        // Missing helpers, the same: no reinstall.
        report.id_map_tools = Some(IdMapTools {
            missing: vec!["newgidmap".to_string()],
            unprivileged: Vec::new(),
            ..tools.clone()
        });
        assert_eq!(report.id_map_tools.as_ref().unwrap().command(), None);
        // Without such a reason the reinstall is the command again.
        report.id_map_tools = Some(IdMapTools {
            reinstall_keeps: Vec::new(),
            ..tools.clone()
        });
        assert_eq!(
            ranges_finding(&report).unwrap().fix.as_deref(),
            Some("sudo apt-get install --reinstall -y uidmap && podman system migrate")
        );

        // A copy no package installed, uidmap not installed: removed or
        // fixed, with uidmap's install named.
        report.id_map_tools = Some(IdMapTools {
            unprivileged: vec!["/usr/local/bin/newuidmap".to_string()],
            foreign: vec!["/usr/local/bin/newuidmap".to_string()],
            package: Some("uidmap".to_string()),
            package_installed: Some(false),
            install: Some("sudo apt-get install -y uidmap".to_string()),
            ..IdMapTools::default()
        });
        let ranges = ranges_finding(&report).unwrap();
        assert_eq!(
            ranges.fix.as_deref(),
            Some(
                "Remove or fix /usr/local/bin/newuidmap (Podman needs it to be setuid root or to \
                 have its file capability), or take its directory off PATH: no package \
                 installed it, so no reinstall changes it; uidmap, which installs a newuidmap \
                 and newgidmap that can map, is not installed (`sudo apt-get install -y \
                 uidmap`), then: podman system migrate"
            )
        );
        assert!(!ranges.fix.unwrap().contains("Reinstall the package"));
        let fix = local_fix(&report, "alex", &IdRangeProblem::NoIdMapTools).unwrap();
        assert!(fix.manual.is_some() && fix.install.is_none(), "{fix:?}");
        // newgidmap nowhere as well: the install is the command, not named
        // twice.
        report.id_map_tools.as_mut().unwrap().missing = vec!["newgidmap".to_string()];
        assert_eq!(
            ranges_finding(&report).unwrap().fix.as_deref(),
            Some(
                "Remove or fix /usr/local/bin/newuidmap (Podman needs it to be setuid root or to \
                 have its file capability), or take its directory off PATH: no package \
                 installed it, so no reinstall changes it, then: sudo apt-get install -y uidmap \
                 && podman system migrate"
            )
        );
        // Whose package is not known (no package manager answered): both.
        report.id_map_tools = Some(IdMapTools {
            unprivileged: vec!["/usr/bin/newuidmap".to_string()],
            ..IdMapTools::default()
        });
        assert_eq!(
            ranges_finding(&report).unwrap().fix.as_deref(),
            Some(
                "Reinstall the package that provides /usr/bin/newuidmap so that it is setuid root \
                 or has its file capabilities again (Debian and Ubuntu: uidmap; Fedora and RHEL: \
                 shadow-utils; Arch: shadow); if no package installed it (a copy put there by \
                 hand), remove or fix that copy instead, then: podman system migrate"
            )
        );
    }

    /// A helper whose package is installed is missing because its file was
    /// deleted, or cannot map because its setuid bit or file capabilities
    /// were lost: an install changes nothing then (`apt-get install -y
    /// uidmap` and `dnf install -y shadow-utils` say it is already
    /// installed), so the fix is the package manager's reinstall, as the
    /// caller built it. A helper that is there but not on PATH is said to
    /// be so, and no install or reinstall is offered: only PATH changes it.
    #[test]
    fn installed_helpers_are_reinstalled_or_put_on_path_never_installed_again() {
        let ok = parse_subordinate_ids("alex:100000:65536\n", "alex:100000:65536\n", "alex", None);
        let mut report = linux_report(SINGLE_INFO, Some(ok));
        let reinstall = "sudo dnf reinstall -y shadow-utils";
        // Deleted, the package installed.
        report.id_map_tools = Some(IdMapTools {
            missing: vec!["newuidmap".to_string(), "newgidmap".to_string()],
            package: Some("shadow-utils".to_string()),
            package_installed: Some(true),
            install: Some(reinstall.to_string()),
            ..IdMapTools::default()
        });
        assert_eq!(
            id_range_problem(&report),
            Some(IdRangeProblem::NoIdMapTools)
        );
        let ranges = ranges_finding(&report).unwrap();
        assert!(
            ranges.detail.starts_with(
                "newuidmap and newgidmap are missing, though their package shadow-utils is \
                 installed, so rootless Podman maps only alex's own user ID"
            ),
            "{}",
            ranges.detail
        );
        assert_eq!(
            ranges.fix.as_deref(),
            Some("sudo dnf reinstall -y shadow-utils && podman system migrate")
        );
        let fix = local_fix(&report, "alex", &IdRangeProblem::NoIdMapTools).unwrap();
        assert_eq!(fix.manual, None);
        assert_eq!(fix.install.as_deref(), Some(reinstall));
        // One deleted: "its".
        report.id_map_tools.as_mut().unwrap().missing = vec!["newgidmap".to_string()];
        assert!(ranges_finding(&report)
            .unwrap()
            .detail
            .starts_with("newgidmap is missing, though its package shadow-utils is installed"));

        // There but unable to map, the package installed: the reinstall
        // restores the setuid bit (Debian, Ubuntu) or the file
        // capabilities (Fedora, RHEL), so it is run, not said in words.
        let apt = "sudo apt-get install --reinstall -y uidmap";
        report.id_map_tools = Some(IdMapTools {
            unprivileged: vec!["/usr/bin/newuidmap".to_string()],
            package: Some("uidmap".to_string()),
            package_installed: Some(true),
            install: Some(apt.to_string()),
            ..IdMapTools::default()
        });
        let ranges = ranges_finding(&report).unwrap();
        assert!(ranges
            .detail
            .starts_with("/usr/bin/newuidmap is neither setuid nor given file capabilities"));
        assert_eq!(
            ranges.fix.as_deref(),
            Some("sudo apt-get install --reinstall -y uidmap && podman system migrate")
        );
        // A copy the package does not own, first on PATH: a reinstall would
        // change nothing, so the fix is in words and runs nothing.
        report.id_map_tools.as_mut().unwrap().unprivileged =
            vec!["/home/alex/bin/newuidmap".to_string()];
        report.id_map_tools.as_mut().unwrap().foreign =
            vec!["/home/alex/bin/newuidmap".to_string()];
        let ranges = ranges_finding(&report).unwrap();
        assert_eq!(
            ranges.fix.as_deref(),
            Some(
                "Delete /home/alex/bin/newuidmap or take its directory off PATH: it is not the \
                 file uidmap installed, so no reinstall changes it, then: podman system migrate"
            )
        );
        let fix = local_fix(&report, "alex", &IdRangeProblem::NoIdMapTools).unwrap();
        assert!(fix.manual.is_some() && fix.install.is_none(), "{fix:?}");

        // In /usr/bin, which is not on PATH: said so, and no command but
        // the migrate once PATH has it.
        report.id_map_tools = Some(IdMapTools {
            missing: vec!["newuidmap".to_string(), "newgidmap".to_string()],
            off_path: vec![
                "/usr/bin/newuidmap".to_string(),
                "/usr/bin/newgidmap".to_string(),
            ],
            package: Some("uidmap".to_string()),
            package_installed: Some(true),
            install: Some(apt.to_string()),
            ..IdMapTools::default()
        });
        let ranges = ranges_finding(&report).unwrap();
        assert!(
            ranges.detail.starts_with(
                "newuidmap and newgidmap are in /usr/bin, which is not on PATH, so rootless \
                 Podman maps only alex's own user ID"
            ),
            "{}",
            ranges.detail
        );
        assert!(
            !ranges.detail.contains("not installed"),
            "{}",
            ranges.detail
        );
        assert_eq!(
            ranges.fix.as_deref(),
            Some(
                "Add /usr/bin to PATH where Podman and Axocoatl run (Podman looks newuidmap and \
                 newgidmap up on PATH only), then: podman system migrate"
            )
        );
        let fix = local_fix(&report, "alex", &IdRangeProblem::NoIdMapTools).unwrap();
        assert!(fix.manual.is_some() && fix.install.is_none(), "{fix:?}");

        // Through `podman info`'s error on a Mac: never a Linux package fix.
        let mut mac = linux_report(SINGLE_INFO, None);
        mac.runtime = None;
        mac.readiness = PodmanReadiness::Unavailable(
            "Error: command required for rootless mode with multiple IDs: exec: \"newuidmap\": \
             executable file not found in $PATH"
                .to_string(),
        );
        assert_eq!(id_range_problem(&mac), Some(IdRangeProblem::NoIdMapTools));
        mac.machine_platform = true;
        assert_eq!(id_range_problem(&mac), None);
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
