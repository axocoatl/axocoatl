//! Step 2: the rootless Podman sandbox every Session and every run uses.
//!
//! macOS: install Podman with Homebrew, create and start the machine (sized
//! by `podman::machine_spec`, applehv, never rootful), start a stopped one,
//! switch a rootful machine (Podman Desktop's default) to rootless, and
//! grow one under 4 GiB. Linux: install Podman with the distribution's
//! package manager, refuse a Podman older than 4.3 (Ubuntu 22.04's 3.4.4),
//! add a range of subordinate ids, and report Landlock. Then one throwaway
//! container proves the setup. Each change is asked for on its own.

use axocoatl_isolation::podman::{
    machine_spec, MachineSpec, PodmanReadiness, MACHINE_DOWNLOAD, MACHINE_PROVIDER,
    MACHINE_START_COMMAND, MIN_MACHINE_MEMORY_MIB, TESTED_MACHINE_MEMORY_MIB,
};
use axocoatl_isolation::podman_report::{
    self, rootless_switch_command, subordinate_ids_command, PodmanReport, MIN_PODMAN_VERSION,
    TESTED_LINUX_PODMAN,
};

use super::detect::Os;
use super::host::Cmd;
use super::ui::joined;
use super::{Onboarding, PodmanChoice};

/// The image Sessions start from, which the proof container runs.
const PROOF_IMAGE: &str = "docker.io/library/alpine:3.20";

/// The machine to create here: [`machine_spec`] for this computer, with
/// `--machine-cpus` and `--machine-memory` applied.
pub(crate) fn spec(o: &Onboarding) -> MachineSpec {
    let mut spec = machine_spec(
        o.facts
            .memory_bytes
            .unwrap_or((TESTED_MACHINE_MEMORY_MIB * 2) << 20),
        o.facts.cpus,
    );
    if let Some(cpus) = o.args.machine_cpus {
        spec.cpus = cpus;
    }
    if let Some(memory) = o.args.machine_memory {
        spec.memory_mib = memory;
    }
    spec
}

async fn reinspect(o: &mut Onboarding) {
    if o.ui.plan() {
        return;
    }
    let program = o
        .host
        .find("podman")
        .map(|path| path.to_string_lossy().into_owned());
    o.facts.podman_path = o.host.find("podman");
    o.facts.podman = match program {
        Some(program) => podman_report::inspect_with(&program).await,
        None => PodmanReport {
            readiness: PodmanReadiness::NotInstalled,
            client_version: None,
            runtime: None,
            machines: Vec::new(),
            default_provider: None,
        },
    };
}

/// Step 2; returns whether the sandbox is ready.
pub(crate) async fn run(o: &mut Onboarding) -> bool {
    if o.args.podman == Some(PodmanChoice::Skip) {
        o.ui.say("  Podman left as it is (--podman skip).");
        let ready = matches!(
            o.facts.podman.readiness,
            PodmanReadiness::Ready | PodmanReadiness::MachineStopped
        );
        if !ready {
            o.step_left(format!(
                "Set up Podman: {}",
                crate::doctor::podman_install_fix(&o.facts.os)
            ));
        }
        return ready;
    }
    // In --plan, each change is assumed made, so the next one is shown.
    let mut assumed = 0;
    for _ in 0..6 {
        let readiness = if o.ui.plan() && assumed > 0 {
            match assumed {
                1 if o.facts.os.is_macos() => PodmanReadiness::MachineMissing,
                _ => PodmanReadiness::Ready,
            }
        } else {
            o.facts.podman.readiness.clone()
        };
        match readiness {
            PodmanReadiness::Ready => break,
            PodmanReadiness::NotInstalled => {
                if !install(o).await {
                    return false;
                }
                assumed = 1;
            }
            PodmanReadiness::MachineMissing => {
                if !create_machine(o).await {
                    return false;
                }
                assumed = 2;
            }
            PodmanReadiness::MachineStopped => {
                let name = machine_name(&o.facts.podman);
                let start = Cmd::new("podman", &["machine", "start"]);
                if !o.ui.consent(
                    &format!("Start {name} now? (`{MACHINE_START_COMMAND}`)"),
                    true,
                    false,
                ) {
                    o.ui.ok(format!(
                        "{name} stays stopped; it starts on first use, when a Session or an \
                         axocoatl run needs it"
                    ));
                    break;
                }
                if let Err(error) = o.ui.execute(&o.host, &start).await {
                    o.step_left(format!("{error}; start it with `{MACHINE_START_COMMAND}`"));
                    return false;
                }
                assumed = 2;
            }
            PodmanReadiness::Unavailable(error) => {
                o.step_left(format!(
                    "Podman does not answer ({}): check `podman system connection list` and \
                     `podman info`",
                    error.lines().next().unwrap_or_default()
                ));
                return false;
            }
        }
        reinspect(o).await;
    }
    if !o.ui.plan()
        && !matches!(
            o.facts.podman.readiness,
            PodmanReadiness::Ready | PodmanReadiness::MachineStopped
        )
    {
        o.step_left(format!(
            "Podman is not ready yet: {}",
            o.facts.podman.readiness.summary()
        ));
        return false;
    }
    let mut ready = true;
    if !o.ui.plan() {
        ready &= rootless(o).await;
        ready &= machine_memory(o).await;
        ready &= version(o);
        ready &= subordinate_ids(o).await;
        ready &= landlock(o);
    }
    if ready {
        ready = proof(o).await;
    }
    ready
}

fn machine_name(report: &PodmanReport) -> String {
    report
        .machine()
        .map(|machine| machine.name.clone())
        .unwrap_or_else(|| "podman-machine-default".to_string())
}

async fn install(o: &mut Onboarding) -> bool {
    match o.facts.os.clone() {
        Os::MacOs { apple_silicon, .. } => {
            if !apple_silicon {
                o.step_left(crate::doctor::podman_install_fix(&o.facts.os));
                return false;
            }
            if o.facts.brew.is_none() {
                o.step_left(
                    "Install Podman with the installer from https://podman.io (or install \
                     Homebrew and run `brew install podman`), then run `axocoatl onboard` again",
                );
                return false;
            }
            let cmd = Cmd::new("brew", &["install", "podman"]);
            if !o.ui.consent(
                "Axocoatl runs every Session in a rootless Podman sandbox. Install Podman now \
                 with `brew install podman` (about 100 MB)?",
                true,
                false,
            ) {
                o.step_left("Install Podman: brew install podman");
                return false;
            }
            if let Err(error) = o.ui.execute(&o.host, &cmd).await {
                o.step_left(format!(
                    "{error}; install Podman, then run `axocoatl onboard` again"
                ));
                return false;
            }
            true
        }
        Os::Linux { release, wsl, .. } => {
            if release.podman_too_old() {
                o.step_left(
                    "Ubuntu 22.04 ships Podman 3.4.4. Axocoatl runs need Podman 4.3 or later \
                     (4.9 tested). Use Ubuntu 24.04 or newer.",
                );
                return false;
            }
            let Some(manager) = release.package_manager() else {
                o.step_left(
                    "Install Podman 4.9 or later (https://podman.io/docs/installation), then run \
                     `axocoatl onboard` again",
                );
                return false;
            };
            let cmd = manager.install(&["podman"]);
            let wsl_note = if wsl.is_some() {
                " (inside this WSL distribution)"
            } else {
                ""
            };
            if !o.ui.consent(
                &format!(
                    "Axocoatl runs every Session in a rootless Podman sandbox. Install Podman now \
                     with `{}`{wsl_note}? This asks for your password.",
                    cmd.display()
                ),
                true,
                true,
            ) {
                o.step_left(format!("Install Podman: {}", cmd.display()));
                return false;
            }
            if let Err(error) = o.ui.execute(&o.host, &cmd).await {
                o.step_left(format!(
                    "{error}; install Podman, then run `axocoatl onboard` again"
                ));
                return false;
            }
            true
        }
    }
}

/// The containers.conf whose `[machine] provider` pins new machines.
fn containers_conf(o: &Onboarding) -> std::path::PathBuf {
    o.facts
        .xdg_config_home
        .clone()
        .unwrap_or_else(|| o.host.home.join(".config"))
        .join("containers")
        .join("containers.conf")
}

async fn create_machine(o: &mut Onboarding) -> bool {
    let spec = spec(o);
    // The podman.io package defaults to libkrun; Axocoatl is tested with
    // applehv, which Homebrew's Podman uses.
    if let Some(provider) = o
        .facts
        .podman
        .default_provider
        .clone()
        .filter(|provider| provider != MACHINE_PROVIDER)
    {
        let conf = containers_conf(o);
        if conf.exists() {
            o.ui.warn(format!(
                "Podman creates machines with {provider} here, which Axocoatl has not been tested \
                 with; to use {MACHINE_PROVIDER}, add `[machine]` and `provider = \
                 \"{MACHINE_PROVIDER}\"` to {}",
                conf.display()
            ));
        } else if o.ui.consent(
            &format!(
                "Podman creates machines with {provider} here, which Axocoatl has not been tested \
                 with. Use {MACHINE_PROVIDER} (the tested provider) by writing `[machine] \
                 provider = \"{MACHINE_PROVIDER}\"` to {}?",
                conf.display()
            ),
            true,
            false,
        ) {
            if o.ui.plan() {
                o.ui.say(format!("    would write {}", conf.display()));
            } else if let Err(error) = conf
                .parent()
                .map(std::fs::create_dir_all)
                .unwrap_or(Ok(()))
                .and_then(|()| {
                    std::fs::write(
                        &conf,
                        format!(
                            "# Written by axocoatl onboard: the provider Axocoatl is tested with.\n\
                             [machine]\nprovider = \"{MACHINE_PROVIDER}\"\n"
                        ),
                    )
                })
            {
                o.ui.warn(format!("Could not write {}: {error}", conf.display()));
            } else {
                o.host.log(&format!("wrote {}", conf.display()));
            }
        }
    }
    let init = Cmd {
        args: spec.init_args(None),
        ..Cmd::new("podman", &[])
    };
    let start = Cmd::new("podman", &["machine", "start"]);
    let untested = if spec.tested() {
        String::new()
    } else {
        format!(
            " This size has not been tested yet ({} CPUs / {} is).",
            axocoatl_isolation::podman::TESTED_MACHINE_CPUS,
            axocoatl_isolation::podman::gib_text(TESTED_MACHINE_MEMORY_MIB)
        )
    };
    let fits = o
        .facts
        .memory_bytes
        .is_none_or(|memory| spec.memory_mib << 20 <= memory / 2);
    if !o.ui.consent(
        &format!(
            "Create the Podman machine your Sessions run in?\n      {}\n      {}\n    This \
             downloads {MACHINE_DOWNLOAD} and uses up to {} of memory while it runs.{untested}",
            init.display(),
            start.display(),
            spec.memory_text()
        ),
        fits,
        false,
    ) {
        o.step_left(format!(
            "Create the Podman machine: {} && {}",
            init.display(),
            start.display()
        ));
        return false;
    }
    if let Err(error) = o.ui.execute_all(&o.host, &[init, start]).await {
        o.step_left(format!("{error}; see `podman machine list`"));
        return false;
    }
    true
}

async fn rootless(o: &mut Onboarding) -> bool {
    if o.facts.podman.rootless() != Some(false) {
        return true;
    }
    if !o.facts.os.is_macos() {
        o.step_left(
            "Podman runs containers as root here; run Podman and Axocoatl as your normal user, \
             not as root",
        );
        return false;
    }
    let name = machine_name(&o.facts.podman);
    let commands = vec![
        Cmd::new("podman", &["machine", "stop"]),
        Cmd::new("podman", &["machine", "set", "--rootful=false"]),
        Cmd::new("podman", &["machine", "start"]),
    ];
    if o.ui.consent(
        &format!(
            "{name} runs containers as root. Axocoatl runs (fix, qa, audit) need rootless Podman \
             and would stop with exit 5. Switch it to rootless? This runs {}, and affects other \
             tools that use this machine.",
            joined(&commands)
        ),
        false,
        false,
    ) {
        if let Err(error) = o.ui.execute_all(&o.host, &commands).await {
            o.step_left(format!(
                "{error}; switch it with: {}",
                rootless_switch_command(&name)
            ));
            return false;
        }
        reinspect(o).await;
        if o.facts.podman.rootless() == Some(false) {
            o.step_left(format!(
                "{name} still runs as root: {}",
                rootless_switch_command(&name)
            ));
            return false;
        }
        return true;
    }
    let spec = spec(o);
    o.step_left(format!(
        "Podman runs as root, which Axocoatl runs refuse. Either switch {name} to rootless ({}), \
         or keep it and give Axocoatl its own rootless machine: {} && podman machine start \
         axocoatl, then `export CONTAINER_CONNECTION=axocoatl` in the shell that starts \
         Axocoatl (`axocoatl service install` records it; only one machine can run at a time)",
        rootless_switch_command(&name),
        Cmd {
            args: spec.init_args(Some("axocoatl")),
            ..Cmd::new("podman", &[])
        }
        .display()
    ));
    false
}

async fn machine_memory(o: &mut Onboarding) -> bool {
    if !o.facts.os.is_macos() {
        return true;
    }
    let Some(machine) = o.facts.podman.machine().cloned() else {
        return true;
    };
    let Some(memory) = machine
        .memory_mib
        .filter(|memory| *memory < MIN_MACHINE_MEMORY_MIB)
    else {
        return true;
    };
    let target = spec(o).memory_mib;
    let target_text = target.to_string();
    let commands = vec![
        Cmd::new("podman", &["machine", "stop"]),
        Cmd::new("podman", &["machine", "set", "--memory", &target_text]),
        Cmd::new("podman", &["machine", "start"]),
    ];
    if o.ui.consent(
        &format!(
            "{} has {}; Sessions take up to 2 GiB each. Give it {}? This runs {}, and affects \
             other tools that use this machine.",
            machine.name,
            axocoatl_isolation::podman::gib_text(memory),
            axocoatl_isolation::podman::gib_text(target),
            joined(&commands)
        ),
        false,
        false,
    ) {
        if let Err(error) = o.ui.execute_all(&o.host, &commands).await {
            o.ui.warn(error);
        }
        reinspect(o).await;
    } else {
        o.ui.warn(format!(
            "{} keeps {}; it holds few Sessions at once",
            machine.name,
            axocoatl_isolation::podman::gib_text(memory)
        ));
    }
    true
}

fn version(o: &mut Onboarding) -> bool {
    let Some((major, minor, _)) = o.facts.podman.client_numbers() else {
        return true;
    };
    let version = o.facts.podman.client_version.clone().unwrap_or_default();
    if (major, minor) < MIN_PODMAN_VERSION {
        let note = o
            .facts
            .os
            .release()
            .filter(|release| release.podman_too_old())
            .map(|_| " Ubuntu 22.04 ships Podman 3.4.4; use Ubuntu 24.04 or newer.")
            .unwrap_or_default();
        o.step_left(format!(
            "Podman {version} is too old: Axocoatl runs need Podman 4.3 or later (4.9 tested).{note}"
        ));
        return false;
    }
    if !o.facts.os.is_macos() && (major, minor) < TESTED_LINUX_PODMAN {
        o.ui.warn(format!(
            "Podman {version} has not been tested with Axocoatl (4.9 is)"
        ));
    }
    true
}

async fn subordinate_ids(o: &mut Onboarding) -> bool {
    let Some(runtime) = o.facts.podman.runtime.clone() else {
        return true;
    };
    let missing = runtime.rootless == Some(true)
        && [runtime.uid_ranges, runtime.gid_ranges]
            .iter()
            .any(|ranges| matches!(ranges, Some(0 | 1)));
    if !missing {
        return true;
    }
    let user = o.facts.user.clone();
    let commands = vec![
        Cmd::sudo(
            "usermod",
            &[
                "--add-subuids",
                "100000-165535",
                "--add-subgids",
                "100000-165535",
                &user,
            ],
        ),
        Cmd::new("podman", &["system", "migrate"]),
    ];
    if !o.ui.consent(
        &format!(
            "Rootless Podman needs a range of user IDs for {user}. Add one with `{}`?",
            subordinate_ids_command(&user)
        ),
        true,
        true,
    ) {
        o.step_left(format!(
            "Add a range of user IDs for {user}: {}",
            subordinate_ids_command(&user)
        ));
        return false;
    }
    if let Err(error) = o.ui.execute_all(&o.host, &commands).await {
        o.step_left(format!("{error}; run {}", subordinate_ids_command(&user)));
        return false;
    }
    reinspect(o).await;
    true
}

fn landlock(o: &mut Onboarding) -> bool {
    let Os::Linux {
        release,
        wsl,
        systemd,
    } = o.facts.os.clone()
    else {
        return true;
    };
    if wsl.is_some() && !systemd {
        o.ui.warn(
            "This WSL distribution runs without systemd: the Always-On Service is not \
             available, and `axocoatl dev` works",
        );
    }
    if o.facts
        .podman
        .runtime
        .as_ref()
        .is_some_and(|runtime| runtime.selinux == Some(true))
    {
        o.ui.warn(
            "SELinux is enabled; Sessions under SELinux enforcing have not been verified yet",
        );
    }
    let item = crate::doctor::landlock_item(o.facts.landlock_abi, Some(&release), wsl.is_some());
    match item.status {
        crate::doctor::Status::Ok => {
            o.ui.ok(&item.detail);
            true
        }
        crate::doctor::Status::Warn => {
            o.ui.warn(&item.detail);
            true
        }
        crate::doctor::Status::Fail => {
            o.step_left(format!("{}: {}", item.detail, item.fix.unwrap_or_default()));
            false
        }
    }
}

/// One throwaway container with a hardened Session's shape: the user
/// mapped in with keep-id, no network, no capabilities, `--rm
/// --pull=never`.
async fn proof(o: &mut Onboarding) -> bool {
    if o.facts.podman.readiness == PodmanReadiness::MachineStopped && !o.ui.plan() {
        o.ui.warn("The sandbox was not checked with a container: the machine is stopped");
        return true;
    }
    let pull = Cmd::new("podman", &["pull", PROOF_IMAGE]);
    let run_args = [
        "run",
        "--rm",
        "--pull=never",
        "--userns=keep-id:uid=1000,gid=1000",
        "--user",
        "1000:1000",
        "--network=none",
        "--cap-drop=all",
        "--security-opt=no-new-privileges",
        PROOF_IMAGE,
        "id",
        "-u",
    ];
    if !o.ui.consent(
        &format!(
            "Check the sandbox with one throwaway container? This pulls {PROOF_IMAGE} (about 4 \
             MB), the image Sessions start from, and runs it rootless with your user mapped in \
             and no network."
        ),
        true,
        false,
    ) {
        o.ui.warn("The sandbox was not checked with a container");
        return true;
    }
    if o.ui.plan() {
        o.ui.say(format!("    would run: {}", pull.display()));
        o.ui.say(format!("    would run: podman {}", run_args.join(" ")));
        return true;
    }
    if let Err(error) = o.host.run(&pull).await {
        o.step_left(format!(
            "{error}: Podman could not pull the Session image (the log has Podman's output)"
        ));
        return false;
    }
    let ran = o.host.probe("podman", &run_args).await;
    match ran {
        Some(probe) if probe.success && probe.stdout.trim() == "1000" => {
            o.ui.ok(sandbox_ready_line(o));
            true
        }
        Some(probe) => {
            let reason = probe
                .stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("it printed nothing")
                .trim()
                .to_string();
            let fix = if reason.contains("subuid") || reason.contains("insufficient UIDs") {
                subordinate_ids_command(&o.facts.user)
            } else if reason.contains("keep-id") {
                "upgrade Podman to 4.9 or later".to_string()
            } else {
                "see the onboarding log for Podman's output".to_string()
            };
            o.step_left(format!(
                "The sandbox check container failed ({reason}): {fix}"
            ));
            false
        }
        None => {
            o.step_left("The sandbox check container did not run: see the onboarding log");
            false
        }
    }
}

/// `Sandbox ready: Podman 5.8.2, applehv, rootless, 6 CPUs / 8 GiB`.
fn sandbox_ready_line(o: &Onboarding) -> String {
    let report = &o.facts.podman;
    let mut parts = vec![format!(
        "Podman {}",
        report.client_version.as_deref().unwrap_or("?")
    )];
    if let Some(machine) = report.machine().filter(|_| o.facts.os.is_macos()) {
        parts.push(machine.describe());
    } else if report.rootless() == Some(true) {
        parts.push("rootless".to_string());
    }
    if let Some(abi) = o.facts.landlock_abi {
        parts.push(format!("Landlock ABI {abi}"));
    }
    format!("Sandbox ready: {}", parts.join(", "))
}
