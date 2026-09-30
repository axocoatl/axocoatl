//! Authoritative lookup for the first-party command adapter. No tool or command
//! is executed here. A command receipt is usable only with its exact original
//! invocation admission, source scope and protected request bytes.
use super::*;
use axocoatl_session::control_command::{
    CommandReceiptView, CommandSourceRecord, ControlCommandEvent, ControlCommandRequest,
    ControlParameters, ControlTransition,
};
use axocoatl_session::invocation_audit::ProtectedArguments;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlLookupPolicy {
    schema_version: u32,
    adapter: String,
    invocation_id: InvocationId,
    activation: ActivationRef,
    arguments: ProtectedArguments,
    command_id: CommandId,
}
#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum LookupCall {
    Submit { request: HumanControlActionRequest },
    Canonical { request: ControlCommandRequest },
}
impl LookupCall {
    fn command_id(&self) -> &CommandId {
        match self {
            Self::Submit { request } => &request.command_id,
            Self::Canonical { request } => &request.command_id,
        }
    }
    fn matches(&self, receipt: &CommandReceiptView) -> bool {
        match self {
            Self::Canonical { request } => request == &receipt.request,
            Self::Submit { request } => {
                request.schema_version == receipt.request.schema_version
                    && request.command_id == receipt.request.command_id
                    && request.session_id == receipt.request.session_id
                    && request.turn_id == receipt.request.turn_id
                    && request.execution_epoch_id == receipt.request.execution_epoch_id
                    && request.expected_turn_revision == receipt.request.expected_turn_revision
                    && request.expected_graph_revision == receipt.request.expected_graph_revision
            }
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandValidation {
    kind: String,
    request: EvidenceRef,
    canonical_journal: String,
    turn_revision: u64,
    graph_revision: u64,
    source: CommandSourceRecord,
    parameters: ControlParameters,
    invocation_admission: Option<InvocationId>,
    inspect_offer: Option<EvidenceRef>,
}

impl DispatchState {
    pub(super) fn control_lookup_policy(
        &mut self,
        activation: &ActivationRef,
        invocation: &InvocationId,
        request: &ToolInvocationRequest,
        arguments: &DurableToolArguments,
    ) -> Result<InvocationReplayPolicy> {
        if request.tool_call.name != control_tool::NAME {
            return Ok(InvocationReplayPolicy::ManualOnly);
        }
        let Ok(call) = serde_json::from_value::<LookupCall>(request.tool_call.arguments.clone())
        else {
            return Ok(InvocationReplayPolicy::ManualOnly);
        };
        let policy = ControlLookupPolicy {
            schema_version: 1,
            adapter: "coordination-control-journal-v1".into(),
            invocation_id: invocation.clone(),
            activation: activation.clone(),
            arguments: arguments.protected_arguments().clone(),
            command_id: call.command_id().clone(),
        };
        let policy_ref = self
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: serde_json::to_string(&policy).map_err(error)?,
            })
            .map_err(error)?
            .reference()
            .clone();
        Ok(InvocationReplayPolicy::ReconcileBeforeReplay { policy_ref })
    }

    /// A missing lookup or mismatched provenance is still Outcome unknown. In
    /// particular, an arbitrary matching local command ID is insufficient.
    pub(super) fn reconcile_control_tool_outcome(
        &mut self,
        snapshot: &DurableTurnSnapshot,
        intent: &InvocationIntent,
        arguments: &DurableToolArguments,
    ) -> Result<bool> {
        if intent.tool_name != control_tool::NAME || intent.activation.turn_id != self.turn_id {
            return Ok(false);
        }
        let InvocationReplayPolicy::ReconcileBeforeReplay { policy_ref } = &intent.replay_policy
        else {
            return Ok(false);
        };
        let ActivationEvidenceContent::Guidance { text } = self
            .content
            .resolve_activation_evidence(policy_ref)
            .map_err(error)?
        else {
            return Ok(false);
        };
        let Ok(policy) = serde_json::from_str::<ControlLookupPolicy>(text) else {
            return Ok(false);
        };
        let bytes = self.content.read_tool_arguments(arguments).map_err(error)?;
        let Ok(call) = serde_json::from_slice::<LookupCall>(&bytes) else {
            return Ok(false);
        };
        let expected = ControlLookupPolicy {
            schema_version: 1,
            adapter: "coordination-control-journal-v1".into(),
            invocation_id: intent.invocation_id.clone(),
            activation: intent.activation.clone(),
            arguments: intent.arguments.clone(),
            command_id: call.command_id().clone(),
        };
        if policy != expected || arguments.protected_arguments() != &intent.arguments {
            return Ok(false);
        }
        let Some(receipt) = self.commands.receipt(&policy.command_id).map_err(error)? else {
            return Ok(false);
        };
        let view = receipt.view().clone();
        if !call.matches(&view)
            || view.request.session_id != intent.activation.session_id
            || view.request.turn_id != intent.activation.turn_id
            || view.request.execution_epoch_id != intent.activation.execution_epoch_id
        {
            return Ok(false);
        }
        let CommandSourceRecord::Agent {
            activation,
            grant_id,
            grant_revision,
            live_scope,
            ..
        } = &view.source
        else {
            return Ok(false);
        };
        if activation != &intent.activation
            || grant_id != &intent.authority.grant_id
            || *grant_revision != intent.authority.grant_revision
            || live_scope != &intent.dispatch_scope
        {
            return Ok(false);
        }
        let accepted = self
            .commands
            .records()
            .map_err(error)?
            .iter()
            .find_map(|record| match &record.event {
                ControlCommandEvent::Transition { update }
                    if update.command_id == policy.command_id =>
                {
                    match &update.transition {
                        ControlTransition::Accepted {
                            validation,
                            pending,
                        } => Some((validation.clone(), pending.clone())),
                        _ => None,
                    }
                }
                _ => None,
            });
        let Some((validation_ref, pending)) = accepted else {
            return Ok(false);
        };
        let ActivationEvidenceContent::Guidance { text } = self
            .content
            .resolve_activation_evidence(&validation_ref)
            .map_err(error)?
        else {
            return Ok(false);
        };
        let Ok(validation) = serde_json::from_str::<CommandValidation>(text) else {
            return Ok(false);
        };
        let admitted_revision = self.canonical.records().map_err(error)?.iter().find_map(|record| {
            (record.turn_id == intent.activation.turn_id && matches!(&record.event,
                TurnContractEvent::RecordIntent { invocation_id, activation } if invocation_id == &intent.invocation_id && activation == &intent.activation))
                .then(|| record.expected_revision.checked_add(1)).flatten()
        });
        if validation.kind != "control-validation-v1"
            || validation.canonical_journal != snapshot.journal_id()
            || Some(validation.turn_revision) != admitted_revision
            || validation.graph_revision != view.request.expected_graph_revision
            || validation.request != pending
            || pending != self.command_request_evidence(&view)?
            || validation.source != view.source
            || validation.parameters != view.request.parameters
            || validation.invocation_admission.as_ref() != Some(&intent.invocation_id)
        {
            return Ok(false);
        }
        // This is a newly observed authoritative receipt, not a claim that the
        // original process returned these bytes or that the command settled.
        let observed = serde_json::json!({
            "receipt": view,
            "reconciliation": { "adapter": policy.adapter, "invocation_id": intent.invocation_id,
                "validation": validation_ref, "inspect_offer": validation.inspect_offer,
                "note": "Read the exact persisted command receipt; no command was executed again." }
        });
        let returned: std::result::Result<serde_json::Value, String> = Ok(observed);
        self.content
            .record_tool_result(
                arguments,
                InvocationOutcome::Succeeded,
                &serde_json::to_vec(&returned).map_err(error)?,
                now_ms()?,
            )
            .map_err(error)?;
        Ok(true)
    }
}

#[cfg(test)]
impl SessionDispatchController {
    pub(crate) fn control_tool_recovery_evidence_for_test(
        &self,
    ) -> (
        axocoatl_session::invocation_audit::AuditedInvocation,
        Option<serde_json::Value>,
    ) {
        let state = self.lock().unwrap();
        let intent = state
            .audit
            .records()
            .unwrap()
            .iter()
            .find_map(|record| match &record.command {
                InvocationAuditCommand::Intent(command)
                    if command.intent.tool_name == control_tool::NAME =>
                {
                    Some(command.intent.clone())
                }
                _ => None,
            })
            .unwrap();
        let snapshot = state
            .canonical
            .snapshot(&intent.activation.turn_id)
            .unwrap();
        let arguments = state
            .content
            .tool_arguments(&snapshot, &intent.activation, &intent.invocation_id)
            .unwrap()
            .unwrap();
        let result = state
            .content
            .tool_result(&arguments)
            .unwrap()
            .map(|result| {
                serde_json::from_slice(&state.content.read_tool_result(&result).unwrap()).unwrap()
            });
        (
            state
                .audit
                .invocation(&intent.invocation_id)
                .unwrap()
                .unwrap()
                .clone(),
            result,
        )
    }

    pub(crate) fn reject_unbound_control_reconciliation_for_test(&self) {
        let (audited, result) = self.control_tool_recovery_evidence_for_test();
        assert!(result.is_none());
        let mut state = self.lock().unwrap();
        let intent = audited.intent;
        let snapshot = state
            .canonical
            .snapshot(&intent.activation.turn_id)
            .unwrap();
        let arguments = state
            .content
            .tool_arguments(&snapshot, &intent.activation, &intent.invocation_id)
            .unwrap()
            .unwrap();
        let mut mismatches = Vec::new();
        let mut changed = intent.clone();
        changed.dispatch_scope.push_str("-other");
        mismatches.push(changed);
        let mut changed = intent.clone();
        changed.authority.grant_revision += 1;
        mismatches.push(changed);
        let mut changed = intent.clone();
        changed.invocation_id = InvocationId::new("another-invocation").unwrap();
        mismatches.push(changed);
        let mut changed = intent.clone();
        changed.activation.generation += 1;
        mismatches.push(changed);
        let mut changed = intent.clone();
        changed.replay_policy = InvocationReplayPolicy::ManualOnly;
        mismatches.push(changed);
        let mut changed = intent;
        changed.tool_name = "bash".into();
        mismatches.push(changed);
        for changed in mismatches {
            assert!(!state
                .reconcile_control_tool_outcome(&snapshot, &changed, &arguments)
                .unwrap());
            assert!(state.content.tool_result(&arguments).unwrap().is_none());
        }
    }
}
