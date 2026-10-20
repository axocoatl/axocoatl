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
//! On Linux the id ranges are read from `/etc/subuid` and `/etc/subgid`
//! ([`read_subordinate_ids`]), not only from `podman info`: `podman info`
//! reports the user namespace Podman's pause process already holds, which
//! keeps the ranges it was started with after a range is removed, while
//! every new `keep-id` container is mapped from the files. Without a range
//! there, crun fails with `write to /proc/sys/net/ipv4/ping_group_range:
//! Invalid argument` (the container has no group 0).

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
/// The Podman Axocoatl is tested with on Linux (Ubuntu 24.04's).
pub const TESTED_LINUX_PODMAN: (u64, u64) = (4, 9);
/// The Podman minor releases Axocoatl is tested with on macOS (6.1 is what
/// Homebrew installs for onboarding today).
pub const TESTED_MACOS_PODMAN: &[(u64, u64)] = &[(5, 8), (6, 1)];

/// "5.8 is", or "5.8 and 6.1 are": the tested macOS releases, for a warning.
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
    /// Linux: the user's ranges in `/etc/subuid` and `/etc/subgid`, which
    /// the caller reads with [`read_subordinate_ids`] (`None`: not read, as
    /// on macOS, or not knowable from the files).
    pub subordinate_ids: Option<SubordinateIds>,
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
        }
    }

    /// The range the `usermod` fix adds: one no line of `/etc/subuid` or
    /// `/etc/subgid` uses, else [`DEFAULT_SUBORDINATE_RANGE`].
    pub fn free_subordinate_range(&self) -> (u64, u64) {
        self.subordinate_ids
            .as_ref()
            .map(|ids| ids.free)
            .unwrap_or(DEFAULT_SUBORDINATE_RANGE)
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
    /// `(first, last)`: [`MIN_SUBORDINATE_IDS`] ids that no line of either
    /// file uses, for the `usermod` fix.
    pub free: (u64, u64),
}

impl SubordinateIds {
    /// Enough of both for rootless Podman and Sessions.
    pub fn enough(&self) -> bool {
        self.uids >= MIN_SUBORDINATE_IDS && self.gids >= MIN_SUBORDINATE_IDS
    }

    /// `no range of subordinate user and group IDs`, or how many there are.
    fn describe(&self, user: &str) -> String {
        if self.uids == 0 && self.gids == 0 {
            format!("{user} has no range of subordinate user and group IDs in /etc/subuid and /etc/subgid")
        } else {
            format!(
                "{user} has {} subordinate user IDs in /etc/subuid and {} group IDs in \
                 /etc/subgid, fewer than {MIN_SUBORDINATE_IDS} of each",
                self.uids, self.gids
            )
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
/// of `used` (`[first, end)` each), as `(first, last)`.
fn free_subordinate_range(used: &[(u64, u64)]) -> (u64, u64) {
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
    // Ids are 32-bit, and 4294967295 is no id.
    let last = first.saturating_add(MIN_SUBORDINATE_IDS - 1);
    if last >= u64::from(u32::MAX) {
        DEFAULT_SUBORDINATE_RANGE
    } else {
        (first, last)
    }
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

/// What is wrong with the user's subordinate ids for rootless Podman.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdRangeProblem {
    /// `/etc/subuid` or `/etc/subgid` gives the user no range or too small
    /// a one (or, when the files were not read, `podman info` maps only
    /// the user's own id): `usermod` adds one.
    Missing,
    /// The files give the user a range, but the user namespace Podman's
    /// pause process holds maps only the user's own id (it started before
    /// the range was added): `podman system migrate` restarts it.
    NotApplied,
}

/// The problem with the user's subordinate ids in `report`, if any. Rootful
/// Podman needs no range.
pub fn id_range_problem(report: &PodmanReport) -> Option<IdRangeProblem> {
    if report.readiness == PodmanReadiness::NotInstalled || report.rootless() == Some(false) {
        return None;
    }
    let single_mapping = report.runtime.as_ref().is_some_and(|runtime| {
        runtime.rootless == Some(true)
            && [runtime.uid_ranges, runtime.gid_ranges]
                .iter()
                .any(|ranges| matches!(ranges, Some(0 | 1)))
    });
    match &report.subordinate_ids {
        Some(ids) if !ids.enough() => Some(IdRangeProblem::Missing),
        Some(_) if single_mapping => Some(IdRangeProblem::NotApplied),
        Some(_) => None,
        None if single_mapping => Some(IdRangeProblem::Missing),
        None => None,
    }
}

/// The failing finding for `problem` in `report`, for `user`.
pub fn id_range_finding(report: &PodmanReport, user: &str, problem: &IdRangeProblem) -> Finding {
    const EFFECT: &str = "so rootless Podman cannot unpack images or start Sessions";
    let (detail, fix) = match (problem, &report.subordinate_ids) {
        (IdRangeProblem::Missing, Some(ids)) => (
            format!("{}, {EFFECT}", ids.describe(user)),
            subordinate_ids_command(user, report.free_subordinate_range()),
        ),
        (IdRangeProblem::Missing, None) => (
            format!(
                "Rootless Podman has no range of subordinate user and group IDs for {user}, \
                 {EFFECT}"
            ),
            subordinate_ids_command(user, report.free_subordinate_range()),
        ),
        (IdRangeProblem::NotApplied, _) => (
            format!(
                "/etc/subuid and /etc/subgid give {user} a range, but rootless Podman still runs \
                 without it (it started before the range was added), {EFFECT}"
            ),
            MIGRATE_FIX.to_string(),
        ),
    };
    Finding::new(Level::Fail, "User ID ranges", detail, Some(fix))
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

/// The command that adds the subordinate ids `(first, last)` for `user`
/// and restarts rootless Podman so it maps them.
pub fn subordinate_ids_command(user: &str, (first, last): (u64, u64)) -> String {
    format!(
        "sudo usermod --add-subuids {first}-{last} --add-subgids {first}-{last} {user} && podman \
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

    if let Some(problem) = id_range_problem(report) {
        list.push(id_range_finding(report, &platform.user, &problem));
    }
    if let Some(runtime) = &report.runtime {
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
            subordinate_ids: None,
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

    /// Podman 6.1, what Homebrew installs for onboarding, is tested on macOS
    /// too: no version warning.
    #[test]
    fn podman_6_1_on_a_mac_is_tested() {
        let mut report = mac_report();
        report.client_version = Some("6.1.4".to_string());
        let findings = findings(&report, &platform(true));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].level, Level::Ok);
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
        assert!(list[1].detail.contains(
            "Podman 6.2.0 has not been tested with Axocoatl on macOS yet (5.8 and 6.1 are)"
        ));
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
        }
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
        assert_eq!(ids.free, (331_072, 396_607));
        let ids = parse_subordinate_ids(subuid, subgid, "alex", None);
        assert_eq!((ids.uids, ids.gids), (1000, 65_536));
        assert!(!ids.enough());
        // Nothing in the files: the standard first range is free.
        let none = parse_subordinate_ids("", "", "alex", Some(1001));
        assert_eq!(
            (none.uids, none.gids, none.free),
            (0, 0, (100_000, 165_535))
        );
        // A range ending inside the default one moves the free range past it.
        let shifted = parse_subordinate_ids("x:50000:60000\n", "", "alex", None);
        assert_eq!(shifted.free, (110_000, 175_535));
        // A line reaching past the last id leaves no free range to name:
        // the default is printed, and nothing overflows.
        let full = parse_subordinate_ids(
            "x:100000:18446744073709551615\n",
            "alex:1:18446744073709551615\n",
            "alex",
            None,
        );
        assert_eq!(full.gids, u64::MAX);
        assert_eq!(full.free, DEFAULT_SUBORDINATE_RANGE);
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

    #[test]
    fn a_small_range_fails_and_a_range_podman_has_not_applied_needs_migrate() {
        let small = parse_subordinate_ids("alex:100000:1000\n", "alex:100000:1000\n", "alex", None);
        let report = linux_report(LINUX_INFO, Some(small));
        let list = findings(&report, &platform(false));
        let ranges = list.iter().find(|f| f.name == "User ID ranges").unwrap();
        assert_eq!(
            ranges.detail,
            "alex has 1000 subordinate user IDs in /etc/subuid and 1000 group IDs in \
             /etc/subgid, fewer than 65536 of each, so rootless Podman cannot unpack images or \
             start Sessions"
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
