//! Generation-bound model control port. Every mutation uses the ordinary command journal.
use super::*;
use axocoatl_session::control_command::{
    CommandReceiptView, CommandSourceRecord, ControlCommandRequest, ControlCommandState,
};
use axocoatl_tools::{BuiltinTool, ToolError};
use serde::Deserialize;

pub(super) const NAME: &str = "coordination_control";
#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Call {
    Inspect,
    ReplaceFuture {
        request: axocoatl_actor::ChildExecutionRequest,
        target: TurnNodeId,
        rewire_dependents: Vec<TurnNodeId>,
    },
    Submit {
        request: Box<HumanControlActionRequest>,
        #[serde(default)]
        knowledge: Vec<super::knowledge::KnowledgeReference>,
    },
    Canonical {
        request: ControlCommandRequest,
    },
}
/// Opaque proof of the one canonical revision added by this exact tool call's
/// own admission. The original model request remains unchanged in its receipt.
pub(super) struct ControlInvocationAdmission {
    invocation: InvocationId,
    request: ControlCommandRequest,
    source: CommandSourceRecord,
    admitted_revision: u64,
    inspect_offer: Option<EvidenceRef>,
}
impl ControlInvocationAdmission {
    pub(super) fn matches(&self, view: &CommandReceiptView, revision: u64) -> bool {
        self.request == view.request
            && self.source == view.source
            && self.admitted_revision == revision
    }
    pub(super) fn invocation(&self) -> &InvocationId {
        &self.invocation
    }
    pub(super) fn inspect_offer(&self) -> Option<&EvidenceRef> {
        self.inspect_offer.as_ref()
    }
}

impl DispatchState {
    /// An inspect response is recorded before its tool outcome advances the
    /// canonical revision. A later submit can consume that exact offer only
    /// across the inspect outcome and its own admission, with no intervening
    /// canonical work or changes to the offered target.
    fn control_inspect_offer(
        &self,
        snapshot: &DurableTurnSnapshot,
        activation: &ActivationRef,
        request: &ControlCommandRequest,
        live_scope: &str,
        previous: &TurnContractEnvelope,
        arguments: &serde_json::Value,
    ) -> Result<Option<EvidenceRef>> {
        let TurnContractEvent::RecordOutcome {
            invocation_id,
            outcome: InvocationOutcome::Succeeded,
            evidence,
        } = &previous.event
        else {
            return Ok(None);
        };
        if previous.expected_revision != request.expected_turn_revision {
            return Ok(None);
        }
        let Some(audited) = self.audit.invocation(invocation_id).map_err(error)? else {
            return Ok(None);
        };
        if audited.intent.activation != *activation
            || audited.intent.dispatch_scope != live_scope
            || audited.intent.tool_name != NAME
        {
            return Ok(None);
        }
        let Some(retained_arguments) = self
            .content
            .tool_arguments(snapshot, activation, invocation_id)
            .map_err(error)?
        else {
            return Ok(None);
        };
        let inspected: serde_json::Value = serde_json::from_slice(
            &self
                .content
                .read_tool_arguments(&retained_arguments)
                .map_err(error)?,
        )
        .map_err(error)?;
        if inspected != serde_json::json!({"operation":"inspect"}) {
            return Ok(None);
        }
        let Some(retained_result) = self
            .content
            .tool_result(&retained_arguments)
            .map_err(error)?
        else {
            return Ok(None);
        };
        if retained_result.is_truncated()
            || retained_result.outcome() != InvocationOutcome::Succeeded
            || retained_result.protected_result().evidence_ref != *evidence
            || !matches!(&audited.final_evidence,
                Some(InvocationFinalEvidence::Outcome {
                    outcome: InvocationOutcome::Succeeded,
                    result,
                    source: InvocationOutcomeSource::Executor,
                    ..
                }) if result == retained_result.protected_result())
        {
            return Ok(None);
        }
        #[derive(Deserialize)]
        struct InspectOffer {
            source: ActivationRef,
            turn_revision: u64,
            graph: TurnGraphSnapshot,
            legal_actions: Vec<HumanControlActionRequest>,
        }
        let result: std::result::Result<InspectOffer, String> = serde_json::from_slice(
            &self
                .content
                .read_tool_result(&retained_result)
                .map_err(error)?,
        )
        .map_err(error)?;
        let Ok(offer) = result else {
            return Ok(None);
        };
        let Call::Submit {
            request: submitted, ..
        } = serde_json::from_value(arguments.clone()).map_err(error)?
        else {
            return Ok(None);
        };
        if offer.source != *activation
            || offer.turn_revision != request.expected_turn_revision
            || offer.graph.revision != request.expected_graph_revision
            || !offer.legal_actions.into_iter().any(|mut offered| {
                // These are the model's documented editable fields. Every
                // identity, revision and selected target remains exact.
                offered.command_id = submitted.command_id.clone();
                if matches!(
                    offered.action,
                    HumanControlAction::Guide | HumanControlAction::Revise
                ) {
                    offered.instruction = submitted.instruction.clone();
                }
                offered == *submitted
            })
        {
            return Ok(None);
        }
        Ok(Some(evidence.clone()))
    }

    fn control_invocation_admission(
        &self,
        activation: &ActivationRef,
        request: &ControlCommandRequest,
        source: &CommandSourceRecord,
        arguments: &serde_json::Value,
    ) -> Result<Option<ControlInvocationAdmission>> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let revision = snapshot.contract().revision();
        let direct = request.expected_turn_revision.checked_add(1) == Some(revision);
        let from_inspect = request.expected_turn_revision.checked_add(2) == Some(revision);
        if !direct && !from_inspect {
            return Ok(None);
        }
        let mut records = self
            .canonical
            .records()
            .map_err(error)?
            .iter()
            .rev()
            .filter(|record| record.turn_id == self.turn_id);
        let Some(record) = records.next() else {
            return Ok(None);
        };
        let TurnContractEvent::RecordIntent {
            invocation_id,
            activation: intent_activation,
        } = &record.event
        else {
            return Ok(None);
        };
        if record.expected_revision.checked_add(1) != Some(revision)
            || intent_activation != activation
        {
            return Ok(None);
        }
        let CommandSourceRecord::Agent {
            activation: source_activation,
            live_scope,
            ..
        } = source
        else {
            return Ok(None);
        };
        let Some(audited) = self.audit.invocation(invocation_id).map_err(error)? else {
            return Ok(None);
        };
        if source_activation != activation
            || audited.intent.activation != *activation
            || audited.intent.tool_name != NAME
            || audited.intent.dispatch_scope != *live_scope
            || audited.final_evidence.is_some()
        {
            return Ok(None);
        }
        let Some(retained) = self
            .content
            .tool_arguments(&snapshot, activation, invocation_id)
            .map_err(error)?
        else {
            return Ok(None);
        };
        let actual: serde_json::Value =
            serde_json::from_slice(&self.content.read_tool_arguments(&retained).map_err(error)?)
                .map_err(error)?;
        if &actual != arguments {
            return Ok(None);
        }
        let inspect_offer = if from_inspect {
            let Some(previous) = records.next() else {
                return Ok(None);
            };
            let Some(evidence) = self.control_inspect_offer(
                &snapshot, activation, request, live_scope, previous, arguments,
            )?
            else {
                return Ok(None);
            };
            Some(evidence)
        } else {
            None
        };
        Ok(Some(ControlInvocationAdmission {
            invocation: invocation_id.clone(),
            request: request.clone(),
            source: source.clone(),
            admitted_revision: revision,
            inspect_offer,
        }))
    }
}

struct ControlTool {
    controller: SessionDispatchController,
    activation: ActivationRef,
}
impl SessionDispatchController {
    pub(super) fn scoped_control_tool(
        &self,
        activation: &ActivationRef,
    ) -> Result<Option<Arc<dyn BuiltinTool>>> {
        let state = self.lock()?;
        let bound = state
            .bound
            .get(&activation.activation_id)
            .filter(|bound| bound.activation == *activation)
            .ok_or_else(|| error("control source is not bound"))?;
        let policy = state
            .authority
            .grant_policy(bound.grant.grant_id.as_str())
            .map_err(error)?;
        Ok(policy
            .delegation
            .as_ref()
            .filter(|_| policy.holder == activation.node_id)
            .map(|_| {
                Arc::new(ControlTool {
                    controller: self.clone(),
                    activation: activation.clone(),
                }) as Arc<dyn BuiltinTool>
            }))
    }
    fn scoped_control(
        &self,
        activation: &ActivationRef,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let bytes = serde_json::to_vec(&arguments).map_err(error)?;
        if bytes.len() > axocoatl_session::control_command::MAX_CONTROL_REQUEST_BYTES {
            return Err(error("control request exceeds the bounded command size"));
        }
        let call: Call = serde_json::from_slice(&bytes).map_err(error)?;
        let mut state = self.lock()?;
        state.execution_admission()?;
        let bound = state
            .bound
            .get(&activation.activation_id)
            .filter(|bound| bound.activation == *activation)
            .cloned()
            .ok_or_else(|| error("control source has no exact live owner"))?;
        let source = state
            .authority
            .attest_control_source(&bound.lease, now_ms()?)
            .map_err(error)?;
        let policy = state
            .authority
            .grant_policy(bound.grant.grant_id.as_str())
            .map_err(error)?;
        if policy.holder != activation.node_id || policy.delegation.is_none() {
            return Err(error(
                "control requires the explicit delegated holder grant",
            ));
        }
        if matches!(call, Call::Inspect) {
            let snapshot = state.current(activation)?;
            let graph = snapshot
                .contract()
                .graph()
                .ok_or_else(|| error("current graph is absent"))?;
            let mut legal = vec![];
            let mut targets: Vec<_> = snapshot
                .contract()
                .activations()
                .iter()
                .flat_map(|target| {
                    [
                        HumanControlAction::Stop,
                        HumanControlAction::Guide,
                        HumanControlAction::Retry,
                        HumanControlAction::Revise,
                    ]
                    .map(|action| (Some(target.activation.clone()), action))
                })
                .collect();
            targets.push((None, HumanControlAction::Finish));
            for (target, action) in targets {
                let request = HumanControlActionRequest {
                    schema_version: 1,
                    command_id: CommandId::new(format!(
                        "offer-{:x}",
                        Sha256::digest(
                            serde_json::to_vec(&(
                                activation,
                                &target,
                                action,
                                snapshot.contract().revision()
                            ))
                            .map_err(error)?
                        )
                    ))
                    .map_err(error)?,
                    session_id: activation.session_id.clone(),
                    turn_id: activation.turn_id.clone(),
                    execution_epoch_id: snapshot
                        .contract()
                        .epochs()
                        .last()
                        .ok_or_else(|| error("epoch is absent"))?
                        .id
                        .clone(),
                    expected_turn_revision: snapshot.contract().revision(),
                    expected_graph_revision: graph.revision,
                    activation: target,
                    action,
                    instruction: matches!(
                        action,
                        HumanControlAction::Guide | HumanControlAction::Revise
                    )
                    .then(|| "Replace this instruction with the specific follow-up".into()),
                    include_previous_output: false,
                    context: None,
                    continuation: None,
                    blocker_id: None,
                    human_response: None,
                    partial_finish: None,
                };
                let Some(evidence) = snapshot.request_ref() else {
                    continue;
                };
                if let Ok(command) =
                    state.build_human_control(&request, now_ms()?, evidence, Some(evidence), true)
                {
                    let view = CommandReceiptView {
                        request: command,
                        source: source.record().clone(),
                        state: ControlCommandState::Requested,
                        revision: 1,
                        last_transition: None,
                    };
                    if state.preview_agent_control(&view).is_ok() {
                        legal.push(request);
                    }
                }
            }
            return Ok(
                serde_json::json!({"source":activation,"grant":policy,"turn_revision":snapshot.contract().revision(),"graph":graph,"legal_actions":legal,"receipts":state.control_plane_commands()?.into_iter().filter(|view|matches!(&view.source,CommandSourceRecord::Agent{activation:owner,..}if owner==activation)).collect::<Vec<_>>(),"note":"Use a new stable command_id for new content. Exact retries return the original receipt. Receipt acceptance does not mean settlement. Human approvals and grant expansion require human review."}),
            );
        }
        if let Call::ReplaceFuture {
            request,
            target,
            rewire_dependents,
        } = call
        {
            drop(state);
            self.replace_coordinator_future(activation, &request, target, rewire_dependents)?;
            return Ok(
                serde_json::json!({"state":"admitted","note":"Inspect the exact graph and command receipts for settlement."}),
            );
        }
        let canonical = match call {
            Call::Submit { request, knowledge } => {
                request.validate()?;
                if !knowledge.is_empty()
                    && !matches!(
                        request.action,
                        HumanControlAction::Guide | HumanControlAction::Revise
                    )
                {
                    return Err(error(
                        "knowledge evidence requires a Guide or Revise follow-up",
                    ));
                }
                if request.action == HumanControlAction::Resume
                    || request.context.is_some()
                    || request.human_response.is_some()
                {
                    return Err(error("human approval and human attachment capture are unavailable to Agent control"));
                }
                let evidence = state
                    .content
                    .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                        text: serde_json::to_string(&request).map_err(error)?,
                    })
                    .map_err(error)?
                    .reference()
                    .clone();
                let instruction = request
                    .instruction
                    .as_ref()
                    .map(|text| {
                        let text = state.knowledge_instruction(activation, text, &knowledge)?;
                        state
                            .content
                            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                                text,
                            })
                            .map(|receipt| receipt.reference().clone())
                            .map_err(error)
                    })
                    .transpose()?;
                let issued = state
                    .commands
                    .receipt(&request.command_id)
                    .map_err(error)?
                    .map(|receipt| receipt.view().request.issued_at_ms)
                    .unwrap_or(now_ms()?);
                state.build_human_control(
                    &request,
                    issued,
                    &evidence,
                    instruction.as_ref(),
                    false,
                )?
            }
            Call::Canonical { request } => {
                ControlCommandRequest::decode(&serde_json::to_vec(&request).map_err(error)?)
                    .map_err(error)?
            }
            Call::Inspect | Call::ReplaceFuture { .. } => {
                return Err(error("invalid control operation"))
            }
        };
        if canonical.session_id != activation.session_id
            || canonical.turn_id != activation.turn_id
            || canonical.parameters.delegated_operation().is_none()
        {
            return Err(error(
                "control is outside this exact task or requires a human decision",
            ));
        }
        let own_admission = state.control_invocation_admission(
            activation,
            &canonical,
            source.record(),
            &arguments,
        )?;
        let receipt =
            state.submit_control_command_from_tool(canonical, source, own_admission.as_ref())?;
        serde_json::to_value(receipt.view()).map_err(error)
    }
}
#[async_trait]
impl BuiltinTool for ControlTool {
    fn description(&self) -> &str {
        "Inspect exact legal work controls and submit generation-bound commands under your approved grant. Returns a durable receipt promptly; inspect later for settlement. Never approves a human wait or expands its own grant. For Guide/Revise follow-ups, knowledge accepts up to four exact note revisions or proposals from this activation; the host captures them in the command evidence."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","required":["operation"],"properties":{"operation":{"enum":["inspect","submit","canonical","expand","replace_future"]},"target":{"type":"string"},"rewire_dependents":{"type":"array","items":{"type":"string"}},"request":{"type":"object","description":"Exact typed request from inspect; for canonical operations use the existing ControlCommandRequest schema and retained inputs."},"knowledge":{"type":"array","maxItems":4,"items":{"type":"object","required":["kind","id"],"properties":{"kind":{"enum":["note","proposal"]},"id":{"type":"string"},"revision":{"type":"integer","minimum":1}},"additionalProperties":false},"description":"Only for submit Guide/Revise. Note needs an exact revision; proposal must come from this activation."}},"additionalProperties":false})
    }
    fn advertised_parameters_schema(&self) -> Option<serde_json::Value> {
        let state = self.controller.lock().ok()?;
        let bound = state.bound.get(&self.activation.activation_id)?;
        state
            .authority
            .attest_control_source(&bound.lease, now_ms().ok()?)
            .ok()?;
        Some(self.parameters_schema())
    }
    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        axocoatl_llm::ConcurrencyPolicy::Exclusive
    }
    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, ToolError> {
        let result = if arguments["operation"] == "expand" {
            match serde_json::from_value::<SessionGrantChange>(arguments["request"].clone()) {
                Ok(request) => {
                    self.controller
                        .propose_grant_change(&self.activation, request)
                        .await
                }
                Err(reason) => Err(error(reason)),
            }
        } else {
            self.controller.scoped_control(&self.activation, arguments)
        };
        result.map_err(|reason| ToolError::ExecutionFailed {
            tool: NAME.into(),
            reason: reason.to_string(),
        })
    }
}
