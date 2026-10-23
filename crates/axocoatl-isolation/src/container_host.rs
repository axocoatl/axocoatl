//! What the Podman host gives the containers Axocoatl starts: which of their
//! resource caps it can apply, and whether SELinux labels their mounts.
//!
//! Resource caps need cgroup controllers: `--memory` the `memory` controller,
//! `--cpus` the `cpu` one and `--pids-limit` the `pids` one. Rootless Podman
//! has only those the system delegates to the person's user (systemd's
//! `Delegate=` for `user@.service`; cgroup v1 delegates none), and `podman
//! info` lists them. A container gets each cap whose controller is there, so
//! one missing controller drops only its own cap, and the caps it did not get
//! are said and recorded ([`ResourceCaps::not_applied`]).
//!
//! Under SELinux (Fedora, RHEL), a container process may read and write only
//! files labelled for containers, so a bind-mounted repository (labelled
//! `user_home_t`) and the execution supervisor bound from the data root are
//! refused (`Permission denied`, Podman 6.1.4 on Fedora CoreOS, enforcing).
//! Relabelling them (`:z`) would change the labels of the person's own files,
//! so on a Linux host whose Podman reports SELinux enabled the containers run
//! with `--security-opt label=disable`, as Toolbox does: SELinux then
//! confines them as it does the person's own processes, and every other
//! boundary (the user namespace, dropped capabilities, seccomp,
//! `no-new-privileges`, the supervisor's Landlock filter) stays. A Podman
//! machine on macOS mounts the Mac's folders with the `nfs_t` context, which
//! its containers may use, so nothing changes there.

use tokio::process::Command;

use crate::session_sandbox::{SessionSandbox, SANDBOX_READINESS_TIMEOUT};

/// The memory cap of a Session, browser or helper container.
pub const MEMORY_CAP: &str = "2g";
/// Its CPU cap.
pub const CPUS_CAP: &str = "2";
/// Its process cap.
pub const PIDS_CAP: &str = "512";
/// The option that turns SELinux labelling off for one container.
pub const LABEL_DISABLE: &str = "--security-opt=label=disable";

/// Which resource caps a container gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceCaps {
    pub memory: bool,
    pub cpus: bool,
    pub pids: bool,
}

impl ResourceCaps {
    pub const ALL: Self = Self {
        memory: true,
        cpus: true,
        pids: true,
    };
    pub const NONE: Self = Self {
        memory: false,
        cpus: false,
        pids: false,
    };

    /// The caps whose cgroup controller is among `controllers`, as `podman
    /// info` lists them (`memory`, `cpu`, `pids`, ...).
    pub fn from_controllers(controllers: &[String]) -> Self {
        let has = |name: &str| controllers.iter().any(|controller| controller == name);
        Self {
            memory: has("memory"),
            cpus: has("cpu"),
            pids: has("pids"),
        }
    }

    pub fn any(&self) -> bool {
        self.memory || self.cpus || self.pids
    }

    /// The `podman run` options for these caps.
    pub fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.memory {
            args.extend(["--memory".to_string(), MEMORY_CAP.to_string()]);
        }
        if self.cpus {
            args.extend(["--cpus".to_string(), CPUS_CAP.to_string()]);
        }
        if self.pids {
            args.extend(["--pids-limit".to_string(), PIDS_CAP.to_string()]);
        }
        args
    }

    /// The caps left out, each with its cgroup controller:
    /// `memory (2 GiB, controller memory)`.
    pub fn not_applied(&self) -> Vec<String> {
        let mut missing = Vec::new();
        if !self.memory {
            missing.push("memory (2 GiB, cgroup controller memory)".to_string());
        }
        if !self.cpus {
            missing.push("CPU (2 CPUs, cgroup controller cpu)".to_string());
        }
        if !self.pids {
            missing.push("processes (512, cgroup controller pids)".to_string());
        }
        missing
    }
}

/// `true` for every cap, `false` for none.
impl From<bool> for ResourceCaps {
    fn from(all: bool) -> Self {
        if all {
            Self::ALL
        } else {
            Self::NONE
        }
    }
}

/// What the Podman host said about the containers it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerHost {
    /// The caps its cgroup controllers allow.
    pub caps: ResourceCaps,
    /// Its cgroup controllers as Podman listed them, for a message.
    pub controllers: Vec<String>,
    /// Containers it starts run with [`LABEL_DISABLE`].
    pub label_disable: bool,
}

impl ContainerHost {
    /// Read `podman info --format` [`HOST_FORMAT`]'s output, for a Podman
    /// that runs on a Linux host (`linux`) or in a machine.
    pub fn parse(stdout: &str, linux: bool) -> Option<Self> {
        let (controllers, selinux) = stdout.trim().rsplit_once(' ')?;
        let controllers: Vec<String> = match controllers.trim() {
            "null" => Vec::new(),
            listed => serde_json::from_str(listed).ok()?,
        };
        let selinux = match selinux {
            "true" => true,
            "false" => false,
            _ => return None,
        };
        Some(Self {
            caps: ResourceCaps::from_controllers(&controllers),
            controllers,
            label_disable: linux && selinux,
        })
    }
}

/// What `podman info` prints for [`ContainerHost::parse`]: the cgroup
/// controllers as JSON and whether SELinux is enabled (Podman 4.9.3 and
/// 6.1.4).
pub const HOST_FORMAT: &str = "{{json .Host.CgroupControllers}} {{.Host.Security.SELinuxEnabled}}";

/// What this process's Podman host gives containers, asked once and kept
/// (the delegated controllers change only when the person's systemd user
/// manager restarts, SELinux only with a reboot). `None` when Podman did not
/// say: the caller then asks for every cap, as before, and falls back on a
/// cgroup error.
pub async fn container_host() -> Option<ContainerHost> {
    static HOST: std::sync::OnceLock<ContainerHost> = std::sync::OnceLock::new();
    if let Some(host) = HOST.get() {
        return Some(host.clone());
    }
    let mut info = Command::new("podman");
    info.args(["info", "--format", HOST_FORMAT]);
    let output = SessionSandbox::run_bounded_command(info, SANDBOX_READINESS_TIMEOUT)
        .await
        .ok()?;
    if output.timed_out || !output.status.success() {
        tracing::debug!(
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "podman info did not say which resource caps and labels containers get"
        );
        return None;
    }
    let host = ContainerHost::parse(
        &String::from_utf8_lossy(&output.stdout),
        cfg!(target_os = "linux"),
    )?;
    Some(HOST.get_or_init(|| host).clone())
}

/// Put [`LABEL_DISABLE`] into `podman run` arguments, right after their
/// `--security-opt=no-new-privileges`, or right after `run` when they have
/// none.
pub fn insert_label_disable(args: &mut Vec<String>) {
    let at = args
        .iter()
        .position(|arg| arg == "--security-opt=no-new-privileges")
        .map(|index| index + 1)
        .unwrap_or(args.len().min(1));
    args.insert(at, LABEL_DISABLE.to_string());
}

/// The caps a container Axocoatl starts for a Session or beside it (`what`:
/// `the browser container`) gets on this host: each whose cgroup controller
/// Podman lists, or every one when Podman did not say (a cgroup error then
/// drops them). With `required` (`sandbox.require_resource_limits`) a cap
/// the host cannot apply refuses its start, naming it
/// ([`required_caps_missing`]); otherwise the caps it runs without are
/// logged at `warn`, once for each kind of container.
pub async fn caps_for(what: &str, required: bool) -> Result<ResourceCaps, String> {
    match container_host().await {
        Some(host) => caps_on(&host, what, required),
        None => Ok(ResourceCaps::ALL),
    }
}

/// [`caps_for`] on `host`.
fn caps_on(host: &ContainerHost, what: &str, required: bool) -> Result<ResourceCaps, String> {
    if required {
        if let Some(refusal) = required_caps_missing(host.caps, &host.controllers) {
            return Err(refusal);
        }
    }
    let missing = host.caps.not_applied();
    if !missing.is_empty() && first_warning(what) {
        tracing::warn!(
            controllers = %host.controllers.join(", "),
            "this host's Podman cannot apply {what}'s {} cap: starting it without {}",
            missing.join(", "),
            if missing.len() == 1 { "that cap" } else { "those caps" }
        );
    }
    Ok(host.caps)
}

/// Whether this process has not warned about `what`'s caps yet (and now
/// has).
fn first_warning(what: &str) -> bool {
    static WARNED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let mut warned = WARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let first = !warned.iter().any(|said| said == what);
    if first {
        warned.push(what.to_string());
    }
    first
}

/// Why a container that must have every cap (`sandbox.require_resource_limits`)
/// cannot start here, naming what is missing, or `None` when it can.
pub fn required_caps_missing(caps: ResourceCaps, controllers: &[String]) -> Option<String> {
    let missing = caps.not_applied();
    (!missing.is_empty()).then(|| {
        format!(
            "resource limits required but unavailable on this host: Podman's cgroup controllers \
             are [{}], so the container would run without its {} cap. Delegate the missing \
             controllers to your user (systemd `Delegate=cpu cpuset io memory pids` for \
             user@.service, then log in again), or set sandbox.require_resource_limits = false \
             to allow it",
            controllers.join(", "),
            missing.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_cap_needs_its_own_controller() {
        let host = ContainerHost::parse(r#"["cpu","io","memory","pids"] true"#, false).unwrap();
        assert_eq!(host.caps, ResourceCaps::ALL);
        // A machine's SELinux labels the Mac's files for containers already.
        assert!(!host.label_disable);
        // A host that delegates memory and pids, not cpu.
        let host = ContainerHost::parse(r#"["memory","pids"] false"#, true).unwrap();
        assert_eq!(
            host.caps,
            ResourceCaps {
                memory: true,
                cpus: false,
                pids: true
            }
        );
        assert_eq!(
            host.caps.args(),
            ["--memory", "2g", "--pids-limit", "512"].map(String::from)
        );
        assert_eq!(
            host.caps.not_applied(),
            ["CPU (2 CPUs, cgroup controller cpu)".to_string()]
        );
        // Rootless cgroup v1: none.
        let host = ContainerHost::parse("null false", true).unwrap();
        assert_eq!(host.caps, ResourceCaps::NONE);
        assert!(!host.caps.any());
        assert!(host.caps.args().is_empty());
        assert_eq!(host.caps.not_applied().len(), 3);
        assert_eq!(ResourceCaps::from(true), ResourceCaps::ALL);
        assert_eq!(ResourceCaps::from(false), ResourceCaps::NONE);
        assert_eq!(
            ResourceCaps::ALL.args(),
            ["--memory", "2g", "--cpus", "2", "--pids-limit", "512"].map(String::from)
        );
        for unreadable in ["", "true", "[\"cpu\"] maybe", "[cpu] true"] {
            assert_eq!(ContainerHost::parse(unreadable, true), None, "{unreadable}");
        }
    }

    #[test]
    fn selinux_on_a_linux_host_turns_labelling_off_for_the_container() {
        let host = ContainerHost::parse(r#"["cpu","memory","pids"] true"#, true).unwrap();
        assert!(host.label_disable);
        let mut args: Vec<String> = [
            "run",
            "-d",
            "--name",
            "c",
            "-v",
            "/r:/r:rw",
            "--security-opt=no-new-privileges",
            "--cap-drop",
            "NET_RAW",
            "img",
        ]
        .map(String::from)
        .to_vec();
        insert_label_disable(&mut args);
        assert_eq!(args[6], "--security-opt=no-new-privileges");
        assert_eq!(args[7], LABEL_DISABLE);
        assert_eq!(args.last().map(String::as_str), Some("img"));
        let mut bare: Vec<String> = ["run", "img"].map(String::from).to_vec();
        insert_label_disable(&mut bare);
        assert_eq!(bare, ["run", LABEL_DISABLE, "img"].map(String::from));
    }

    #[test]
    fn a_required_cap_that_cannot_apply_is_refused_with_what_is_missing() {
        let controllers = ["memory".to_string(), "pids".to_string()];
        let refusal =
            required_caps_missing(ResourceCaps::from_controllers(&controllers), &controllers)
                .unwrap();
        assert!(refusal.contains("[memory, pids]"), "{refusal}");
        assert!(refusal.contains("CPU (2 CPUs"), "{refusal}");
        assert!(
            refusal.contains("require_resource_limits = false"),
            "{refusal}"
        );
        assert_eq!(required_caps_missing(ResourceCaps::ALL, &[]), None);
    }

    /// A container started for a Session or beside it gets the caps the
    /// host can apply; required, a missing one refuses its start.
    #[test]
    fn a_helper_container_gets_the_caps_the_host_has_or_is_refused() {
        let partial = ContainerHost::parse(r#"["memory","pids"] false"#, true).unwrap();
        let caps = caps_on(&partial, "the browser container", false).unwrap();
        assert_eq!(caps, partial.caps);
        let refusal = caps_on(&partial, "the browser container", true).unwrap_err();
        assert!(refusal.starts_with("resource limits required"), "{refusal}");
        assert!(refusal.contains("CPU (2 CPUs"), "{refusal}");
        let all = ContainerHost::parse(r#"["cpu","memory","pids"] false"#, true).unwrap();
        assert_eq!(caps_on(&all, "SearXNG", true), Ok(ResourceCaps::ALL));
        assert!(first_warning("a kind no other test names"));
        assert!(!first_warning("a kind no other test names"));
    }
}
