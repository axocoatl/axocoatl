//! Canonical evidence around the existing isolated candidate task owner.
use super::*;

impl DispatchState {
    pub(super) fn is_isolated_ways(&self) -> Result<bool> {
        Ok(self
            .content
            .turn_admission(&self.canonical, &self.turn_id)
            .map_err(error)?
            .is_some_and(|(_, admission)| {
                serde_json::from_str::<crate::bootstrap::native_ways::NativeWaysAdmission>(
                    &admission.source,
                )
                .is_ok()
            }))
    }
}

impl SessionDispatchController {
    pub(crate) fn native_way_route(
        &self,
        activation: &ActivationRef,
    ) -> Result<Vec<crate::trajectory::Action>> {
        let state = self.lock()?;
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        let mut actions = vec![];
        for invocation in snapshot
            .contract()
            .invocations()
            .iter()
            .filter(|invocation| invocation.activation == *activation)
        {
            let audited = state
                .audit
                .invocation(&invocation.invocation_id)
                .map_err(error)?
                .ok_or_else(|| error("Way invocation is missing its audit"))?;
            let arguments = state
                .content
                .tool_arguments(&snapshot, activation, &invocation.invocation_id)
                .map_err(error)?
                .ok_or_else(|| error("Way invocation arguments are missing"))?;
            let bytes = state
                .content
                .read_tool_arguments(&arguments)
                .map_err(error)?;
            let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(error)?;
            let mut action = crate::trajectory::Action::from_call(
                actions.len(),
                &audited.intent.tool_name,
                Some(&value),
            );
            action.failed = matches!(
                &audited.final_evidence,
                Some(
                    axocoatl_session::invocation_audit::InvocationFinalEvidence::Outcome {
                        outcome: InvocationOutcome::Failed,
                        ..
                    }
                )
            );
            actions.push(action);
        }
        Ok(actions)
    }

    /// The existing Ways tasks call this only after a candidate future settles.
    /// Closing the exploration does not select a winner or authorize Keep.
    pub(crate) fn settle_native_way_task(
        &self,
        activation: &ActivationRef,
        failure: Option<&str>,
    ) -> Result<bool> {
        let mut state = self.lock()?;
        state.ready()?;
        let mut snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        if snapshot
            .contract()
            .state()
            .is_some_and(LogicalTurnState::is_closed)
        {
            return Ok(true);
        }
        if let Some(reason) = failure {
            if snapshot.contract().activations().iter().any(|item| {
                item.activation == *activation && item.state == ActivationState::Running
            }) {
                let reference = state
                    .content
                    .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                        text: format!(
                            "Isolated Way execution failed: {}",
                            reason.chars().take(4096).collect::<String>()
                        ),
                    })
                    .map_err(error)?
                    .reference()
                    .clone();
                let envelope = driver::driver_event(
                    &state,
                    "ways-failed",
                    TurnContractEvent::FailActivation {
                        activation: activation.clone(),
                        evidence: reference,
                    },
                )?;
                state.canonical.append(envelope).map_err(error)?;
                snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
            }
        }
        if snapshot.contract().activations().iter().any(|item| {
            matches!(
                item.state,
                ActivationState::Running | ActivationState::Unstarted
            )
        }) {
            return Ok(false);
        }
        if snapshot.contract().stop_requested().is_some() {
            state.reconcile_turn_stop()?;
            return Ok(state
                .canonical
                .snapshot(&state.turn_id)
                .map_err(error)?
                .contract()
                .state()
                .is_some_and(LogicalTurnState::is_closed));
        }
        if snapshot.contract().completion_satisfied() {
            let envelope = driver::driver_event(
                &state,
                "ways-completed",
                TurnContractEvent::Close {
                    closure: TurnClosure::Completed,
                },
            )?;
            state.close_and_promote(envelope)?;
            Ok(true)
        } else {
            let epoch_id = snapshot
                .contract()
                .epochs()
                .last()
                .ok_or_else(|| error("Way epoch is missing"))?
                .id
                .clone();
            let envelope = driver::driver_event(
                &state,
                "ways-attention",
                TurnContractEvent::PauseEpoch { epoch_id },
            )?;
            state.canonical.append(envelope).map_err(error)?;
            state.changed.notify_waiters();
            Ok(false)
        }
    }
}
