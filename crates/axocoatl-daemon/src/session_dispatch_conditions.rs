//! Durable repository-check lifecycle. The parent host owns the actual resource,
//! writer guard, backend start and process cleanup. These private permits never
//! resolve a path or accept a caller-provided readiness verdict.
use super::*;
use axocoatl_session::control_authority::{AuthorityError, ConditionCallClaim};
use axocoatl_session::execution_content::{
    ConditionOutputCapture, ConditionOutputEvidence, ConditionProcessStatus,
    ConditionSupervisionEvidence, DurableConditionArguments, DurableConditionResult,
};

/// Consumed once, with another current authority check at the execution edge.
/// Dropping this value proves only that this particular permit never dispatched.
pub(super) struct PreparedRepositoryCheck {
    controller: SessionDispatchController,
    arguments: DurableConditionArguments,
    claim: Option<ConditionCallClaim>,
}

/// Once returned to the owned backend, losing this handle cannot prove that the
/// process never ran. Its Drop stops current activation leases and interrupts
/// only its unresolved current epoch; the turn authority remains recoverable.
pub(super) struct InFlightRepositoryCheck {
    controller: SessionDispatchController,
    arguments: DurableConditionArguments,
    claim: Option<ConditionCallClaim>,
}

/// Durable output and canonical effect/readiness state are deliberately distinct.
/// An unresolved result cannot release a repository writer solely because its
/// observed stdout/stderr were saved successfully.
pub struct SettledRepositoryCheck {
    pub result: DurableConditionResult,
    /// The retained result establishes an observed outcome or no dispatch.
    /// This does not prove process-tree quiescence or safe writer release.
    pub outcome_known: bool,
    /// The exact effect resolution exists in canonical history. A known late
    /// result cannot add that resolution to an already closed immutable turn.
    pub canonical_effect_resolved: bool,
    pub verdict: Option<ConditionOutcome>,
}

impl PreparedRepositoryCheck {
    pub(super) fn arguments(&self) -> &DurableConditionArguments {
        &self.arguments
    }

    /// Called by the owned host immediately before process dispatch. There is
    /// no await while the controller and authority inspect current state.
    #[cfg(test)]
    pub(super) fn begin_dispatch(mut self) -> Result<InFlightRepositoryCheck> {
        let validation = {
            let state = self.controller.lock()?;
            state.ready()?;
            let claim = self
                .claim
                .as_ref()
                .ok_or_else(|| error("check permit was already consumed"))?;
            state
                .authority
                .validate_condition_claim(&state.canonical, claim, now_ms()?)
        };
        if let Err(refused) = validation {
            let mut state = self.controller.lock()?;
            if definitive_authority_refusal(&refused) {
                let result = state.condition_not_dispatched(&self.arguments, self.claim.as_ref());
                self.claim.take();
                state.fail_closed(result)?;
            } else {
                // A storage failure is not evidence of absent dispatch history.
                self.claim.take();
                return state.fail_closed(Err(error(refused)));
            }
            return Err(error(refused));
        }
        Ok(InFlightRepositoryCheck {
            controller: self.controller.clone(),
            arguments: self.arguments.clone(),
            claim: self.claim.take(),
        })
    }

    /// The last authority check, resource arming, cancellation registration and
    /// supervisor handoff share the controller gate. A Stop cannot interleave
    /// between admission and the owned transport's synchronous handoff.
    pub(super) fn dispatch_supervised(
        mut self,
        lease: &mut crate::bootstrap::session_repository::SessionRepositoryExecutionLease,
        command: axocoatl_isolation::supervisor_transport::PreparedSupervisedCommand,
    ) -> Result<(
        InFlightRepositoryCheck,
        axocoatl_isolation::supervisor_transport::RunningSupervisedCommand,
    )> {
        let controller = self.controller.clone();
        let mut state = controller.lock()?;
        state.ready()?;
        let claim = self
            .claim
            .as_ref()
            .ok_or_else(|| error("check permit already consumed"))?;
        state
            .authority
            .validate_condition_claim(&state.canonical, claim, now_ms()?)
            .map_err(error)?;
        lease.mark_dispatched().map_err(error)?;
        state
            .repository_checks
            .insert(self.arguments.run().run_id.clone(), command.cancellation());
        let running = command.dispatch().map_err(error)?;
        Ok((
            InFlightRepositoryCheck {
                controller: self.controller.clone(),
                arguments: self.arguments.clone(),
                claim: self.claim.take(),
            },
            running,
        ))
    }
}

impl Drop for PreparedRepositoryCheck {
    fn drop(&mut self) {
        let Some(claim) = self.claim.take() else {
            return;
        };
        if let Ok(mut state) = self.controller.lock() {
            let result = state.condition_not_dispatched(&self.arguments, Some(&claim));
            let _ = state.fail_closed(result);
        }
    }
}

impl InFlightRepositoryCheck {
    #[cfg(test)]
    pub(super) fn arguments(&self) -> &DurableConditionArguments {
        &self.arguments
    }

    /// Status and captures come only from the owned backend. Permission to
    /// release its resource requires actual backend cleanup in addition to
    /// whatever observed outcome is retained here.
    #[cfg(test)]
    pub(super) fn settle(
        self,
        status: ConditionProcessStatus,
        stdout: ConditionOutputEvidence,
        stderr: ConditionOutputEvidence,
        recorded_at_unix_ms: u64,
    ) -> Result<SettledRepositoryCheck> {
        if status == ConditionProcessStatus::NotDispatched {
            return Err(error(
                "dispatched check cannot claim it was never dispatched",
            ));
        }
        self.settle_observation(status, stdout, stderr, recorded_at_unix_ms, None)
    }

    pub(super) fn settle_observation(
        mut self,
        status: ConditionProcessStatus,
        stdout: ConditionOutputEvidence,
        stderr: ConditionOutputEvidence,
        recorded_at_unix_ms: u64,
        supervision: Option<ConditionSupervisionEvidence>,
    ) -> Result<SettledRepositoryCheck> {
        if status == ConditionProcessStatus::NotDispatched
            && !supervision
                .as_ref()
                .is_some_and(|proof| proof.quiescent && !proof.launched)
        {
            return Err(error(
                "no-dispatch observation requires the exact supervised acknowledgement",
            ));
        }
        let mut state = self.controller.lock()?;
        let claim = self
            .claim
            .as_ref()
            .ok_or_else(|| error("check execution was already consumed"))?;
        let result = (|| {
            let result = state
                .content
                .record_condition_supervised_result(
                    &self.arguments,
                    status,
                    stdout,
                    stderr,
                    recorded_at_unix_ms,
                    supervision,
                )
                .map_err(error)?;
            state
                .authority
                .settle_condition_run(claim, &result)
                .map_err(error)?;
            state.finish_condition_result(result)
        })();
        // Retained unknown outcomes remain unknown; the host keeps the process
        // owner. On failed persistence Drop must not invent a different result.
        self.claim.take();
        state.repository_checks.remove(&self.arguments.run().run_id);
        state.fail_closed(result)
    }
}

impl Drop for InFlightRepositoryCheck {
    fn drop(&mut self) {
        if self.claim.take().is_none() {
            return;
        }
        if let Ok(mut state) = self.controller.lock() {
            let result = state.interrupt_condition_epoch(self.arguments.run());
            let _ = state.fail_closed(result);
        }
    }
}

impl SessionDispatchController {
    /// Private host port: repository and isolation arguments must come from the
    /// parent's owned resource capability, not an untrusted request body.
    pub(super) fn prepare_repository_check(
        &self,
        run: ConditionRunRef,
        repository: EvidenceRef,
        grant: GrantSnapshotRef,
        isolation: &str,
    ) -> Result<PreparedRepositoryCheck> {
        let mut state = self.lock()?;
        state.ready()?;
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        if run.turn_id != state.turn_id || run.session_id != snapshot.owner().session_id {
            return Err(error("condition belongs to another Session or turn"));
        }
        // Prior intents are never resumed as executable work, even if the
        // current store contains no authority claim or final output for them.
        if snapshot.contract().condition_run(&run.run_id).is_some() {
            return Err(error(
                "condition intent already exists; reconcile evidence without replay",
            ));
        }
        let arguments =
            match state
                .content
                .reserve_condition_arguments(&snapshot, &run, &repository)
            {
                Ok(arguments) => arguments,
                Err(problem) => return content_admission_error(&mut state, problem),
            };
        let result = state.append(
            &condition_command(&run.run_id, "intent")?,
            TurnContractEvent::RecordConditionIntent {
                run,
                intent: arguments.reference().clone(),
            },
        );
        state.fail_closed(result)?;
        let claimed = state.authority.claim_condition_run(
            &state.canonical,
            &state.content,
            &arguments,
            &grant,
            isolation,
            now_ms()?,
        );
        let claim = match claimed {
            Ok(claim) => claim,
            Err(problem) if definitive_authority_refusal(&problem) => {
                // This stack frame has never released a permit to the backend.
                // Persist that positive fact, rather than reasoning from absence
                // of a claim during a later recovery attempt.
                let result = state.condition_not_dispatched(&arguments, None);
                state.fail_closed(result)?;
                return Err(error(problem));
            }
            Err(problem) => return state.fail_closed(Err(error(problem))),
        };
        state.changed.notify_waiters();
        Ok(PreparedRepositoryCheck {
            controller: self.clone(),
            arguments,
            claim: Some(claim),
        })
    }

    /// Evidence reconciliation does not mint a check permit or start a process.
    /// The daemon reconciles when it opens a controller and at lifecycle
    /// boundaries; tests call this directly.
    #[cfg(test)]
    pub fn reconcile_repository_checks(&self) -> Result<()> {
        let mut state = self.lock()?;
        state.ready()?;
        let result = state.reconcile_conditions();
        state.fail_closed(result)
    }
}

impl DispatchState {
    pub(super) fn reconcile_conditions(&mut self) -> Result<()> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        for run in snapshot.contract().condition_runs() {
            let arguments = self
                .content
                .condition_arguments(&snapshot, &run.run.run_id)
                .map_err(error)?
                .ok_or_else(|| error("canonical condition intent lacks retained arguments"))?;
            if arguments.run() != &run.run || arguments.reference() != &run.intent {
                return Err(error(
                    "retained condition arguments differ from canonical intent",
                ));
            }
            let result = self.content.condition_result(&arguments).map_err(error)?;
            let call = self
                .authority
                .condition_call(&run.run.run_id)
                .map_err(error)?;
            match (result, call) {
                (Some(result), Some(_)) => {
                    let receipt = self
                        .authority
                        .condition_settlement_receipt(&run.run.run_id)
                        .map_err(error)?;
                    self.authority
                        .reconcile_condition_run(&receipt, &result)
                        .map_err(error)?;
                    self.finish_condition_result(result)?;
                }
                (Some(result), None)
                    if *result.status() == ConditionProcessStatus::NotDispatched =>
                {
                    self.finish_condition_result(result)?;
                }
                (Some(_), None) => {
                    return Err(error(
                        "observed check execution has no corresponding authority claim",
                    ))
                }
                (None, Some(call)) if call.result.is_some() => {
                    return Err(error("settled condition authority lacks retained result"))
                }
                (None, _) if run.resolution.is_some() => {
                    return Err(error("resolved condition lacks retained result evidence"))
                }
                (None, _) => (), // Positive evidence is required; absence stays unknown.
            }
        }
        Ok(())
    }

    fn condition_not_dispatched(
        &mut self,
        arguments: &DurableConditionArguments,
        claim: Option<&ConditionCallClaim>,
    ) -> Result<SettledRepositoryCheck> {
        let result = self
            .content
            .record_condition_result(
                arguments,
                ConditionProcessStatus::NotDispatched,
                ConditionOutputCapture::new(arguments.definition().stdout_bytes)
                    .map_err(error)?
                    .finish(false),
                ConditionOutputCapture::new(arguments.definition().stderr_bytes)
                    .map_err(error)?
                    .finish(false),
                now_ms()?,
            )
            .map_err(error)?;
        if let Some(claim) = claim {
            self.authority
                .settle_condition_run(claim, &result)
                .map_err(error)?;
        }
        self.finish_condition_result(result)
    }

    fn finish_condition_result(
        &mut self,
        result: DurableConditionResult,
    ) -> Result<SettledRepositoryCheck> {
        let run = result.arguments().run();
        let disposition = condition_disposition(&result);
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let canonical = snapshot
            .contract()
            .condition_run(&run.run_id)
            .ok_or_else(|| error("condition result has no canonical intent"))?;
        if &canonical.run != run || &canonical.intent != result.arguments().reference() {
            return Err(error("condition result belongs to another intent"));
        }
        let Some(resolution) = disposition else {
            if canonical.resolution.is_some() {
                return Err(error(
                    "uncertain condition output conflicts with resolved intent",
                ));
            }
            self.changed.notify_waiters();
            return Ok(SettledRepositoryCheck {
                result,
                outcome_known: false,
                canonical_effect_resolved: false,
                verdict: None,
            });
        };
        if let Some(recorded) = &canonical.resolution {
            if recorded != &resolution {
                return Err(error(
                    "condition effect resolution conflicts with retained result",
                ));
            }
        } else if !snapshot
            .contract()
            .state()
            .is_some_and(LogicalTurnState::is_closed)
        {
            self.append(
                &condition_command(&run.run_id, "effect")?,
                TurnContractEvent::ResolveConditionIntent {
                    run_id: run.run_id.clone(),
                    resolution: resolution.clone(),
                },
            )?;
        }
        let proposed = condition_verdict(&result);
        let current = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let contract = current.contract();
        let canonical_effect_resolved = contract
            .condition_run(&run.run_id)
            .is_some_and(|recorded| recorded.resolution.as_ref() == Some(&resolution));
        let mut verdict = None;
        if let Some(outcome) = proposed {
            if contract
                .current_condition(&run.condition_id)
                .is_some_and(|observation| {
                    observation.epoch_id == run.epoch_id
                        && observation.activations == run.activations
                        && observation.evidence == *result.reference()
                        && observation.outcome == outcome
                })
            {
                verdict = Some(outcome);
            } else if condition_scope_current(contract, run)
                && contract.current_condition(&run.condition_id).is_none()
                && !contract.condition_runs().iter().any(|item| {
                    item.run.condition_id == run.condition_id && item.resolution.is_none()
                })
            {
                self.append(
                    &condition_command(&run.run_id, "verdict")?,
                    TurnContractEvent::RecordCondition {
                        epoch_id: run.epoch_id.clone(),
                        condition_id: run.condition_id.clone(),
                        activations: run.activations.clone(),
                        outcome,
                        evidence: result.reference().clone(),
                    },
                )?;
                verdict = Some(outcome);
            }
        }
        self.changed.notify_waiters();
        Ok(SettledRepositoryCheck {
            result,
            outcome_known: true,
            canonical_effect_resolved,
            verdict,
        })
    }

    fn interrupt_condition_epoch(&mut self, run: &ConditionRunRef) -> Result<()> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let recorded = snapshot
            .contract()
            .condition_run(&run.run_id)
            .ok_or_else(|| error("dropped check has no canonical intent"))?;
        if &recorded.run != run {
            return Err(error("dropped check belongs to another intent"));
        }
        // A settled old handle cannot stop a new epoch or permanently close the
        // turn. Unknown process ownership is retained by the separate supervisor.
        if recorded.resolution.is_some()
            || snapshot.contract().state() != Some(LogicalTurnState::Running)
            || !snapshot
                .contract()
                .epochs()
                .last()
                .is_some_and(|epoch| epoch.id == run.epoch_id && epoch.state == EpochState::Running)
        {
            return Ok(());
        }
        for item in snapshot.contract().activations().iter().filter(|item| {
            item.state == ActivationState::Running
                && item.activation.execution_epoch_id == run.epoch_id
        }) {
            if let Some(bound) = self.bound.get(&item.activation.activation_id) {
                if bound.activation != item.activation {
                    return Err(error("running activation executor identity differs"));
                }
                self.authority
                    .stop_activation(&bound.activation, self.authority.revision().map_err(error)?)
                    .map_err(error)?;
                bound.control.cancel();
            } else {
                // As in driver interruption, positively account for a factory
                // wait before replacing Running with Interrupted. Missing
                // authority records alone can never be treated as zero usage.
                let ActivationEvidenceContent::Definition { profile, .. } = &self
                    .content
                    .resolve_activation_evidence(&item.input.definition.snapshot)
                    .map_err(error)?
                else {
                    return Err(error(
                        "unbound activation has no retained execution definition",
                    ));
                };
                self.authority
                    .record_undispatched_activation(
                        &snapshot,
                        &item.activation,
                        profile.clone(),
                        self.authority.revision().map_err(error)?,
                    )
                    .map_err(error)?;
            }
        }
        self.append(
            &condition_command(&run.run_id, "interrupted")?,
            TurnContractEvent::InterruptEpoch {
                epoch_id: run.epoch_id.clone(),
            },
        )?;
        self.changed.notify_waiters();
        Ok(())
    }
}

fn condition_command(run: &ConditionRunId, stage: &str) -> Result<String> {
    let digest = Sha256::digest(run.as_str().as_bytes());
    let command = format!("condition-{stage}-{digest:x}");
    CommandId::new(&command).map_err(error)?;
    Ok(command)
}

fn condition_scope_current(contract: &TurnContract, run: &ConditionRunRef) -> bool {
    !contract.state().is_none_or(LogicalTurnState::is_closed)
        && contract
            .epochs()
            .last()
            .is_some_and(|epoch| epoch.id == run.epoch_id)
        && contract
            .graph()
            .and_then(|graph| {
                graph
                    .conditions
                    .iter()
                    .find(|condition| condition.condition_id == run.condition_id)
            })
            .is_some_and(|condition| {
                condition.nodes.len() == run.activations.len()
                    && run.activations.iter().all(|activation| {
                        condition.nodes.contains(&activation.node_id)
                            && contract
                                .current_accepted_activations()
                                .iter()
                                .any(|accepted| accepted.activation == *activation)
                    })
            })
}

fn condition_verdict(result: &DurableConditionResult) -> Option<ConditionOutcome> {
    if result.supervision().is_some_and(|proof| !proof.quiescent) {
        return None;
    }
    if !result.stdout().complete() || !result.stderr().complete() {
        return None;
    }
    match result.status() {
        ConditionProcessStatus::Exited { code: 0 } => Some(ConditionOutcome::Passed),
        ConditionProcessStatus::Exited { .. } | ConditionProcessStatus::Signalled { .. } => {
            Some(ConditionOutcome::Failed)
        }
        ConditionProcessStatus::TimedOut
        | ConditionProcessStatus::Interrupted
        | ConditionProcessStatus::LaunchFailed { .. }
            if result.supervision().is_some_and(|proof| proof.quiescent) =>
        {
            Some(ConditionOutcome::Failed)
        }
        _ => None,
    }
}

fn condition_disposition(result: &DurableConditionResult) -> Option<ConditionEffectResolution> {
    if let Some(proof) = result.supervision() {
        if !proof.quiescent || matches!(result.status(), ConditionProcessStatus::Uncertain { .. }) {
            return None;
        }
        if *result.status() != ConditionProcessStatus::NotDispatched {
            return Some(ConditionEffectResolution::OutcomeRecorded {
                evidence: result.reference().clone(),
            });
        }
    }
    if *result.status() == ConditionProcessStatus::NotDispatched {
        Some(ConditionEffectResolution::NotDispatched {
            evidence: result.reference().clone(),
        })
    } else {
        condition_verdict(result).map(|_| ConditionEffectResolution::OutcomeRecorded {
            evidence: result.reference().clone(),
        })
    }
}

fn definitive_authority_refusal(problem: &AuthorityError) -> bool {
    matches!(
        problem,
        AuthorityError::Denied
            | AuthorityError::StaleLease
            | AuthorityError::Capacity
            | AuthorityError::Invalid(_)
    )
}

fn content_admission_error<T>(
    state: &mut DispatchState,
    problem: axocoatl_session::execution_content::ExecutionContentError,
) -> Result<T> {
    use axocoatl_session::execution_content::ExecutionContentError;
    match problem {
        ExecutionContentError::Invalid(_)
        | ExecutionContentError::OwnerMismatch
        | ExecutionContentError::Conflict
        | ExecutionContentError::Capacity => Err(error(problem)),
        _ => state.fail_closed(Err(error(problem))),
    }
}

#[cfg(all(test, unix))]
#[path = "session_dispatch_conditions_tests.rs"]
mod tests;
