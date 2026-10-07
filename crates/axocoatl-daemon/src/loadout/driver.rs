//! The generic run driver: prepare, drive the kind, build the Outcome.
//! Owner: core.
//!
//! `run_to_outcome` drives the loadout's kind (`driver_for`), then folds
//! what it observed into one [`RunOutcome`]: the final turn's checks (with
//! their reports) and review, the kind's findings and adjudications, every
//! node that ended without a result as not covered, the warnings, usage and
//! the network summary. It decides the verdict, writes `outcome.json` once
//! and records the `Ended` event. Nothing is ever silently a pass: a failed
//! hook, a failed node or a run cut off by its wall clock each change the
//! exit code.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use axocoatl_session::failure_class::{classify_failure, FailureFacts};
use axocoatl_session::run_outcome::{
    exit_code, FailureClass, LoadoutRef, NodeState, NotCovered, RunOutcome, RunTurnRef, RunUsage,
    RunVerdict, RunWarning, TurnObservation, TurnState, VerdictInputs, RUN_OUTCOME_SCHEMA,
};
use axocoatl_session::run_record::RunEvent;

use super::team_plan::{default_slots, team_edit, writer_identities};
use super::{driver_for, KindDriver, KindReport, RunContext, RunError, RunHost};

/// How long a stopped turn may take to settle before the driver stops
/// waiting for it.
pub const STOP_GRACE: Duration = Duration::from_secs(30);
/// Extra time past the wall clock before the driver abandons a kind driver
/// that does not return (each wait already honors the deadline).
pub const DRIVER_GRACE: Duration = Duration::from_secs(120);

/// One turn of the resolved team; the driver of `custom` loadouts and the
/// building block of `fix` and `qa`.
pub struct SingleTurnDriver;

#[async_trait]
impl KindDriver for SingleTurnDriver {
    async fn drive(&self, host: &dyn RunHost, run: &RunContext) -> Result<KindReport, RunError> {
        let turn = run_single_turn(host, run).await?;
        let budget_exhausted = deadline_passed(run) && turn.state != TurnState::Completed;
        Ok(KindReport {
            turn_refs: vec![RunTurnRef {
                turn_id: turn.turn_id.clone(),
                purpose: "run".into(),
                state: turn.state,
            }],
            turns: vec![turn],
            budget_exhausted,
            ..KindReport::default()
        })
    }
}

/// Now, in Unix milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_millis() as u64)
        .unwrap_or(0)
}

fn deadline_passed(run: &RunContext) -> bool {
    Instant::now() >= run.deadline
}

/// Record one event; a record that cannot be written ends the run.
async fn record(host: &dyn RunHost, run: &RunContext, event: RunEvent) -> Result<(), RunError> {
    host.record(&run.run_id, event).await
}

async fn phase(
    host: &dyn RunHost,
    run: &RunContext,
    name: &str,
    detail: impl Into<String>,
) -> Result<(), RunError> {
    record(
        host,
        run,
        RunEvent::Phase {
            at_ms: now_ms(),
            phase: name.into(),
            detail: detail.into(),
        },
    )
    .await
}

/// Send `request` as one turn and wait for it. Past the deadline the turn is
/// stopped and observed once more; the observation says how it ended.
pub async fn run_turn(
    host: &dyn RunHost,
    run: &RunContext,
    request: &str,
    purpose: &str,
) -> Result<TurnObservation, RunError> {
    // A person who stopped the run gets no further turn.
    if host.stop_requested(&run.run_id).await {
        return Err(RunError::Stopped);
    }
    let turn_id = host.send_turn(&run.session_id, request).await?;
    record(
        host,
        run,
        RunEvent::TurnStarted {
            at_ms: now_ms(),
            turn_id: turn_id.clone(),
            purpose: purpose.into(),
        },
    )
    .await?;
    let mut observed = host
        .wait_turn(&run.session_id, &turn_id, run.deadline)
        .await?;
    if observed.state == TurnState::Running {
        phase(
            host,
            run,
            "stopping",
            "the run's wall clock ran out; stopping the turn",
        )
        .await?;
        host.stop_turn(&run.session_id, &turn_id).await?;
        observed = host
            .wait_turn(&run.session_id, &turn_id, Instant::now() + STOP_GRACE)
            .await?;
    }
    record(
        host,
        run,
        RunEvent::TurnEnded {
            at_ms: now_ms(),
            turn_id,
            state: observed.state,
        },
    )
    .await?;
    Ok(observed)
}

/// Apply the loadout's team, send its prompt, and wait for the turn.
pub async fn run_single_turn(
    host: &dyn RunHost,
    run: &RunContext,
) -> Result<TurnObservation, RunError> {
    phase(host, run, "applying_team", "applying the loadout's team").await?;
    let slots = default_slots(&run.resolved)?;
    let edit = team_edit(&run.resolved, &slots, true, 0)?;
    host.apply_team(&run.session_id, edit).await?;
    phase(host, run, "running", "the team is working on the task").await?;
    run_turn(host, run, &run.resolved.prompt, "run").await
}

/// Close every turn of the run that is still open once the run has
/// everything its Outcome needs, whatever the verdict. A turn that needs
/// attention keeps its Session's hold on the Workspace until someone
/// continues, stops or closes it, and nobody continues a run's turn: the next
/// run on the same repository would find the Workspace held. Each turn the
/// run started (`events`' `TurnStarted`) that was not observed completed is
/// stopped as a person's Stop stops it, observed until it settles, and
/// stopped once more: that second Stop releases the Workspace when the first
/// came while the turn's last execution was still finishing. A turn
/// observed stopped only gets that second Stop. The Outcome keeps every turn
/// as it was observed. A Stop that fails is logged, never fatal: the next
/// run's admission then names the Session that still holds the Workspace.
async fn close_open_turns(
    host: &dyn RunHost,
    run: &RunContext,
    observed: &[TurnObservation],
    events: &[RunEvent],
) -> Result<(), RunError> {
    let mut turns: Vec<(String, Option<TurnState>)> = Vec::new();
    for event in events {
        if let RunEvent::TurnStarted { turn_id, .. } = event {
            if !turns.iter().any(|(known, _)| known == turn_id) {
                turns.push((turn_id.clone(), None));
            }
        }
    }
    for turn in observed {
        match turns.iter_mut().find(|(known, _)| *known == turn.turn_id) {
            Some(entry) => entry.1 = Some(turn.state),
            None => turns.push((turn.turn_id.clone(), Some(turn.state))),
        }
    }
    for (turn_id, state) in turns {
        if state == Some(TurnState::Completed) {
            continue;
        }
        if state != Some(TurnState::Stopped) {
            let how = match state {
                Some(TurnState::NeedsAttention) => "needs attention",
                Some(TurnState::Running) => "is still running",
                Some(TurnState::Failed) => "failed",
                _ => "was not observed to end",
            };
            phase(
                host,
                run,
                "closing_turn",
                format!(
                    "Stopping turn {turn_id}, which {how}, so the run's Session no longer holds \
                     the Workspace; the Outcome keeps the turn as it was observed"
                ),
            )
            .await?;
            if let Err(error) = host.stop_turn(&run.session_id, &turn_id).await {
                tracing::debug!(run = %run.run_id, turn = %turn_id, %error, "stopping a run's open turn");
            }
            if let Err(error) = host
                .wait_turn(&run.session_id, &turn_id, Instant::now() + STOP_GRACE)
                .await
            {
                tracing::debug!(run = %run.run_id, turn = %turn_id, %error, "observing a run's stopped turn");
            }
        }
        if let Err(error) = host.stop_turn(&run.session_id, &turn_id).await {
            // Expected once the turn is closed and its Workspace released.
            tracing::debug!(run = %run.run_id, turn = %turn_id, %error, "settling a run's stopped turn");
        }
    }
    Ok(())
}

/// The class of a failure the host did not classify: the runtime's
/// classification, or the closest safe class while it is not available.
pub fn class_of(message: &str, recorded: Option<&str>) -> FailureClass {
    let facts = FailureFacts {
        message,
        recorded_class: recorded,
        ..FailureFacts::default()
    };
    match classify_failure(&facts) {
        Ok(class) => class,
        Err(_) => match recorded {
            Some("budget") => FailureClass::Budget,
            Some("round_limit") | Some("runtime_limit") => FailureClass::RuntimeLimit,
            Some("stopped") => FailureClass::Stopped,
            Some("blocked") => FailureClass::Blocked,
            _ => FailureClass::Other,
        },
    }
}

/// Every node of every turn that ended without an accepted result, as not
/// covered: failed, blocked or stopped nodes, and required nodes that never
/// started. The required reviewer is left out: the review covers it.
pub fn scan_not_covered(turns: &[TurnObservation], wall_clock_ran_out: bool) -> Vec<NotCovered> {
    let mut entries = Vec::new();
    for turn in turns {
        for node in &turn.nodes {
            if node.kind == "reviewer" {
                continue;
            }
            let Some(latest) = node.latest() else {
                if node.required {
                    entries.push(NotCovered {
                        area: node.slot_id.clone(),
                        class: if wall_clock_ran_out {
                            FailureClass::Budget
                        } else {
                            FailureClass::NotReached
                        },
                        detail: format!("{} never started", node.slot_id),
                        node_id: Some(node.node_id.clone()),
                        turn_id: Some(turn.turn_id.clone()),
                    });
                }
                continue;
            };
            let (class, detail) = match latest.state {
                NodeState::Accepted | NodeState::Superseded | NodeState::Running => continue,
                NodeState::NeverStarted if !node.required => continue,
                NodeState::NeverStarted => (
                    FailureClass::NotReached,
                    format!("{} never started", node.slot_id),
                ),
                NodeState::Failed | NodeState::Blocked | NodeState::Stopped => {
                    match &latest.failure {
                        Some(failure) if !failure.message.trim().is_empty() => {
                            (failure.class, failure.message.clone())
                        }
                        // No failure, or one with an empty reason: the
                        // entry still says what happened, never "other: ".
                        failure => (
                            match (failure.as_ref().map(|failure| failure.class), latest.state) {
                                (Some(class), _) if class != FailureClass::Other => class,
                                (_, NodeState::Blocked) => FailureClass::Blocked,
                                (_, NodeState::Stopped) => FailureClass::Stopped,
                                _ => FailureClass::Other,
                            },
                            format!("{} ended without a result", node.slot_id),
                        ),
                    }
                }
            };
            let class = if wall_clock_ran_out
                && matches!(class, FailureClass::Stopped | FailureClass::NotReached)
            {
                FailureClass::Budget
            } else {
                class
            };
            entries.push(NotCovered {
                area: node.slot_id.clone(),
                class,
                detail,
                node_id: Some(node.node_id.clone()),
                turn_id: Some(turn.turn_id.clone()),
            });
        }
    }
    entries
}

/// The run's warnings: those of its resolution, the kind's, and the
/// same-model reviewer warning against the reviewer the turn ran, each once.
fn warnings(run: &RunContext, report: &KindReport, turns: &[TurnObservation]) -> Vec<RunWarning> {
    let mut out: Vec<RunWarning> = run
        .resolved
        .warnings
        .iter()
        .map(|warning| RunWarning {
            code: warning.code.clone(),
            message: warning.message.clone(),
        })
        .collect();
    let mut push = |warning: RunWarning| {
        if !out.iter().any(|existing| existing.code == warning.code) {
            out.push(warning);
        }
    };
    for warning in &report.warnings {
        push(warning.clone());
    }
    let writers = writer_identities(&run.resolved);
    for turn in turns {
        if let Some(review) = &turn.review {
            if let Some(warning) =
                axocoatl_session::run_outcome::same_model_warning(&writers, &review.reviewer)
            {
                push(warning);
            }
        }
    }
    out
}

fn usage(turns: &[TurnObservation], events: &[RunEvent]) -> RunUsage {
    let mut usage = RunUsage {
        complete: true,
        ..RunUsage::default()
    };
    for turn in turns {
        usage.input_tokens = usage.input_tokens.saturating_add(turn.usage.input_tokens);
        usage.output_tokens = usage.output_tokens.saturating_add(turn.usage.output_tokens);
        usage.cost_microunits = usage
            .cost_microunits
            .saturating_add(turn.usage.cost_microunits);
        usage.complete &= turn.usage.complete;
        usage.retries = usage.retries.saturating_add(turn.usage.retries);
    }
    let recorded = events
        .iter()
        .filter(|event| matches!(event, RunEvent::ProviderRetry { .. }))
        .count() as u32;
    usage.retries = usage.retries.max(recorded);
    usage
}

/// The Outcome skeleton of `run`, before anything is folded in.
pub fn empty_outcome(run: &RunContext, started_at_ms: u64) -> RunOutcome {
    let loadout = &run.resolved.loadout;
    RunOutcome {
        schema: RUN_OUTCOME_SCHEMA.into(),
        run_id: run.run_id.clone(),
        session_id: run.session_id.clone(),
        workspace_id: run.workspace_id.clone(),
        loadout: LoadoutRef {
            id: loadout.file.id.clone(),
            version: loadout.file.version,
            kind: loadout.file.kind.to_string(),
            digest: loadout.digest.clone(),
            builtin: loadout.source == axocoatl_config::loadout::LoadoutSource::Builtin,
        },
        task: run.options.task.clone(),
        started_at_ms,
        finished_at_ms: started_at_ms,
        verdict: RunVerdict::Error,
        exit_code: exit_code::INFRASTRUCTURE,
        attention: Vec::new(),
        turns: Vec::new(),
        checks: Vec::new(),
        review: None,
        adjudications: Vec::new(),
        findings: Vec::new(),
        not_covered: Vec::new(),
        warnings: Vec::new(),
        usage: RunUsage::default(),
        network: Default::default(),
        keep: None,
        error: None,
    }
}

/// Run `run` to its Outcome: drive its kind, fold the report and observed
/// turns into a [`RunOutcome`], decide the verdict, and record it.
pub async fn run_to_outcome(host: &dyn RunHost, run: &RunContext) -> Result<RunOutcome, RunError> {
    let driver = driver_for(run.resolved.loadout.file.kind);
    run_to_outcome_with(host, run, driver.as_ref()).await
}

/// [`run_to_outcome`] with an explicit kind driver.
pub async fn run_to_outcome_with(
    host: &dyn RunHost,
    run: &RunContext,
    driver: &dyn KindDriver,
) -> Result<RunOutcome, RunError> {
    let started_at_ms = now_ms();
    let mut outcome = empty_outcome(run, started_at_ms);
    phase(
        host,
        run,
        "running",
        format!("running loadout {}", outcome.loadout.id),
    )
    .await?;
    let abandon_at = run.deadline + DRIVER_GRACE;
    let driven = match tokio::time::timeout_at(
        tokio::time::Instant::from_std(abandon_at),
        driver.drive(host, run),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(RunError::Deadline),
    };
    let mut usage_error = false;
    let mut interrupted = host.stop_requested(&run.run_id).await;
    let mut report = KindReport::default();
    match driven {
        Ok(driven) => report = driven,
        Err(RunError::Deadline) => report.budget_exhausted = true,
        Err(RunError::Stopped) => interrupted = true,
        Err(RunError::Usage(message)) => {
            usage_error = true;
            outcome.error = Some(message);
        }
        Err(error @ RunError::NotImplemented(_)) | Err(error @ RunError::Infrastructure(_)) => {
            outcome.error = Some(error.to_string())
        }
    }
    phase(host, run, "finishing", "building the Outcome").await?;
    let wall_clock_ran_out = report.budget_exhausted || deadline_passed(run);
    if let Some(last) = report.turns.last() {
        outcome.checks = last.checks.clone();
        outcome.review = last.review.clone();
    }
    let needs_reports = run.resolved.checks.iter().any(|check| check.e2e.is_some());
    if needs_reports && !outcome.checks.is_empty() && outcome.error.is_none() {
        if let Err(error) = super::e2e::collect_reports(host, run, &mut outcome.checks).await {
            outcome.error = Some(error.to_string());
        }
    }
    let started = host.recorded_events(&run.run_id).await.unwrap_or_default();
    close_open_turns(host, run, &report.turns, &started).await?;
    outcome.turns = if report.turn_refs.is_empty() {
        report
            .turns
            .iter()
            .map(|turn| RunTurnRef {
                turn_id: turn.turn_id.clone(),
                purpose: "run".into(),
                state: turn.state,
            })
            .collect()
    } else {
        report.turn_refs.clone()
    };
    outcome.adjudications = report.adjudications.clone();
    outcome.findings = report.findings.clone();
    outcome.not_covered = report.not_covered.clone();
    for entry in scan_not_covered(&report.turns, wall_clock_ran_out) {
        let known = outcome.not_covered.iter().any(|existing| {
            (entry.node_id.is_some()
                && existing.node_id == entry.node_id
                && existing.turn_id == entry.turn_id)
                || existing.area == entry.area
        });
        if !known {
            outcome.not_covered.push(entry);
        }
    }
    outcome.warnings = warnings(run, &report, &report.turns);
    let events = host.recorded_events(&run.run_id).await.unwrap_or_default();
    outcome.usage = usage(&report.turns, &events);
    outcome.network = host
        .network_summary(&run.session_id)
        .await
        .unwrap_or_default();
    let final_state = report.turns.last().map(|turn| turn.state);
    let turn_needs_attention = match final_state {
        Some(TurnState::Completed) => false,
        Some(_) => true,
        // A run whose driver ended without any turn needs a person unless
        // it already failed with an error.
        None => outcome.error.is_none() && !interrupted,
    };
    outcome.finished_at_ms = now_ms();
    outcome.decide(VerdictInputs {
        fail_on_findings: report.fail_on_findings,
        turn_needs_attention,
        budget_exhausted: wall_clock_ran_out,
        interrupted,
    });
    if usage_error {
        outcome.exit_code = exit_code::USAGE;
    }
    // Kind drivers record their own warnings, findings and not-covered
    // entries as they go; only what is not in the record yet is added, so
    // nothing appears twice.
    for warning in &outcome.warnings {
        let recorded = events.iter().any(|event| {
            matches!(event, RunEvent::Warning { warning: existing, .. }
                if existing.code == warning.code && existing.message == warning.message)
        });
        if !recorded
            && !run
                .resolved
                .warnings
                .iter()
                .any(|resolved| resolved.code == warning.code)
        {
            record(
                host,
                run,
                RunEvent::Warning {
                    at_ms: now_ms(),
                    warning: warning.clone(),
                },
            )
            .await?;
        }
    }
    for entry in &outcome.not_covered {
        let recorded = events.iter().any(|event| {
            matches!(event, RunEvent::NotCovered { entry: existing, .. } if **existing == *entry)
        });
        if recorded {
            continue;
        }
        record(
            host,
            run,
            RunEvent::NotCovered {
                at_ms: now_ms(),
                entry: Box::new(entry.clone()),
            },
        )
        .await?;
    }
    // The Ended event is the record's last line; outcome.json, written
    // once after it, closes the record to further events.
    let ended = RunEvent::Ended {
        at_ms: now_ms(),
        outcome: Box::new(outcome.clone()),
    };
    if record(host, run, ended).await.is_err() {
        // An Outcome too large for one event line is in outcome.json; the
        // event still marks the end.
        phase(
            host,
            run,
            "ended",
            format!(
                "verdict {:?}, exit code {}",
                outcome.verdict, outcome.exit_code
            ),
        )
        .await?;
    }
    host.finish(&run.run_id, &outcome).await?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loadout::host::ReproRequest;
    use crate::loadout::{KeepMode, RunOptions};
    use crate::SessionTeamEdit;
    use axocoatl_config::loadout::{parse_loadout, resolve_loadout, LoadoutSource, ParamValues};
    use axocoatl_session::run_outcome::*;
    use std::sync::Mutex;

    const CUSTOM: &str = r#"
schema: axocoatl.loadout/1
id: tidy
version: 2
name: Tidy
kind: custom
params:
  writer_model: { kind: model, default: "openrouter:qwen/qwen3-coder" }
  reviewer_model: { kind: model, default: "openrouter:openai/gpt-oss-120b" }
agents:
  - id: writer
    role: writer
    model: { param: writer_model }
    tools: [read_file, write_file, bash]
checks:
  - { name: tests, run: { argv: [cargo, test] }, timeout: 2m }
review:
  model: { param: reviewer_model }
  rounds: 2
  tools: [read_file]
budgets:
  agent: { activations: 2, invocations: 40, tokens: 100000, cost_usd: 1 }
  reviewer: { activations: 2, invocations: 4, tokens: 10000, cost_usd: 1 }
  wall_clock: 10m
prompt: "{task}"
"#;

    fn context(params: &[(&str, &str)], deadline: Instant) -> RunContext {
        let loadout = parse_loadout(CUSTOM, LoadoutSource::Builtin).unwrap();
        let mut values = ParamValues::new();
        for (name, value) in params {
            values.insert((*name).into(), (*value).into());
        }
        let resolved = resolve_loadout(&loadout, &values, "tidy up", "/repo").unwrap();
        RunContext {
            run_id: "run-00000000-0000-4000-8000-000000000009".into(),
            session_id: "ses-1".into(),
            workspace_id: "wsp-1".into(),
            resolved,
            options: RunOptions {
                task: "tidy up".into(),
                repo: "/repo".into(),
                params: values,
                keep: KeepMode::None,
                check_command: None,
                setup_command: None,
            },
            deadline,
        }
    }

    fn check(state: CheckState) -> CheckResult {
        CheckResult {
            name: "tests".into(),
            argv: vec!["cargo".into(), "test".into()],
            state,
            timeout_ms: 120_000,
            exit_code: Some(if state == CheckState::Passed { 0 } else { 1 }),
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            candidate_sha256: None,
            report: None,
            reason: None,
        }
    }

    fn review(passed: bool, model: &str) -> ReviewOutcome {
        ReviewOutcome {
            reviewer: ModelIdentity {
                provider: "openrouter".into(),
                model: model.into(),
                runtime: "native".into(),
            },
            max_rounds: 2,
            rounds: vec![],
            passed,
            state: if passed { "approved" } else { "changes" }.into(),
            reason: if passed {
                "approved"
            } else {
                "two rounds of changes"
            }
            .into(),
        }
    }

    fn observation(
        state: TurnState,
        checks: Vec<CheckResult>,
        review: ReviewOutcome,
    ) -> TurnObservation {
        TurnObservation {
            session_id: "ses-1".into(),
            turn_id: "turn-1".into(),
            state,
            attention_reason: None,
            nodes: vec![NodeObservation {
                node_id: "node-writer".into(),
                slot_id: "writer".into(),
                model: ModelIdentity {
                    provider: "openrouter".into(),
                    model: "qwen/qwen3-coder".into(),
                    runtime: "native".into(),
                },
                required: true,
                kind: "slot".into(),
                generations: vec![GenerationObservation {
                    generation: 1,
                    state: if state == TurnState::Completed {
                        NodeState::Accepted
                    } else {
                        NodeState::Stopped
                    },
                    answer: Some("done".into()),
                    failure: None,
                }],
            }],
            checks,
            review: Some(review),
            usage: RunUsage {
                input_tokens: 10,
                output_tokens: 5,
                cost_microunits: 7,
                complete: true,
                retries: 0,
            },
        }
    }

    #[derive(Default)]
    struct FakeHost {
        /// Observations `wait_turn` returns, in order; once they run out,
        /// the last one again (a turn that ended stays ended).
        waits: Mutex<Vec<TurnObservation>>,
        last_wait: Mutex<Option<TurnObservation>>,
        applied: Mutex<Vec<SessionTeamEdit>>,
        stopped: Mutex<Vec<String>>,
        events: Mutex<Vec<RunEvent>>,
        finished: Mutex<Option<RunOutcome>>,
    }

    impl FakeHost {
        /// The recorded events in order: a phase's name, `ended` for the
        /// `Ended` event and `event` for any other.
        fn phases(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|event| match event {
                    RunEvent::Phase { phase, .. } => phase.clone(),
                    RunEvent::Ended { .. } => "ended".into(),
                    _ => "event".into(),
                })
                .collect()
        }
    }

    #[async_trait]
    impl RunHost for FakeHost {
        async fn apply_team(&self, _: &str, edit: SessionTeamEdit) -> Result<(), RunError> {
            self.applied.lock().unwrap().push(edit);
            Ok(())
        }
        async fn send_turn(&self, _: &str, request: &str) -> Result<String, RunError> {
            assert_eq!(request, "tidy up");
            Ok("turn-1".into())
        }
        async fn wait_turn(
            &self,
            _: &str,
            _: &str,
            _: Instant,
        ) -> Result<TurnObservation, RunError> {
            let mut waits = self.waits.lock().unwrap();
            let mut last = self.last_wait.lock().unwrap();
            if !waits.is_empty() {
                *last = Some(waits.remove(0));
            }
            Ok(last.clone().expect("a scripted observation"))
        }
        async fn stop_turn(&self, _: &str, turn_id: &str) -> Result<(), RunError> {
            self.stopped.lock().unwrap().push(turn_id.into());
            Ok(())
        }
        async fn run_repro(&self, _: &str, _: &ReproRequest) -> Result<ReproRun, RunError> {
            Err(RunError::NotImplemented("fake"))
        }
        async fn read_sandbox_file(
            &self,
            _: &str,
            _: &str,
            _: usize,
        ) -> Result<Option<Vec<u8>>, RunError> {
            Ok(None)
        }
        async fn record(&self, _: &str, event: RunEvent) -> Result<(), RunError> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
        async fn recorded_events(&self, _: &str) -> Result<Vec<RunEvent>, RunError> {
            Ok(self.events.lock().unwrap().clone())
        }
        async fn finish(&self, _: &str, outcome: &RunOutcome) -> Result<(), RunError> {
            *self.finished.lock().unwrap() = Some(outcome.clone());
            Ok(())
        }
    }

    fn host(waits: Vec<TurnObservation>) -> FakeHost {
        FakeHost {
            waits: Mutex::new(waits),
            ..FakeHost::default()
        }
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(600)
    }

    #[tokio::test]
    async fn a_passing_run_exits_zero_and_is_recorded() {
        let run = context(&[], later());
        let host = host(vec![observation(
            TurnState::Completed,
            vec![check(CheckState::Passed)],
            review(true, "openai/gpt-oss-120b"),
        )]);
        let outcome = run_to_outcome(&host, &run).await.unwrap();
        assert_eq!(
            outcome.exit_code,
            exit_code::PASS,
            "{:?}",
            outcome.attention
        );
        assert_eq!(outcome.verdict, RunVerdict::Pass);
        assert_eq!(outcome.loadout.id, "tidy");
        assert_eq!(outcome.loadout.version, 2);
        assert!(!outcome.loadout.builtin || outcome.loadout.kind == "custom");
        assert_eq!(outcome.turns.len(), 1);
        assert_eq!(outcome.usage.input_tokens, 10);
        assert!(outcome.warnings.is_empty());
        let applied = host.applied.lock().unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].required_checks, vec![vec!["cargo", "test"]]);
        assert_eq!(applied[0].check_options[0].timeout_ms, Some(120_000));
        assert_eq!(host.finished.lock().unwrap().as_ref(), Some(&outcome));
        let events = host.events.lock().unwrap();
        assert!(matches!(events.last(), Some(RunEvent::Ended { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, RunEvent::TurnStarted { .. })));
    }

    #[tokio::test]
    async fn a_failed_check_exits_one() {
        let run = context(&[], later());
        let host = host(vec![observation(
            TurnState::NeedsAttention,
            vec![check(CheckState::Failed)],
            review(true, "openai/gpt-oss-120b"),
        )]);
        let outcome = run_to_outcome(&host, &run).await.unwrap();
        assert_eq!(outcome.exit_code, exit_code::CHECKS_FAILED);
        // The turn needed attention: it is stopped before the run ends, so
        // the Session no longer holds the Workspace, and the Outcome keeps
        // it as observed.
        assert_eq!(
            host.stopped.lock().unwrap().as_slice(),
            ["turn-1", "turn-1"]
        );
        assert_eq!(outcome.turns[0].state, TurnState::NeedsAttention);
        let phases = host.phases();
        let closing = phases.iter().position(|phase| phase == "closing_turn");
        let ended = phases.iter().position(|phase| phase == "ended");
        assert!(closing.unwrap() < ended.unwrap(), "{phases:?}");
    }

    /// Whatever the verdict, no turn the run started stays open once the run
    /// has its Outcome: a turn that needs attention (review not passed), one
    /// a failing driver left unobserved, and none of a passing run.
    #[tokio::test]
    async fn every_ended_run_closes_its_open_turns() {
        // Review not passed: needs attention, exit 2.
        let run = context(&[], later());
        let mut review_failed = observation(
            TurnState::NeedsAttention,
            vec![check(CheckState::Passed)],
            review(false, "openai/gpt-oss-120b"),
        );
        review_failed.nodes[0].generations[0].state = NodeState::Accepted;
        let host = host(vec![review_failed]);
        let outcome = run_to_outcome(&host, &run).await.unwrap();
        assert_eq!(outcome.exit_code, exit_code::NEEDS_ATTENTION);
        assert_eq!(
            host.stopped.lock().unwrap().as_slice(),
            ["turn-1", "turn-1"]
        );

        // Passing: its completed turn already released the Workspace.
        let host = super::tests::host(vec![observation(
            TurnState::Completed,
            vec![check(CheckState::Passed)],
            review(true, "openai/gpt-oss-120b"),
        )]);
        let outcome = run_to_outcome(&host, &run).await.unwrap();
        assert_eq!(outcome.exit_code, exit_code::PASS);
        assert!(host.stopped.lock().unwrap().is_empty());
        assert!(!host.phases().iter().any(|phase| phase == "closing_turn"));

        // A driver that started a turn and then failed before observing it:
        // exit 5, and the turn it started is still closed.
        struct StartsThenFails;
        #[async_trait]
        impl KindDriver for StartsThenFails {
            async fn drive(
                &self,
                host: &dyn RunHost,
                run: &RunContext,
            ) -> Result<KindReport, RunError> {
                host.record(
                    &run.run_id,
                    RunEvent::TurnStarted {
                        at_ms: now_ms(),
                        turn_id: "turn-7".into(),
                        purpose: "run".into(),
                    },
                )
                .await?;
                Err(RunError::Infrastructure(
                    "the turn could not be observed".into(),
                ))
            }
        }
        let host = super::tests::host(vec![observation(
            TurnState::NeedsAttention,
            vec![],
            review(true, "openai/gpt-oss-120b"),
        )]);
        let outcome = run_to_outcome_with(&host, &run, &StartsThenFails)
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, exit_code::INFRASTRUCTURE);
        assert_eq!(
            host.stopped.lock().unwrap().as_slice(),
            ["turn-7", "turn-7"]
        );
    }

    /// A node that failed with an empty reason is not covered with a reason
    /// that says so, never `other: ` with nothing after it.
    #[test]
    fn an_empty_failure_reason_is_never_the_not_covered_detail() {
        let mut turn = observation(
            TurnState::Stopped,
            vec![],
            review(true, "openai/gpt-oss-120b"),
        );
        turn.nodes[0].generations[0].state = NodeState::Failed;
        turn.nodes[0].generations[0].failure = Some(NodeFailure {
            class: FailureClass::Other,
            message: "  ".into(),
        });
        let entries = scan_not_covered(std::slice::from_ref(&turn), false);
        assert_eq!(entries[0].detail, "writer ended without a result");
        turn.nodes[0].generations[0].state = NodeState::Stopped;
        let entries = scan_not_covered(std::slice::from_ref(&turn), false);
        assert_eq!(entries[0].class, FailureClass::Stopped);
        assert_eq!(entries[0].detail, "writer ended without a result");
        // A class the observation gave is kept.
        turn.nodes[0].generations[0].failure = Some(NodeFailure {
            class: FailureClass::Budget,
            message: String::new(),
        });
        let entries = scan_not_covered(std::slice::from_ref(&turn), false);
        assert_eq!(entries[0].class, FailureClass::Budget);
    }

    #[tokio::test]
    async fn a_review_that_did_not_pass_needs_attention_and_a_same_model_reviewer_is_warned() {
        let run = context(
            &[("reviewer_model", "openrouter:qwen/qwen3-coder")],
            later(),
        );
        let mut done = observation(
            TurnState::NeedsAttention,
            vec![check(CheckState::Passed)],
            review(false, "qwen/qwen3-coder"),
        );
        done.nodes[0].generations[0].state = NodeState::Accepted;
        let host = host(vec![done]);
        let outcome = run_to_outcome(&host, &run).await.unwrap();
        assert_eq!(outcome.exit_code, exit_code::NEEDS_ATTENTION);
        assert!(outcome
            .attention
            .iter()
            .any(|reason| reason.contains("review did not pass")));
        assert_eq!(
            outcome
                .warnings
                .iter()
                .filter(|warning| warning.code == SAME_MODEL_REVIEWER)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn the_wall_clock_stops_the_turn_and_needs_attention() {
        let run = context(&[], Instant::now());
        let running = observation(
            TurnState::Running,
            vec![],
            review(true, "openai/gpt-oss-120b"),
        );
        let stopped = observation(
            TurnState::Stopped,
            vec![check(CheckState::NotRun)],
            review(true, "openai/gpt-oss-120b"),
        );
        let host = host(vec![running, stopped]);
        let outcome = run_to_outcome(&host, &run).await.unwrap();
        // The wall clock's Stop, then the Stop that settles the stopped turn
        // so its Session releases the Workspace.
        assert_eq!(
            host.stopped.lock().unwrap().as_slice(),
            ["turn-1", "turn-1"]
        );
        assert_eq!(outcome.exit_code, exit_code::NEEDS_ATTENTION);
        assert!(outcome
            .attention
            .iter()
            .any(|reason| reason.contains("wall clock")));
        assert_eq!(outcome.not_covered.len(), 1);
        assert_eq!(outcome.not_covered[0].class, FailureClass::Budget);
    }

    #[tokio::test]
    async fn a_stop_before_the_turn_is_an_interruption() {
        struct Stopping(FakeHost);
        #[async_trait]
        impl RunHost for Stopping {
            async fn apply_team(&self, s: &str, edit: SessionTeamEdit) -> Result<(), RunError> {
                self.0.apply_team(s, edit).await
            }
            async fn send_turn(&self, _: &str, _: &str) -> Result<String, RunError> {
                panic!("no turn after a stop")
            }
            async fn wait_turn(
                &self,
                s: &str,
                t: &str,
                d: Instant,
            ) -> Result<TurnObservation, RunError> {
                self.0.wait_turn(s, t, d).await
            }
            async fn stop_turn(&self, s: &str, t: &str) -> Result<(), RunError> {
                self.0.stop_turn(s, t).await
            }
            async fn run_repro(&self, s: &str, r: &ReproRequest) -> Result<ReproRun, RunError> {
                self.0.run_repro(s, r).await
            }
            async fn read_sandbox_file(
                &self,
                s: &str,
                p: &str,
                m: usize,
            ) -> Result<Option<Vec<u8>>, RunError> {
                self.0.read_sandbox_file(s, p, m).await
            }
            async fn record(&self, r: &str, e: RunEvent) -> Result<(), RunError> {
                self.0.record(r, e).await
            }
            async fn stop_requested(&self, _: &str) -> bool {
                true
            }
        }
        let run = context(&[], later());
        let host = Stopping(host(vec![]));
        let outcome = run_to_outcome(&host, &run).await.unwrap();
        assert_eq!(outcome.exit_code, exit_code::INTERRUPTED);
        assert_eq!(outcome.verdict, RunVerdict::Interrupted);
    }

    struct Unfinished;

    #[async_trait]
    impl KindDriver for Unfinished {
        async fn drive(&self, _: &dyn RunHost, _: &RunContext) -> Result<KindReport, RunError> {
            Err(RunError::NotImplemented("loadout::fix::FixDriver"))
        }
    }

    #[tokio::test]
    async fn an_unfinished_hook_is_an_infrastructure_error() {
        let run = context(&[], later());
        let host = host(vec![]);
        let outcome = run_to_outcome_with(&host, &run, &Unfinished).await.unwrap();
        assert_eq!(outcome.exit_code, exit_code::INFRASTRUCTURE);
        assert_eq!(outcome.verdict, RunVerdict::Error);
        assert!(outcome
            .error
            .as_deref()
            .unwrap()
            .contains("not implemented"));
        assert!(host.finished.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn a_failed_node_is_not_covered() {
        let run = context(&[], later());
        let mut done = observation(
            TurnState::Completed,
            vec![check(CheckState::Passed)],
            review(true, "openai/gpt-oss-120b"),
        );
        let mut helper = done.nodes[0].clone();
        helper.node_id = "node-helper".into();
        helper.slot_id = "scout".into();
        helper.kind = "helper".into();
        helper.required = false;
        helper.generations[0].state = NodeState::Failed;
        helper.generations[0].failure = Some(NodeFailure {
            class: FailureClass::ProviderRefusal,
            message: "classifier stop".into(),
        });
        done.nodes.push(helper);
        let host = host(vec![done]);
        let outcome = run_to_outcome(&host, &run).await.unwrap();
        assert_eq!(outcome.exit_code, exit_code::NEEDS_ATTENTION);
        assert_eq!(outcome.not_covered[0].area, "scout");
        assert_eq!(outcome.not_covered[0].class, FailureClass::ProviderRefusal);
        assert!(host
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, RunEvent::NotCovered { .. })));
    }
}
