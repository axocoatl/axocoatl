//! A loadout run's toolchain on the live daemon: the toolchain image (found
//! or built), the Workspace's toolchain cache volume, the setup with its
//! registries opened for it alone, and the check probe before the first
//! model call. The planning is pure (`loadout::toolchain`). Owner: workstream
//! `toolchain`.
use super::*;
use crate::external_agent::recipe_images::{self, RecipeImageRecord};
use crate::loadout::toolchain::{EnvironmentPlan, ProbeContext, ProbeMissing, SetupPlan};
use axocoatl_isolation::{ToolchainCacheState, ToolchainCacheStep};
use axocoatl_session::network_record::{BindingKind, Decision, EgressScope, NetworkEvent};
use axocoatl_session::receipt::{EnvironmentFacts, ProbeFacts, SetupFacts};
use axocoatl_session::toolchain::Toolchain;

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
/// Longest the probe runs the check on the untouched tree. A check still
/// running then can run: it is stopped (the supervisor ends every process
/// it started) and the run goes on.
const PROBE_CHECK_TIMEOUT_CAP_SECS: u64 = 120;
/// Bytes of the probe's output read, each of stdout and stderr.
const PROBE_OUTPUT_BYTES: usize = 256 * 1024;

/// How a toolchain setup went.
pub(crate) struct SetupRun {
    pub facts: SetupFacts,
    /// Why the run cannot go on, with the last lines of the setup's output.
    pub failure: Option<String>,
    /// Whether root took the cache back after the setup (it did not run:
    /// nothing was handed over).
    pub sealed: bool,
}

/// What the probe found.
pub(crate) enum ProbeRun {
    /// The check can run here (and did, on the untouched tree).
    Runs(ProbeFacts),
    /// It cannot: the run is refused with this message, a usage error
    /// (exit 3) unless `environment` (exit 5).
    Refused { message: String, environment: bool },
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
         Fix the repository's dependency files, give the install with --setup \"COMMAND\", \
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
            .unwrap_or_else(|| "axocoatl recipe build RECIPE".into());
        if !build {
            return Err(DaemonError::InvalidRequest(format!(
                "the run's image {} is not built on this computer, and building it was turned \
                 off (--no-build). Build it with `{command}`, then run again",
                plan.image_text()
            )));
        }
        let size = axocoatl_isolation::recipes::image_size(&plan.recipes)
            .map(|size| {
                format!(
                    " (about {} MB of {} to download, {}{} MB on disk)",
                    size.base_download_mb,
                    size.base_image,
                    if size.unmeasured.is_empty() {
                        "about "
                    } else {
                        "at least "
                    },
                    size.disk_mb
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

    /// Run `setup` in the run's Session, all before the first model call.
    ///
    /// Root first takes the toolchain's directory of the Workspace's cache:
    /// when it holds this exact setup already ([`SetupPlan::key`]) nothing
    /// runs and no registry opens. Otherwise the directory is handed to the
    /// writer, the setup's registries are opened to the Session's setup
    /// credential (a per-Session allow, recorded with the run as its
    /// actor), the setup runs as the writer, the registries close, and root
    /// takes the directory back, read-only to the Session's users, whatever
    /// the setup did. A setup whose own connections are refused
    /// [`SETUP_REFUSALS_LIMIT`] times is stopped.
    pub(crate) async fn run_toolchain_setup(
        &self,
        session_id: &str,
        run_id: &str,
        toolchain: Toolchain,
        network: &str,
        setup: &SetupPlan,
    ) -> SetupRun {
        use sha2::{Digest, Sha256};
        let facts = SetupFacts {
            command: setup.command.clone(),
            source: setup.source.as_str().into(),
            script_sha256: format!("{:x}", Sha256::digest(setup.script.as_bytes())),
            ..SetupFacts::default()
        };
        let sandbox = match self.loadout_sandbox(session_id).await {
            Ok(sandbox) => sandbox,
            Err(error) => return failed_setup(facts, error.to_string()),
        };
        let started = std::time::Instant::now();
        let before = ToolchainCacheStep::Before {
            toolchain: toolchain.name().into(),
            key: setup.key.clone(),
            reset: toolchain
                .setup_reset_entries()
                .iter()
                .map(|entry| entry.to_string())
                .collect(),
        };
        match sandbox.toolchain_cache_step(&before).await {
            Ok(ToolchainCacheState::Unchanged) => {
                let mut facts = facts;
                facts.exit_code = Some(0);
                facts.unchanged = true;
                facts.duration_ms = started.elapsed().as_millis() as u64;
                return SetupRun {
                    facts,
                    failure: None,
                    sealed: true,
                };
            }
            Ok(_) => {}
            Err(error) => {
                // Taken back even so: nothing of it may stay the writer's.
                let _ = sandbox
                    .toolchain_cache_step(&ToolchainCacheStep::After {
                        toolchain: toolchain.name().into(),
                        key: None,
                    })
                    .await;
                return failed_setup(
                    facts,
                    format!("preparing the Workspace's toolchain cache for the setup: {error}"),
                );
            }
        }
        let mut run = self
            .run_setup_with_registries(&sandbox, session_id, run_id, network, setup, facts)
            .await;
        let after = ToolchainCacheStep::After {
            toolchain: toolchain.name().into(),
            key: if run.failure.is_none() {
                setup.key.clone()
            } else {
                None
            },
        };
        if let Err(error) = sandbox.toolchain_cache_step(&after).await {
            // Fail closed: the writer never starts with the cache writable.
            // A setup that failed first keeps its own message.
            let error = format!(
                "the Workspace's toolchain cache could not be made read-only after the setup: \
                 {error}"
            );
            run = match run.failure {
                Some(failure) => {
                    let mut run = failed_setup(run.facts, format!("{failure}\n({error})"));
                    run.sealed = false;
                    run
                }
                None => SetupRun {
                    sealed: false,
                    ..failed_setup(run.facts, error)
                },
            };
        }
        run.facts.duration_ms = started.elapsed().as_millis() as u64;
        run
    }

    /// The setup itself, with its registries open for it alone.
    async fn run_setup_with_registries(
        &self,
        sandbox: &Arc<dyn Sandbox>,
        session_id: &str,
        run_id: &str,
        network: &str,
        setup: &SetupPlan,
        mut facts: SetupFacts,
    ) -> SetupRun {
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
                    return failed_setup(facts, format!("opening the setup's registries: {error}"))
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
                        return failed_setup(facts, error);
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
                failed_setup(facts, message)
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
                failed_setup(facts, message)
            }
            Ok(Ok(results)) => match results.into_iter().next() {
                None => failed_setup(facts, "the setup did not run".into()),
                Some(result) => {
                    facts.exit_code = Some(result.exit_code);
                    if result.exit_code == 0 {
                        SetupRun {
                            facts,
                            failure: None,
                            sealed: true,
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
                        failed_setup(facts, message)
                    }
                }
            },
        };
        if let Err(error) = closed {
            // Fail closed: a writer must never start with a registry open.
            run = failed_setup(run.facts, error);
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
    /// writer, without network, through the Session's supervisor exactly as
    /// the run's own checks run (so a refusal of the hardened sandbox shows
    /// here too, and a check that outlasts the probe is stopped with every
    /// process it started). A check that cannot run here refuses the run;
    /// one that runs and fails is what the run is for. `Err` is a probe
    /// that could not run, which decides nothing.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn probe_loadout_check(
        &self,
        session_id: &str,
        run_id: &str,
        plan: &EnvironmentPlan,
        check: &str,
        timeout_secs: u64,
        task: &str,
        setup_note: Option<&str>,
    ) -> Result<ProbeRun, DaemonError> {
        let sandbox = self.loadout_sandbox(session_id).await?;
        let hints = &plan.probe;
        let started = std::time::Instant::now();
        let mut facts = ProbeFacts {
            program: hints.program.clone(),
            program_found: true,
            ..ProbeFacts::default()
        };
        let refused = |missing: &ProbeMissing| ProbeRun::Refused {
            message: crate::loadout::toolchain::probe_refusal(plan, check, missing, setup_note),
            environment: missing.is_environment(),
        };
        if let Some(program) = &hints.program {
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
            if lookup.client_timed_out {
                return Err(DaemonError::Session(format!(
                    "probing the check's program {program} timed out"
                )));
            }
            if lookup.exit_code != Some(0) {
                return Ok(refused(&ProbeMissing::Program(program.clone())));
            }
        }
        let timeout =
            std::time::Duration::from_secs(timeout_secs.clamp(1, PROBE_CHECK_TIMEOUT_CAP_SECS));
        let request = axocoatl_exec::protocol::ExecRequest {
            protocol: axocoatl_exec::protocol::PROTOCOL_VERSION,
            invocation_id: format!("check-probe-{run_id}"),
            argv: vec!["sh".into(), "-c".into(), check.to_string()],
            stdin: None,
            timeout_ms: timeout.as_millis() as u64,
            stdout_bytes: PROBE_OUTPUT_BYTES,
            stderr_bytes: PROBE_OUTPUT_BYTES,
            write_restriction: None,
        };
        request
            .validate()
            .map_err(|error| DaemonError::Session(format!("the check probe's request: {error}")))?;
        let execution = sandbox
            .prepare_supervised_command(request)
            .await
            .and_then(|prepared| prepared.dispatch())
            .map_err(|error| DaemonError::Session(format!("starting the check probe: {error}")))?
            .finish()
            .await
            .map_err(|error| DaemonError::Session(format!("the check probe: {error}")))?;
        facts.duration_ms = started.elapsed().as_millis() as u64;
        let axocoatl_exec::protocol::ServerMessage::Finished {
            outcome,
            stdout,
            stderr,
            ..
        } = execution.result()
        else {
            return Err(DaemonError::Session(
                "the check probe has no terminal result".into(),
            ));
        };
        use axocoatl_exec::protocol::ProcessOutcome;
        let exit_code = match outcome {
            ProcessOutcome::Exited { code } => Some(*code),
            ProcessOutcome::Signalled { .. } => None,
            ProcessOutcome::TimedOut => {
                // It ran for the probe's whole time: it can run.
                facts.timed_out = true;
                return Ok(ProbeRun::Runs(facts));
            }
            ProcessOutcome::Cancelled
            | ProcessOutcome::LaunchFailed { .. }
            | ProcessOutcome::Failed { .. } => {
                return Err(DaemonError::Session(format!(
                    "the check probe did not run: {outcome:?}"
                )))
            }
        };
        facts.baseline_exit = exit_code;
        let text = |output: &axocoatl_exec::protocol::CapturedOutput| {
            output
                .retained_bytes(PROBE_OUTPUT_BYTES)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default()
        };
        let (stdout, stderr) = (text(stdout), text(stderr));
        match crate::loadout::toolchain::classify_probe(
            exit_code,
            &stdout,
            &stderr,
            ProbeContext { hints, task },
        ) {
            Some(missing) => {
                facts.program_found = !matches!(missing, ProbeMissing::Program(_));
                Ok(refused(&missing))
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

/// A setup that cannot go on, with why.
fn failed_setup(mut facts: SetupFacts, error: String) -> SetupRun {
    facts.error = Some(error.clone());
    SetupRun {
        facts,
        failure: Some(error),
        sealed: true,
    }
}

/// How a setup ended, in words: `exited 0`, `did not finish`, or that the
/// Workspace's cache already held it.
pub(crate) fn setup_outcome(facts: &SetupFacts) -> String {
    match (facts.exit_code, facts.unchanged) {
        (Some(0), true) => "nothing to install: the Workspace's cache already holds it".into(),
        (Some(code), _) => format!("exited {code}"),
        (None, _) => "did not finish".into(),
    }
}

/// One line for the run's notes about how its setup went.
pub(crate) fn setup_note(run: &SetupRun) -> String {
    let facts = &run.facts;
    let outcome = setup_outcome(facts);
    let head = format!(
        "setup `{}` ({}): {outcome} ({:.1} s)",
        facts.command,
        facts.source,
        facts.duration_ms as f64 / 1000.0
    );
    if facts.unchanged {
        return format!("{head}; no registry was opened");
    }
    let network = if facts.registry_hosts.is_empty() {
        format!(
            "no registry was opened; {} of its connections were refused",
            facts.refused_connections
        )
    } else {
        format!(
            "{} connections to {} allowed, {} refused; the registries were closed",
            facts.allowed_connections,
            facts.registry_hosts.join(", "),
            facts.refused_connections
        )
    };
    let cache = if run.sealed {
        "the Workspace's toolchain cache was made read-only again"
    } else {
        "the Workspace's toolchain cache could not be made read-only, so the run did not start"
    };
    format!("{head}; {network}; {cache} before the first model call")
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
            key: None,
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
        let run = |facts: SetupFacts, sealed: bool| SetupRun {
            facts,
            failure: None,
            sealed,
        };
        assert_eq!(
            setup_note(&run(facts.clone(), true)),
            "setup `pip install -e . pytest` (detected): exited 0 (4.2 s); 9 connections to \
             pypi.org, files.pythonhosted.org allowed, 0 refused; the registries were closed; \
             the Workspace's toolchain cache was made read-only again before the first model \
             call"
        );
        assert!(setup_note(&run(facts.clone(), false)).contains("could not be made read-only"));
        assert_eq!(setup_outcome(&facts), "exited 0");
        assert_eq!(
            setup_outcome(&SetupFacts {
                exit_code: None,
                ..SetupFacts::default()
            }),
            "did not finish"
        );
        let unchanged = SetupFacts {
            unchanged: true,
            allowed_connections: 0,
            registry_hosts: Vec::new(),
            duration_ms: 300,
            ..facts
        };
        assert_eq!(
            setup_note(&run(unchanged, true)),
            "setup `pip install -e . pytest` (detected): nothing to install: the Workspace's \
             cache already holds it (0.3 s); no registry was opened"
        );
    }
}
