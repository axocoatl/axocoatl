//! A delegated revision waits for its own control-tool return. The ordinary
//! canonical revision path never bypasses unknown effects or replays on restart.
use super::control_tool::ControlInvocationAdmission;
use super::*;
use axocoatl_session::control_command::{
    CommandFailure, CommandReceiptView, CommandSourceRecord, ControlCommandEvent,
    ControlParameters, ControlTransition,
};

impl DispatchState {
    pub(super) fn revision_pending_invocation(
        &self,
        view: &CommandReceiptView,
        preview: bool,
        own_admission: Option<&ControlInvocationAdmission>,
    ) -> Result<Option<InvocationId>> {
        if !matches!(
            view.request.parameters,
            ControlParameters::ReviseActivation { .. }
        ) {
            return Ok(None);
        }
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        if let Some(proof) =
            own_admission.filter(|proof| proof.matches(view, snapshot.contract().revision()))
        {
            return Ok(Some(proof.invocation().clone()));
        }
        let CommandSourceRecord::Agent {
            activation,
            live_scope,
            ..
        } = &view.source
        else {
            return Ok(None);
        };
        if !preview {
            return Ok(None);
        }
        let Some(last) = self
            .canonical
            .records()
            .map_err(error)?
            .iter()
            .rev()
            .find(|record| record.turn_id == self.turn_id)
        else {
            return Ok(None);
        };
        let TurnContractEvent::RecordIntent {
            invocation_id,
            activation: owner,
        } = &last.event
        else {
            return Ok(None);
        };
        if owner != activation
            || last.expected_revision.checked_add(1) != Some(snapshot.contract().revision())
        {
            return Ok(None);
        }
        let Some(audit) = self.audit.invocation(invocation_id).map_err(error)? else {
            return Ok(None);
        };
        if audit.intent.activation != *activation
            || audit.intent.tool_name != control_tool::NAME
            || audit.intent.dispatch_scope != *live_scope
            || audit.final_evidence.is_some()
        {
            return Ok(None);
        }
        let Some(arguments) = self
            .content
            .tool_arguments(&snapshot, activation, invocation_id)
            .map_err(error)?
        else {
            return Ok(None);
        };
        let value: serde_json::Value = serde_json::from_slice(
            &self
                .content
                .read_tool_arguments(&arguments)
                .map_err(error)?,
        )
        .map_err(error)?;
        if value != serde_json::json!({"operation":"inspect"}) {
            return Ok(None);
        }
        Ok(Some(invocation_id.clone()))
    }

    pub(super) fn deferred_revision_invocation(
        &self,
        view: &CommandReceiptView,
    ) -> Result<Option<(InvocationId, u64)>> {
        if !matches!(
            view.request.parameters,
            ControlParameters::ReviseActivation { .. }
        ) || !matches!(view.source, CommandSourceRecord::Agent { .. })
        {
            return Ok(None);
        }
        let accepted = self
            .commands
            .records()
            .map_err(error)?
            .iter()
            .find_map(|record| match &record.event {
                ControlCommandEvent::Transition { update }
                    if update.command_id == view.request.command_id =>
                {
                    match &update.transition {
                        ControlTransition::Accepted { validation, .. } => Some(validation),
                        _ => None,
                    }
                }
                _ => None,
            });
        let Some(accepted) = accepted else {
            return Ok(None);
        };
        let ActivationEvidenceContent::Guidance { text } = self
            .content
            .resolve_activation_evidence(accepted)
            .map_err(error)?
        else {
            return Err(error(
                "deferred revision validation has the wrong evidence type",
            ));
        };
        let validation: serde_json::Value = serde_json::from_str(text).map_err(error)?;
        let Some(invocation) = validation["invocation_admission"].as_str() else {
            return Ok(None);
        };
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        if validation["kind"] != "control-validation-v1"
            || validation["canonical_journal"] != snapshot.journal_id()
            || validation["source"] != serde_json::to_value(&view.source).map_err(error)?
            || validation["parameters"]
                != serde_json::to_value(&view.request.parameters).map_err(error)?
            || validation["request"]
                != serde_json::to_value(self.command_request_evidence(view)?).map_err(error)?
            || validation["graph_revision"] != view.request.expected_graph_revision
        {
            return Err(error(
                "deferred revision differs from its retained validation",
            ));
        }
        let revision = validation["turn_revision"]
            .as_u64()
            .ok_or_else(|| error("deferred revision lacks its admitted revision"))?;
        let invocation = InvocationId::new(invocation).map_err(error)?;
        let CommandSourceRecord::Agent {
            activation,
            live_scope,
            grant_id,
            grant_revision,
            ..
        } = &view.source
        else {
            unreachable!()
        };
        let audit = self
            .audit
            .invocation(&invocation)
            .map_err(error)?
            .ok_or_else(|| error("deferred revision has no admitted invocation"))?;
        if audit.intent.activation != *activation || audit.intent.tool_name != control_tool::NAME
            || audit.intent.dispatch_scope != *live_scope || audit.intent.authority.grant_id != *grant_id
            || audit.intent.authority.grant_revision != *grant_revision
            || !self.canonical.records().map_err(error)?.iter().any(|record| record.turn_id == self.turn_id
                && record.expected_revision.checked_add(1) == Some(revision)
                && matches!(&record.event, TurnContractEvent::RecordIntent { invocation_id, activation: owner } if *invocation_id == invocation && owner == activation)) {
            return Err(error("deferred revision lost its exact invocation admission"));
        }
        Ok(Some((invocation, revision)))
    }

    pub(super) fn reconcile_deferred_revision(
        &mut self,
        view: &CommandReceiptView,
    ) -> Result<bool> {
        let Some((invocation, admitted_revision)) = self.deferred_revision_invocation(view)? else {
            return Ok(false);
        };
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let audit = self
            .audit
            .invocation(&invocation)
            .map_err(error)?
            .ok_or_else(|| error("deferred invocation disappeared"))?;
        if audit.final_evidence.is_none() {
            return Ok(true);
        }
        let check = (|| {
            let Some(InvocationFinalEvidence::Outcome {
                outcome: InvocationOutcome::Succeeded,
                result,
                ..
            }) = &audit.final_evidence
            else {
                return Err(error(
                    "revision control tool did not return a successful receipt",
                ));
            };
            let subsequent: Vec<_> = self
                .canonical
                .records()
                .map_err(error)?
                .iter()
                .filter(|record| {
                    record.turn_id == self.turn_id && record.expected_revision >= admitted_revision
                })
                .collect();
            if subsequent.len() != 1
                || !matches!(&subsequent[0].event,
                TurnContractEvent::RecordOutcome { invocation_id, outcome: InvocationOutcome::Succeeded, evidence }
                if *invocation_id == invocation && evidence == &result.evidence_ref)
            {
                return Err(error(
                    "canonical work changed while the revision receipt was pending",
                ));
            }
            let mut revalidated = view.clone();
            revalidated.request.expected_turn_revision = snapshot.contract().revision();
            self.validate_control(&revalidated)
        })();
        if let Err(reason) = check {
            self.command_update(
                view,
                ControlTransition::Failed {
                    failure: CommandFailure {
                        code: "deferred_revision_not_applied".into(),
                        message: reason.to_string().chars().take(255).collect(),
                        evidence: None,
                        blocker: None,
                    },
                },
            )?;
            return Ok(true);
        }
        let ControlParameters::ReviseActivation {
            activation,
            input,
            instruction,
            invalidate,
        } = &view.request.parameters
        else {
            unreachable!()
        };
        let envelope = self.control_envelope(
            view,
            TurnContractEvent::ReviseAccepted {
                previous: activation.clone(),
                input: input.clone(),
                invalidated_descendants: invalidate.clone(),
                evidence: instruction.clone(),
            },
        )?;
        self.canonical.append(envelope.clone()).map_err(error)?;
        self.changed.notify_waiters();
        self.settle_control_event(view, &envelope)?;
        Ok(true)
    }
}
