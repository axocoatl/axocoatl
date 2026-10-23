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
    MIN_MACHINE_MEMORY_MIB, TESTED_MACHINE_MEMORY_MIB,
};
use axocoatl_isolation::podman_report::{
    self, rootless_switch_command, IdRangeProblem, LocalIdFix, PodmanReport, MIN_PODMAN_VERSION,
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
        Some(program) => {
            podman_report::inspect_through(&program, o.host.podman_connection.clone()).await
        }
        None => PodmanReport::not_installed(),
    };
    crate::doctor::read_id_ranges(
        &mut o.facts.podman,
        &o.host,
        &o.host.root,
        &o.facts.os,
        &o.facts.user,
    )
    .await;
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
                crate::doctor::podman_install_fix(&o.facts.os, o.facts.brew.is_some())
            ));
        }
        return ready;
    }
    // In --plan, each change is assumed made, so the next one is shown.
    let mut assumed = 0;
    // The id fix already ran (or, in --plan, was shown) for a `podman info`
    // that failed for want of the helpers.
    let mut ids_done = false;
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
                let start = machine_cmd(&["start"], &name);
                if !o.ui.consent(
                    &format!("Start {name} now? (`{}`)", start.display()),
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
                    o.step_left(format!("{error}; start it with `{}`", start.display()));
                    return false;
                }
                assumed = 2;
            }
            // `podman info` fails here for want of newuidmap or newgidmap
            // (a range in the files, the helpers missing): install them,
            // asked like the Podman install, instead of a step that says
            // to read `podman info` (whose message says to run `podman
            // system migrate`, which alone changes nothing).
            PodmanReadiness::Unavailable(error)
                if o.facts.podman.runs_here()
                    && podman_report::id_map_tools_failure(&error).is_some() =>
            {
                if !subordinate_ids(o).await {
                    return false;
                }
                ids_done = true;
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
    // Each check only reads what Step 0 found and asks through the Ui, so
    // --plan prints the question and the commands it would run.
    let mut ready = on_path(o);
    ready &= rootless(o).await;
    ready &= machine_memory(o).await;
    ready &= version(o);
    if !ids_done {
        ready &= subordinate_ids(o).await;
    }
    ready &= landlock(o);
    if ready {
        ready = proof(o).await;
    }
    ready
}

/// The daemon finds Podman on PATH, as `axocoatl dev` and the service run
/// it: a Podman onboarding found only in Homebrew's or the installer's
/// directory is a step left until that directory is on PATH.
fn on_path(o: &mut Onboarding) -> bool {
    if o.ui.plan() || o.host.find_on_path("podman").is_some() {
        return true;
    }
    let Some(found) = o.host.find("podman") else {
        return true;
    };
    let directory = found
        .parent()
        .map(|directory| directory.display().to_string())
        .unwrap_or_default();
    let how = if directory == "/opt/homebrew/bin" || directory == "/usr/local/bin" {
        format!(" (for Homebrew: add `eval \"$({directory}/brew shellenv)\"` to ~/.zprofile)")
    } else {
        String::new()
    };
    o.step_left(format!(
        "Podman is installed at {}, but {directory} is not on your PATH, so `axocoatl dev` and \
         every run would not find it: add {directory} to PATH{how}, open a new terminal and run \
         `axocoatl onboard` again",
        found.display()
    ));
    false
}

/// The machine onboarding starts, stops or changes.
fn machine_name(report: &PodmanReport) -> String {
    report
        .machine_in_use()
        .or_else(|| report.machine())
        .map(|machine| machine.name.clone())
        .unwrap_or_else(|| "podman-machine-default".to_string())
}

/// `podman machine <verb…> [name]`: every machine command names the
/// machine (Podman otherwise acts on `podman-machine-default`).
fn machine_cmd(verb: &[&str], name: &str) -> Cmd {
    let mut args = vec!["machine"];
    args.extend_from_slice(verb);
    if name != "podman-machine-default" {
        args.push(name);
    }
    Cmd::new("podman", &args)
}

/// Why onboarding does not change the machine: Podman is reached through
/// `CONTAINER_HOST`, or a `CONTAINER_CONNECTION` that is not a machine.
fn not_our_machine(o: &Onboarding) -> Option<String> {
    if o.facts.podman.machine_in_use().is_some() {
        return None;
    }
    o.facts.podman.connection.describe()
}

async fn install(o: &mut Onboarding) -> bool {
    match o.facts.os.clone() {
        Os::MacOs { apple_silicon, .. } => {
            if !apple_silicon {
                o.step_left(crate::doctor::podman_install_fix(
                    &o.facts.os,
                    o.facts.brew.is_some(),
                ));
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

/// The Podman connections already defined (`podman system connection
/// list`), by name.
pub(crate) fn connection_names(json: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|connection| connection.get("Name").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect()
}

async fn create_machine(o: &mut Onboarding) -> bool {
    // A machine is created only where Podman has nothing defined: a
    // connection that exists but does not answer is a setup to fix, not to
    // replace.
    if !o.ui.plan() {
        let existing = o
            .host
            .probe(
                "podman",
                &["system", "connection", "list", "--format", "json"],
            )
            .await
            .filter(|probe| probe.success)
            .map(|probe| connection_names(&probe.stdout))
            .unwrap_or_default();
        if !existing.is_empty() {
            o.step_left(format!(
                "Podman has connections ({}) but no machine answers: check `podman system \
                 connection list` and `podman machine list`, then run `axocoatl onboard` again",
                existing.join(", ")
            ));
            return false;
        }
    }
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
    if let Some(connection) = not_our_machine(o) {
        o.step_left(format!(
            "Podman runs containers as root, which Axocoatl runs refuse, and it is reached \
             through {connection}: point that at a rootless Podman machine, or unset it to use \
             your own machine (onboarding does not change a Podman it reaches this way)"
        ));
        return false;
    }
    let name = machine_name(&o.facts.podman);
    let commands = vec![
        machine_cmd(&["stop"], &name),
        machine_cmd(&["set", "--rootful=false"], &name),
        machine_cmd(&["start"], &name),
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
    let init = Cmd {
        args: spec.init_args(Some(OWN_MACHINE)),
        ..Cmd::new("podman", &[])
    };
    let exists = o
        .facts
        .podman
        .machines
        .iter()
        .any(|machine| machine.name == OWN_MACHINE);
    // Step 2d: keep that machine as it is and give Axocoatl its own. Only a
    // person answers this (--yes switched the machine above).
    let created = exists
        || (o.ui.mode != super::ui::Mode::Yes
            && o.ui.consent(
                &format!(
                    "Keep {name} as it is and create a separate rootless machine for Axocoatl \
                     instead? This runs `{}`, which downloads {MACHINE_DOWNLOAD} and uses up to \
                     {} of memory while it runs. Only one machine runs at a time: Sessions and \
                     runs start `{OWN_MACHINE}` when {name} is stopped.",
                    init.display(),
                    spec.memory_text()
                ),
                false,
                false,
            )
            && match o.ui.execute(&o.host, &init).await {
                Ok(()) => true,
                Err(error) => {
                    o.ui.fail(error);
                    false
                }
            });
    if created {
        o.step_left(format!(
            "Use Axocoatl's own rootless machine: stop {name} (`{}`), then start Axocoatl from a \
             shell with `export CONTAINER_CONNECTION={OWN_MACHINE}` (`axocoatl service install` \
             records it; Sessions and runs start the machine), and run `axocoatl onboard` again \
             from that shell",
            machine_cmd(&["stop"], &name).display()
        ));
        return false;
    }
    o.step_left(format!(
        "Podman runs as root, which Axocoatl runs refuse. Either switch {name} to rootless ({}), \
         or keep it and give Axocoatl its own rootless machine: {} && podman machine start \
         {OWN_MACHINE}, then `export CONTAINER_CONNECTION={OWN_MACHINE}` in the shell that \
         starts Axocoatl (`axocoatl service install` records it; only one machine can run at a \
         time)",
        rootless_switch_command(&name),
        init.display()
    ));
    false
}

/// The rootless machine onboarding offers when the one in use is rootful
/// and stays so.
pub(crate) const OWN_MACHINE: &str = "axocoatl";

async fn machine_memory(o: &mut Onboarding) -> bool {
    if !o.facts.os.is_macos() {
        return true;
    }
    let Some(machine) = o.facts.podman.machine_in_use().cloned() else {
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
        machine_cmd(&["stop"], &machine.name),
        machine_cmd(&["set", "--memory", &target_text], &machine.name),
        machine_cmd(&["start"], &machine.name),
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
    if !o.facts.os.is_macos() && (major, minor) != TESTED_LINUX_PODMAN {
        o.ui.warn(podman_report::linux_untested(&version));
    }
    true
}

/// Linux: a range of subordinate user and group ids that rootless Podman
/// maps. When Podman runs on this computer, onboarding adds a missing
/// range to `/etc/subuid` and `/etc/subgid` with `sudo usermod`, installs a
/// missing `newuidmap` and `newgidmap` with the package manager (each only
/// with consent, and under `--yes` only with `--allow-sudo`), and restarts
/// rootless Podman with `podman system migrate`; a range Podman has not
/// picked up yet needs only the migrate. Step 2 also comes here when
/// `podman info` fails for want of the helpers. For a Podman reached through
/// `CONTAINER_HOST`, `CONTAINER_CONNECTION` or a remote connection it runs
/// nothing: that Podman's range is set where it runs, and Podman refuses
/// `system migrate` through an override.
async fn subordinate_ids(o: &mut Onboarding) -> bool {
    let Some(problem) = podman_report::id_range_problem(&o.facts.podman) else {
        return true;
    };
    let user = o.facts.user.clone();
    let finding = podman_report::id_range_finding(&o.facts.podman, &user, &problem);
    let fix_text = finding.fix.clone().unwrap_or_default();
    // Elsewhere, or something only the person can do (choose a range,
    // reinstall a helper): a step left, nothing run.
    let commands = podman_report::local_fix(&o.facts.podman, &user, &problem)
        .filter(|fix| fix.manual.is_none())
        .and_then(|fix| {
            id_fix_commands(o, &fix)
                .map(|commands| (commands, fix.uses_sudo(), fix.install.is_some()))
        });
    let Some((commands, sudo, install)) = commands else {
        o.step_left(format!("{}: {fix_text}", finding.detail));
        return false;
    };
    // The command reinstalls their package (it is installed, or rpm did
    // not say, where shadow-utils always is).
    let reinstall = commands
        .first()
        .filter(|_| install)
        .is_some_and(|command| command.args.iter().any(|arg| arg.contains("reinstall")));
    let install_them = if reinstall {
        "Reinstall their package"
    } else {
        "Install them"
    };
    let question = match problem {
        IdRangeProblem::NotApplied => format!(
            "{}. Restart rootless Podman with `podman system migrate`? It stops your running \
             containers.",
            finding.detail
        ),
        IdRangeProblem::NoIdMapTools => format!(
            "{}. {install_them} with `{fix_text}`? `podman system migrate` stops your running \
             containers.",
            finding.detail
        ),
        IdRangeProblem::Missing | IdRangeProblem::Elsewhere => format!(
            "{}. {} with `{fix_text}`? `podman system migrate` stops your running containers.",
            finding.detail,
            if install {
                format!("{install_them} and add a range")
            } else {
                "Add a range".to_string()
            }
        ),
    };
    if !o.ui.consent(&question, true, sudo) {
        o.step_left(format!("{}: {fix_text}", finding.detail));
        return false;
    }
    // The install (or reinstall) first, on its own: when it succeeds and
    // the helpers are still unusable, it did not fix them, and offering it
    // again would only repeat it.
    let (install_command, rest) = match install {
        true => (commands.first().cloned(), &commands[1..]),
        false => (None, &commands[..]),
    };
    if let Some(command) = &install_command {
        let output = match o.ui.execute_showing_output(&o.host, command).await {
            Ok(output) => output,
            Err(error) => {
                // `… failed (exit 1): <what it said last>`
                let (failed, said) = match error.split_once("): ") {
                    Some((failed, said)) => (format!("{failed})"), said.to_string()),
                    None => (error.clone(), String::new()),
                };
                o.step_left(install_failed(command, &failed, &said, &fix_text, rest));
                return false;
            }
        };
        // apt-get exits 0 when it cannot download the package to
        // reinstall it, and changes nothing.
        if output.contains("is not possible, it cannot be downloaded") {
            let said = output
                .lines()
                .find(|line| line.contains("is not possible, it cannot be downloaded"))
                .unwrap_or_default()
                .trim()
                .to_string();
            o.step_left(install_failed(
                command,
                &format!("`{}` did not reinstall it", command.display()),
                &said,
                &fix_text,
                rest,
            ));
            return false;
        }
        if !o.ui.plan() {
            let now = crate::doctor::id_map_tools(&o.host, &o.host.root, &o.facts.os).await;
            if !now.usable() {
                o.step_left(install_did_not_fix(
                    &command.display(),
                    &now.describe(),
                    &now.reinstall_keeps,
                ));
                return false;
            }
        }
    }
    let ran = o.ui.execute_all(&o.host, rest).await;
    if let (Err(error), None) = (&ran, &install_command) {
        o.step_left(format!("{error}; run {fix_text}"));
        return false;
    }
    reinspect(o).await;
    if o.ui.plan() {
        return true;
    }
    // After an install that ran, what is left now, and never that install
    // again when Podman still fails for want of the helpers.
    let left = podman_report::id_range_problem(&o.facts.podman).map(|problem| {
        let finding = podman_report::id_range_finding(&o.facts.podman, &user, &problem);
        let again =
            podman_report::local_fix(&o.facts.podman, &user, &problem).and_then(|fix| fix.install);
        // Any install of the helpers' package again (the same command, or
        // its other form once PATH has them but Podman still fails), so
        // never one.
        match (install_command.as_ref().map(Cmd::display), again) {
            (Some(installed), Some(_)) => {
                let keeps = o
                    .facts
                    .podman
                    .id_map_tools
                    .as_ref()
                    .map(|tools| tools.reinstall_keeps.clone())
                    .unwrap_or_default();
                install_did_not_fix(&installed, &finding.detail, &keeps)
            }
            _ => format!("{}: {}", finding.detail, finding.fix.unwrap_or_default()),
        }
    });
    match (ran, left) {
        (Ok(()), None) => true,
        (Err(error), None) => {
            o.step_left(error);
            false
        }
        (Ok(()), Some(left)) => {
            o.step_left(left);
            false
        }
        (Err(error), Some(left)) => {
            o.step_left(format!("{error}; {}", lowercase_first(&left)));
            false
        }
    }
}

/// `text` with its first letter in lower case, after a `;`.
fn lowercase_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if !text.starts_with('`') => first.to_lowercase().chain(chars).collect(),
        _ => text.to_string(),
    }
}

/// The step left when `ran`, the install or reinstall of the id-map
/// helpers' package, succeeded and the helpers still cannot map (`still`
/// says why): in words, without the same command. With what keeps a
/// reinstall from fixing them, when the package manager's records say
/// (`keeps`, [`IdMapTools::reinstall_keeps`], which doctor and a later run
/// read too, so neither offers it again).
///
/// [`IdMapTools::reinstall_keeps`]: podman_report::IdMapTools::reinstall_keeps
fn install_did_not_fix(ran: &str, still: &str, keeps: &[String]) -> String {
    match keeps {
        [] => format!(
            "`{ran}` ran, but did not fix this: {still}. Running it again would not help: find \
             where this distribution puts newuidmap and newgidmap, make sure they are on PATH and \
             are setuid root or have file capabilities, then run `podman system migrate`"
        ),
        // Each starts with a package's or program's name, in lower case.
        keeps => format!(
            "`{ran}` ran, but did not fix this: {still}. Why: {}, then run `podman system \
             migrate`",
            keeps.join("; ")
        ),
    }
}

/// The step left when `command`, the install or reinstall of the id-map
/// helpers' package, failed or changed nothing (`error`), the package
/// manager's last words being `said`: what failed, and what to do where
/// the same command cannot work as it is. dnf cannot reinstall a build
/// that no enabled repository has any more (Fedora's updates repository
/// keeps only the newest build): `Packages for argument 'shadow-utils'
/// installed, but not available.` (exit 1, dnf5 5.2.18 in fedora:42). apt
/// cannot download a package its lists do not have, or no longer have:
/// `Reinstallation of uidmap is not possible, it cannot be downloaded.`
/// (exit 0, apt 2.7 in ubuntu:24.04), `Unable to locate package`, `Unable
/// to fetch some archives`; its lists need `apt-get update` first. `rest`:
/// what the fix runs after it (`usermod`, `podman system migrate`).
fn install_failed(command: &Cmd, error: &str, said: &str, fix_text: &str, rest: &[Cmd]) -> String {
    let shown = command.display();
    let package = command.args.last().cloned().unwrap_or_default();
    let said = said.trim();
    // dnf5: "Packages for argument '…' installed, but not available."; dnf4
    // (RHEL 9 and its rebuilds) ends with "Error: No packages marked for
    // reinstall." after "Installed package … not available."
    let dnf = command.args.first().is_some_and(|arg| arg == "dnf");
    if said.contains("installed, but not available")
        || (dnf && said.contains("No packages marked for reinstall"))
    {
        return format!(
            "`{shown}` failed: dnf says \"{said}\": the {package} build installed here is not \
             in the enabled repositories (Fedora's updates repository keeps only the newest \
             build), so dnf cannot reinstall it. Install the newest build instead, which writes \
             its files again (`sudo dnf upgrade -y {package}`), or, when dnf says there is \
             nothing to do, enable a repository that has {package} (`dnf repolist --all` lists \
             them); then run `{}`",
            joined(rest)
        );
    }
    let apt = command.args.first().is_some_and(|arg| arg == "apt-get");
    let lists = [
        "cannot be downloaded",
        "Unable to locate package",
        "has no installation candidate",
        "Unable to fetch some archives",
        "Failed to fetch",
    ];
    if apt && lists.iter().any(|words| said.contains(words)) {
        return format!(
            "{error}: apt says \"{said}\": apt's package lists are out of date or no longer \
             have {package}, so it cannot download it. Update them first: `sudo apt-get update \
             && {fix_text}`"
        );
    }
    // Anything else (the network, the repositories' metadata or keys): say
    // what failed; running the same command only helps once that is fixed.
    match said {
        "" => format!(
            "{error}; fix what it reported (often the network or the package manager's \
             repositories), then run {fix_text}"
        ),
        said => format!(
            "{error}: {said}; fix that (often the network or the package manager's \
             repositories), then run {fix_text}"
        ),
    }
}

/// The commands that run `fix`, exactly as its text shows them. `None` when
/// the install command cannot be rebuilt here as shown.
fn id_fix_commands(o: &Onboarding, fix: &LocalIdFix) -> Option<Vec<Cmd>> {
    let mut commands = Vec::new();
    if let Some(shown) = &fix.install {
        let installed = o
            .facts
            .podman
            .id_map_tools
            .as_ref()
            .and_then(|tools| tools.package_installed);
        let install = o
            .facts
            .os
            .release()
            .and_then(|release| release.package_manager())
            .and_then(|manager| manager.id_map_install(installed))
            .filter(|install| install.display() == *shown)?;
        commands.push(install);
    }
    if let Some(usermod) = &fix.usermod {
        let args: Vec<&str> = usermod.iter().skip(1).map(String::as_str).collect();
        commands.push(Cmd::sudo(&usermod[0], &args));
    }
    if fix.migrate {
        commands.push(Cmd::new("podman", &["system", "migrate"]));
    }
    Some(commands)
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
            // Without a range of subordinate ids, keep-id maps only the
            // user's own id: crun cannot set the container's
            // ping_group_range (it has no group 0), and images with other
            // owners do not unpack.
            let ids = ["subuid", "insufficient UIDs", "ping_group_range"]
                .iter()
                .any(|sign| reason.contains(sign));
            let fix = if ids {
                podman_report::id_failure_fix(&o.facts.podman, &o.facts.user)
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

#[cfg(test)]
mod install_failed_tests {
    use super::*;

    /// A reinstall dnf cannot do is named as such on dnf5 and on dnf4 (RHEL
    /// 9 and its rebuilds), and any other failure says to fix what it
    /// reported before running the command again.
    #[test]
    fn a_failed_install_says_what_failed_and_what_to_do() {
        let migrate = [Cmd::new("podman", &["system", "migrate"])];
        let dnf = Cmd::sudo("dnf", &["reinstall", "-y", "shadow-utils"]);
        let fix_text = "sudo dnf reinstall -y shadow-utils && podman system migrate";
        for said in [
            "Packages for argument 'shadow-utils' installed, but not available.",
            "Error: No packages marked for reinstall.",
        ] {
            let step = install_failed(
                &dnf,
                "`sudo dnf reinstall -y shadow-utils` failed (exit 1)",
                said,
                fix_text,
                &migrate,
            );
            assert!(
                step.contains("is not in the enabled repositories"),
                "{step}"
            );
            assert!(step.contains("sudo dnf upgrade -y shadow-utils"), "{step}");
            assert!(!step.contains(&format!("run {fix_text}")), "{step}");
        }
        let step = install_failed(
            &dnf,
            "`sudo dnf reinstall -y shadow-utils` failed (exit 1)",
            "Error: Failed to download metadata for repo 'fedora'",
            fix_text,
            &migrate,
        );
        assert_eq!(
            step,
            "`sudo dnf reinstall -y shadow-utils` failed (exit 1): Error: Failed to download \
             metadata for repo 'fedora'; fix that (often the network or the package manager's \
             repositories), then run sudo dnf reinstall -y shadow-utils && podman system migrate"
        );
    }
}
