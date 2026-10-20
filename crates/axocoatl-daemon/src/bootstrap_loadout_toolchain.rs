//! A loadout run's toolchain on the live daemon: the toolchain image (found
//! or built), the Workspace's toolchain cache volume, the setup with its
//! registries opened for it alone, and the check probe before the first
//! model call. The planning is pure (`loadout::toolchain`). Owner: workstream
//! `toolchain`.
use super::*;
use crate::external_agent::recipe_images::{self, RecipeImageRecord};
use crate::loadout::toolchain::{EnvironmentPlan, ProbeMissing, SetupPlan};
use axocoatl_session::network_record::{BindingKind, Decision, EgressScope, NetworkEvent};
use axocoatl_session::receipt::{EnvironmentFacts, ProbeFacts, SetupFacts};

/// Refused connections of a setup's own processes after which the setup is
/// stopped: a package manager retrying a host that is not among the
/// registries would otherwise wait out the 15-minute setup limit.
pub(crate) const SETUP_REFUSALS_LIMIT: u64 = 30;
/// How often the setup's refusals are counted while it runs.
const SETUP_REFUSAL_POLL: std::time::Duration = std::time::Duration::from_millis(500);
/// Most bytes of a failed setup's output kept in the run's error.
const SETUP_TAIL_BYTES: usize = 3 * 1024;
/// Lines of a failed setup's output kept in the run's error.
const SETUP_TAIL_LINES: usize = 30;
/// Longest the probe waits for `command -v`.
const PROBE_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Longest the probe runs the check on the untouched tree.
const PROBE_CHECK_TIMEOUT_CAP_SECS: u64 = 600;
/// Bytes of the probe's output read.
const PROBE_OUTPUT_BYTES: usize = 256 * 1024;
/// The marker a stamped setup prints when it installs nothing
/// (`axocoatl_session::toolchain::setup_script`).
const UNCHANGED_MARKER: &str = "axocoatl: nothing to install";

/// How a toolchain setup went.
pub(crate) struct SetupRun {
    pub facts: SetupFacts,
    /// Why the run cannot go on, with the last lines of the setup's output.
    pub failure: Option<String>,
}

/// What the probe found.
pub(crate) enum ProbeRun {
    /// The check can run here (and did, on the untouched tree).
    Runs(ProbeFacts),
    /// It cannot: the run is refused with this usage error.
    Refused(String),
}

/// The last lines of `text`, at most [`SETUP_TAIL_BYTES`].
fn last_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(SETUP_TAIL_LINES);
    let tail = lines[start..].join("\n");
    if tail.len() <= SETUP_TAIL_BYTES {
        return tail;
    }
    let mut cut = tail.len() - SETUP_TAIL_BYTES;
    while !tail.is_char_boundary(cut) {
        cut += 1;
    }
    format!("…{}", &tail[cut..])
}

/// The error that ends a run whose toolchain setup failed.
pub(crate) fn setup_failure_message(
    setup: &SetupPlan,
    exit_code: Option<i32>,
    duration_ms: u64,
    stdout: &str,
    stderr: &str,
    reason: Option<&str>,
) -> String {
    let ended = match (reason, exit_code) {
        (Some(reason), _) => reason.to_string(),
        (None, Some(code)) => format!("it exited {code}"),
        (None, None) => "it did not finish".into(),
    };
    let mut output = String::new();
    for part in [stdout.trim_end(), stderr.trim_end()] {
        if !part.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(part);
        }
    }
    let output = if output.is_empty() {
        "(no output)".to_string()
    } else {
        last_lines(&output)
    };
    format!(
        "the toolchain setup `{}` ({}) failed after {:.1} s: {ended}. No writer or reviewer \
         turn ran. \
         Fix the repository's dependency files, give the install with --setup \"<command>\", \
         or run without it with --no-setup. The last lines of its output:\n{output}",
        setup.command,
        setup.source.as_str(),
        duration_ms as f64 / 1000.0
    )
}

impl AxocoatlDaemon {
    /// Whether `session`'s container mounts its Workspace's toolchain cache:
    /// a loadout Session whose image is a recorded recipe image that holds a
    /// language toolchain.
    pub(crate) fn session_mounts_toolchain_cache(&self, session: &Session) -> bool {
        if session.loadout.is_none() {
            return false;
        }
        let Some(id) = session
            .image
            .as_deref()
            .and_then(recipe_images::normalize_image_id)
        else {
            return false;
        };
        recipe_images::recorded_images(&self.data_root)
            .map(|images| {
                images.iter().any(|record| {
                    record.image_id == id
                        && record
                            .recipes
                            .iter()
                            .any(|name| axocoatl_isolation::recipes::is_toolchain(name))
                })
            })
            .unwrap_or(false)
    }

    /// The toolchain cache volume of the Workspace at `workspace`.
    pub(crate) fn toolchain_cache_volume(&self, workspace: &std::path::Path) -> String {
        SessionSandbox::toolchain_cache_volume_name(&self.local_runtime_authority, workspace)
    }

    /// The image of `plan`'s recipes as this computer recorded its build:
    /// built now when it is missing and `build` allows it, refused naming
    /// the `axocoatl recipe build` command otherwise.
    pub(crate) async fn ensure_recipe_image(
        &self,
        plan: &EnvironmentPlan,
        build: bool,
    ) -> Result<RecipeImageRecord, DaemonError> {
        if let Ok(record) = recipe_images::image_for_recipes(&self.data_root, &plan.recipes) {
            return Ok(record);
        }
        let command = plan
            .build_command()
            .unwrap_or_else(|| "axocoatl recipe build <recipes>".into());
        if !build {
            return Err(DaemonError::InvalidRequest(format!(
                "the run's image {} is not built on this computer, and building it was turned \
                 off (--no-build). Build it with `{command}`, then run again",
                plan.image_text()
            )));
        }
        let size = plan
            .recipes
            .iter()
            .find_map(|name| axocoatl_isolation::recipes::toolchain_image(name))
            .map(|image| {
                format!(
                    " (about {} MB to download, {} MB on disk)",
                    image.download_mb, image.image_mb
                )
            })
            .unwrap_or_default();
        tracing::info!(
            image = %plan.image_text(),
            "building the run's image{size}; the same as `{command}`"
        );
        let record = crate::browser_install::build_recipe_image(&plan.recipes)
            .await
            .map_err(|error| {
                DaemonError::Session(format!(
                    "building the run's image {} failed: {error}. Build it with `{command}` to \
                     see Podman's output, then run again",
                    plan.image_text()
                ))
            })?;
        recipe_images::record_image(&self.data_root, record.clone()).map_err(|error| {
            DaemonError::Session(format!(
                "the run's image {} was built but could not be recorded: {error}",
                record.image
            ))
        })?;
        Ok(record)
    }

    /// The Session's sandbox handle.
    async fn loadout_sandbox(&self, session_id: &str) -> Result<Arc<dyn Sandbox>, DaemonError> {
        self.session_sandboxes
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| {
                DaemonError::Session(format!("Session {session_id} has no running container"))
            })
    }

    /// The last sequence number of the Session's network record.
    async fn network_last_seq(&self, session_id: &str) -> Option<u64> {
        self.session_network_records
            .stats(session_id)
            .await
            .ok()
            .map(|stats| stats.last_seq)
    }

    /// The setup's own connections recorded after `after`: allowed, refused
    /// and the refused hosts with their counts.
    async fn setup_connections(
        &self,
        session_id: &str,
        after: Option<u64>,
    ) -> (u64, u64, std::collections::BTreeMap<String, u64>) {
        let mut allowed = 0;
        let mut refused = 0;
        let mut refused_hosts: std::collections::BTreeMap<String, u64> = Default::default();
        let mut after = after;
        loop {
            let Ok(page) = self
                .session_network_records
                .read_after(session_id, after, 1000)
                .await
            else {
                break;
            };
            if page.events.is_empty() {
                break;
            }
            after = page.events.last().map(|line| line.seq);
            for line in page.events {
                if let NetworkEvent::Open {
                    decision,
                    binding: Some(binding),
                    host,
                    port,
                    ..
                } = line.event
                {
                    if binding.kind != BindingKind::Setup {
                        continue;
                    }
                    match decision {
                        Decision::Allow => allowed += 1,
                        Decision::Deny => {
                            refused += 1;
                            *refused_hosts.entry(format!("{host}:{port}")).or_default() += 1;
                        }
                    }
                }
            }
        }
        (allowed, refused, refused_hosts)
    }

    /// Run `setup` in the run's Session: open its registries to the
    /// Session's setup credential (a per-Session allow, recorded with the
    /// run as its actor), run it as the writer, then close them again, all
    /// before the first model call. A setup whose own connections are
    /// refused [`SETUP_REFUSALS_LIMIT`] times is stopped.
    pub(crate) async fn run_toolchain_setup(
        &self,
        session_id: &str,
        run_id: &str,
        network: &str,
        setup: &SetupPlan,
    ) -> SetupRun {
        use sha2::{Digest, Sha256};
        let mut facts = SetupFacts {
            command: setup.command.clone(),
            source: setup.source.as_str().into(),
            script_sha256: format!("{:x}", Sha256::digest(setup.script.as_bytes())),
            ..SetupFacts::default()
        };
        let fail = |mut facts: SetupFacts, error: String| SetupRun {
            failure: Some(error.clone()),
            facts: {
                facts.error = Some(error);
                facts
            },
        };
        let sandbox = match self.loadout_sandbox(session_id).await {
            Ok(sandbox) => sandbox,
            Err(error) => return fail(facts, error.to_string()),
        };
        // Under `network: none` there is no proxy to open anything on.
        let registries: &[(String, Vec<u16>)] = if network == "egress" {
            &setup.registries
        } else {
            &[]
        };
        let actor = format!("toolchain setup of loadout run {run_id}");
        let egress = if registries.is_empty() {
            None
        } else {
            match self.session_egress(session_id).await {
                Ok(egress) => Some(egress),
                Err(error) => {
                    return fail(facts, format!("opening the setup's registries: {error}"))
                }
            }
        };
        let mut opened: Vec<String> = Vec::new();
        if let Some(egress) = &egress {
            for (host, ports) in registries {
                let command_id = format!("{run_id}:setup-allow:{host}");
                match egress
                    .allow(
                        EgressScope::Session,
                        host,
                        Some(ports.clone()),
                        &actor,
                        &command_id,
                    )
                    .await
                {
                    Ok(_) => opened.push(host.clone()),
                    Err(error) => {
                        let closed = self
                            .close_setup_registries(egress, run_id, &actor, &opened)
                            .await;
                        let mut error = format!("opening {host} for the setup: {error}");
                        if let Err(close) = closed {
                            error.push_str(&format!("; {close}"));
                        }
                        return fail(facts, error);
                    }
                }
            }
        }
        facts.registry_hosts = opened.clone();
        let start_seq = self.network_last_seq(session_id).await;
        let started = std::time::Instant::now();
        let commands = vec![setup.script.clone()];
        let watched = async {
            loop {
                tokio::time::sleep(SETUP_REFUSAL_POLL).await;
                let (_, refused, hosts) = self.setup_connections(session_id, start_seq).await;
                if refused >= SETUP_REFUSALS_LIMIT {
                    return hosts;
                }
            }
        };
        let result = tokio::select! {
            result = sandbox.run_approved_setup(&commands) => Ok(result),
            hosts = watched => Err(hosts),
        };
        facts.duration_ms = started.elapsed().as_millis() as u64;
        // Closed before anything else: the writer never runs with them.
        let closed = match &egress {
            Some(egress) => {
                self.close_setup_registries(egress, run_id, &actor, &opened)
                    .await
            }
            None => Ok(()),
        };
        let (allowed, refused, refused_hosts) = self.setup_connections(session_id, start_seq).await;
        facts.allowed_connections = allowed;
        facts.refused_connections = refused;
        let refused_text = || {
            refused_hosts
                .iter()
                .map(|(host, count)| format!("{host} ({count})"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let registries_text = if opened.is_empty() {
            "none".to_string()
        } else {
            opened.join(", ")
        };
        let mut run = match result {
            Err(_) => {
                let reason = format!(
                    "it was stopped after {refused} of its connections were refused: {}. Only \
                     the registries {registries_text} are open for the setup; a host it needs \
                     beyond them can be added to sandbox.egress.allow",
                    refused_text()
                );
                let message =
                    setup_failure_message(setup, None, facts.duration_ms, "", "", Some(&reason));
                fail(facts, message)
            }
            Ok(Err(error)) => {
                let message = setup_failure_message(
                    setup,
                    None,
                    facts.duration_ms,
                    "",
                    "",
                    Some(&format!("{error}")),
                );
                fail(facts, message)
            }
            Ok(Ok(results)) => match results.into_iter().next() {
                None => fail(facts, "the setup did not run".into()),
                Some(result) => {
                    facts.exit_code = Some(result.exit_code);
                    facts.unchanged =
                        result.exit_code == 0 && result.stdout.contains(UNCHANGED_MARKER);
                    if result.exit_code == 0 {
                        SetupRun {
                            facts,
                            failure: None,
                        }
                    } else {
                        let mut reason = None;
                        let refusal_note;
                        if refused > 0 {
                            refusal_note = format!(
                                "it exited {}; {refused} of its connections were refused: {} \
                                 (only {registries_text} are open for the setup)",
                                result.exit_code,
                                refused_text()
                            );
                            reason = Some(refusal_note.as_str());
                        }
                        let message = setup_failure_message(
                            setup,
                            Some(result.exit_code),
                            facts.duration_ms,
                            &result.stdout,
                            &result.stderr,
                            reason,
                        );
                        fail(facts, message)
                    }
                }
            },
        };
        if let Err(error) = closed {
            // Fail closed: a writer must never start with a registry open.
            run = fail(run.facts, error);
        }
        run
    }

    /// Revoke the setup's registries, every one even when one fails.
    async fn close_setup_registries(
        &self,
        egress: &crate::session_egress::SessionEgress,
        run_id: &str,
        actor: &str,
        hosts: &[String],
    ) -> Result<(), String> {
        let mut failed = Vec::new();
        for host in hosts {
            let command_id = format!("{run_id}:setup-revoke:{host}");
            if let Err(error) = egress
                .revoke(EgressScope::Session, host, actor, &command_id)
                .await
            {
                failed.push(format!("{host}: {error}"));
            }
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "the registries opened for the setup could not be closed ({}); the run does not \
                 start with them open",
                failed.join("; ")
            ))
        }
    }

    /// Before the first model call: whether the check's program is on the
    /// writer's PATH, then one run of the check on the untouched tree as the
    /// writer, without network. A check that cannot run here refuses the run
    /// (a usage error); one that runs and fails is what the run is for.
    pub(crate) async fn probe_loadout_check(
        &self,
        session_id: &str,
        plan: &EnvironmentPlan,
        check: &str,
        timeout_secs: u64,
        task: &str,
        setup_note: Option<&str>,
    ) -> Result<ProbeRun, DaemonError> {
        let sandbox = self.loadout_sandbox(session_id).await?;
        let program = axocoatl_session::toolchain::check_program(check);
        let started = std::time::Instant::now();
        let mut facts = ProbeFacts {
            program: program.clone(),
            program_found: true,
            ..ProbeFacts::default()
        };
        if let Some(program) = &program {
            let lookup = sandbox
                .exec_observed(
                    &["sh", "-c", "command -v -- \"$1\"", "sh", program],
                    PROBE_LOOKUP_TIMEOUT,
                    4096,
                    4096,
                )
                .await
                .map_err(|error| {
                    DaemonError::Session(format!("probing the check's program {program}: {error}"))
                })?;
            if lookup.exit_code != Some(0) {
                return Ok(ProbeRun::Refused(crate::loadout::toolchain::probe_refusal(
                    plan,
                    check,
                    &ProbeMissing::Program(program.clone()),
                    setup_note,
                )));
            }
        }
        let timeout =
            std::time::Duration::from_secs(timeout_secs.clamp(1, PROBE_CHECK_TIMEOUT_CAP_SECS));
        let result = sandbox
            .exec_observed(
                &["sh", "-c", check],
                timeout,
                PROBE_OUTPUT_BYTES,
                PROBE_OUTPUT_BYTES,
            )
            .await
            .map_err(|error| DaemonError::Session(format!("probing the check: {error}")))?;
        facts.duration_ms = started.elapsed().as_millis() as u64;
        if result.client_timed_out {
            // A check that outlasts its own timeout is the run's to report.
            return Ok(ProbeRun::Runs(facts));
        }
        facts.baseline_exit = result.exit_code;
        let stdout = String::from_utf8_lossy(&result.stdout.retained);
        let stderr = String::from_utf8_lossy(&result.stderr.retained);
        match crate::loadout::toolchain::classify_probe(
            result.exit_code,
            &stdout,
            &stderr,
            program.as_deref(),
            task,
        ) {
            Some(missing) => {
                facts.program_found = !matches!(missing, ProbeMissing::Program(_));
                Ok(ProbeRun::Refused(crate::loadout::toolchain::probe_refusal(
                    plan, check, &missing, setup_note,
                )))
            }
            None => Ok(ProbeRun::Runs(facts)),
        }
    }

    /// The run's environment facts as admission decided them, before setup.
    pub(crate) fn environment_facts(
        &self,
        plan: &EnvironmentPlan,
        image_id: Option<&str>,
        workspace: &std::path::Path,
    ) -> EnvironmentFacts {
        EnvironmentFacts {
            toolchain: plan.toolchain.map(|toolchain| toolchain.name().to_string()),
            reason: plan.reason.clone(),
            image: plan.image_text(),
            image_id: image_id.map(str::to_string),
            recipes: plan.recipes.clone(),
            check: plan.check.as_ref().map(|(check, _)| check.clone()),
            check_source: plan
                .check
                .as_ref()
                .map(|(_, source)| source.describe().to_string()),
            setup: None,
            cache_volume: plan
                .toolchain
                .map(|_| self.toolchain_cache_volume(workspace)),
            probe: None,
        }
    }
}

/// One line for the run's notes about how its setup went.
pub(crate) fn setup_note(facts: &SetupFacts) -> String {
    let hosts = if facts.registry_hosts.is_empty() {
        "no registry".to_string()
    } else {
        facts.registry_hosts.join(", ")
    };
    let outcome = match (facts.exit_code, facts.unchanged) {
        (Some(0), true) => "nothing to install: the Workspace's cache already holds it".into(),
        (Some(code), _) => format!("exit {code}"),
        (None, _) => "did not finish".into(),
    };
    format!(
        "setup `{}` ({}): {outcome} in {:.1} s; {} connections to {hosts} allowed, {} refused; \
         the registries were closed before the first model call",
        facts.command,
        facts.source,
        facts.duration_ms as f64 / 1000.0,
        facts.allowed_connections,
        facts.refused_connections
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loadout::toolchain::SetupSource;

    #[test]
    fn a_failed_setup_names_the_command_the_exit_and_its_last_lines() {
        let setup = SetupPlan {
            command: "npm ci --prefer-offline --no-audit --no-fund".into(),
            source: SetupSource::Detected,
            script: "npm ci".into(),
            registries: vec![("registry.npmjs.org".into(), vec![443])],
        };
        let stdout: String = (1..=50).map(|n| format!("line {n}\n")).collect();
        let message = setup_failure_message(
            &setup,
            Some(1),
            12_340,
            &stdout,
            "npm error 404 Not Found - GET https://registry.npmjs.org/nope",
            None,
        );
        assert!(message.starts_with(
            "the toolchain setup `npm ci --prefer-offline --no-audit --no-fund` (detected) failed \
             after 12.3 s: it exited 1. No writer or reviewer turn ran."
        ), "{message}");
        assert!(message.contains("--setup"), "{message}");
        assert!(message.contains("--no-setup"), "{message}");
        assert!(message.contains("npm error 404 Not Found"), "{message}");
        assert!(
            !message.contains("line 20\n"),
            "only the last lines: {message}"
        );
        assert!(message.contains("line 50"), "{message}");
        let stopped = setup_failure_message(&setup, None, 900, "", "", Some("it was stopped"));
        assert!(stopped.contains("it was stopped"), "{stopped}");
        assert!(stopped.ends_with("(no output)"), "{stopped}");
        assert!(last_lines(&"x".repeat(10_000)).len() <= SETUP_TAIL_BYTES + 3);
    }

    #[test]
    fn the_setup_note_says_what_ran_and_what_it_reached() {
        let facts = SetupFacts {
            command: "pip install -e . pytest".into(),
            source: "detected".into(),
            exit_code: Some(0),
            duration_ms: 4_210,
            registry_hosts: vec!["pypi.org".into(), "files.pythonhosted.org".into()],
            allowed_connections: 9,
            ..SetupFacts::default()
        };
        assert_eq!(
            setup_note(&facts),
            "setup `pip install -e . pytest` (detected): exit 0 in 4.2 s; 9 connections to \
             pypi.org, files.pythonhosted.org allowed, 0 refused; the registries were closed \
             before the first model call"
        );
        let unchanged = SetupFacts {
            unchanged: true,
            allowed_connections: 0,
            ..facts
        };
        assert!(setup_note(&unchanged).contains("nothing to install"));
    }
}
