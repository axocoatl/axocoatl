//! Authenticated-host translation of small browser actions to exact commands.
//!
//! The browser names an existing activation. Immutable retry inputs, attribution,
//! and successor identities are constructed here under the controller lock.

use super::*;
use crate::session_control_plane::{ControlPlaneCapabilities, ControlPlaneCapability};
use axocoatl_session::control_command::{
    CommandReceiptView, CommandSourceRecord, ControlCommandRequest, ControlCommandState,
    ControlParameters, TrustedCommandSource, CONTROL_COMMAND_SCHEMA_VERSION,
    MAX_CONTROL_REQUEST_BYTES,
};
use serde::{Deserialize, Serialize};
#[path = "session_dispatch_partial_finish.rs"]
mod partial_finish;
use partial_finish::{partial_finish_selection, validate_partial_finish_selection};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanControlAction {
    Stop,
    Retry,
    Guide,
    Resume,
    Revise,
    Continue,
    Finish,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanControlActionRequest {
    pub schema_version: u32,
    pub command_id: CommandId,
    pub session_id: SessionId,
    pub turn_id: LogicalTurnId,
    pub execution_epoch_id: ExecutionEpochId,
    pub expected_turn_revision: u64,
    pub expected_graph_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<ActivationRef>,
    pub action: HumanControlAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_previous_output: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<crate::session_dispatch::human_context::HumanControlContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<HumanContinuationSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker_id: Option<BlockerId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_response: Option<HumanBlockerResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_finish: Option<HumanPartialFinishSelection>,
}

/// Explicit human review, bound to the request's exact turn/graph revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanPartialFinishSelection {
    pub selected_activations: Vec<ActivationRef>,
    pub stop_activations: Vec<ActivationRef>,
    pub missing_conditions: Vec<ConditionId>,
    pub unrun_nodes: Vec<TurnNodeId>,
    pub confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HumanBlockerResponse {
    Approval,
    Decline { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanContinuationSelection {
    pub restart: Vec<ActivationRef>,
    pub checks: Vec<ConditionId>,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl HumanControlActionRequest {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_CONTROL_REQUEST_BYTES {
            return Err(error("control action exceeds the request limit"));
        }
        let request: Self = serde_json::from_slice(bytes).map_err(error)?;
        request.validate()?;
        Ok(request)
    }

    pub(super) fn validate(&self) -> Result<()> {
        if self.schema_version != CONTROL_COMMAND_SCHEMA_VERSION {
            return Err(error("unsupported control action schema"));
        }
        if self.expected_graph_revision == 0 || self.expected_turn_revision == 0 {
            return Err(error("control action has incomplete revision identity"));
        }
        match self.action {
            HumanControlAction::Stop
            | HumanControlAction::Retry
            | HumanControlAction::Guide
            | HumanControlAction::Resume
            | HumanControlAction::Revise => {
                let activation = self.required_activation()?;
                if self.session_id != activation.session_id
                    || self.turn_id != activation.turn_id
                    || activation.generation == 0
                    || (self.action != HumanControlAction::Revise
                        && self.execution_epoch_id != activation.execution_epoch_id)
                {
                    return Err(error("control action has inconsistent activation identity"));
                }
            }
            HumanControlAction::Continue | HumanControlAction::Finish
                if self.activation.is_some() =>
            {
                return Err(error("turn controls cannot borrow an activation target"));
            }
            _ => {}
        }
        if self.action == HumanControlAction::Resume {
            if self.blocker_id.is_none()
                || self.human_response.is_none()
                || matches!(&self.human_response, Some(HumanBlockerResponse::Decline { reason }) if reason.trim().is_empty())
            {
                return Err(error(
                    "Resume requires one exact blocker and an explicit human approval or denial",
                ));
            }
        } else if self.blocker_id.is_some() || self.human_response.is_some() {
            return Err(error("this action cannot carry a human blocker response"));
        }
        if matches!(
            self.action,
            HumanControlAction::Revise | HumanControlAction::Guide
        ) {
            if self
                .instruction
                .as_ref()
                .is_none_or(|text| text.trim().is_empty())
                || self.continuation.is_some()
            {
                return Err(error(
                    "guidance and revision require an instruction and no continuation plan",
                ));
            }
        } else if self.instruction.is_some() || self.include_previous_output {
            return Err(error("this action does not accept revision input"));
        }
        if let Some(context) = &self.context {
            if !matches!(
                self.action,
                HumanControlAction::Guide | HumanControlAction::Revise
            ) || context
                .references
                .len()
                .saturating_add(context.attachment_ids.len())
                > MAX_INPUT_REFERENCES
            {
                return Err(error("Only Guide and Revise accept bounded typed context"));
            }
        }
        if self.action != HumanControlAction::Revise && self.include_previous_output {
            return Err(error("only revision can select previous-output context"));
        }
        if self.action == HumanControlAction::Continue {
            let selection = self
                .continuation
                .as_ref()
                .ok_or_else(|| error("Continue requires explicit work/check selections"))?;
            if selection.restart.is_empty() && selection.checks.is_empty()
                || selection.restart.len() > MAX_CONTRACT_NODES
                || selection.checks.len() > MAX_COMPLETION_CONDITIONS
                || selection
                    .restart
                    .iter()
                    .map(|item| &item.node_id)
                    .collect::<HashSet<_>>()
                    .len()
                    != selection.restart.len()
                || selection.checks.iter().collect::<HashSet<_>>().len() != selection.checks.len()
                || selection.restart.iter().any(|item| {
                    item.session_id != self.session_id
                        || item.turn_id != self.turn_id
                        || item.generation == 0
                })
            {
                return Err(error("Continue requires bounded exact unique selections"));
            }
        } else if self.continuation.is_some() {
            return Err(error("this action does not accept continuation selections"));
        }
        if let Some(selection) = &self.partial_finish {
            if self.action != HumanControlAction::Finish
                || !selection.confirmed
                || selection.selected_activations.len() > MAX_CONTRACT_NODES
                || selection.stop_activations.len() > MAX_CONTRACT_NODES
                || selection.unrun_nodes.len() > MAX_CONTRACT_NODES
                || selection.missing_conditions.len() > MAX_COMPLETION_CONDITIONS
            {
                return Err(error(
                    "partial Finish requires bounded explicit human confirmation",
                ));
            }
        }
        Ok(())
    }

    fn required_activation(&self) -> Result<&ActivationRef> {
        self.activation
            .as_ref()
            .ok_or_else(|| error("this control requires an exact activation"))
    }
}

impl SessionDispatchController {
    /// Privileged host boundary: authenticate the human's Session access before
    /// calling. This entrypoint is crate-private and must not be an agent tool.
    #[cfg(test)]
    pub(crate) fn submit_human_action(
        &self,
        request: HumanControlActionRequest,
        issued_at_ms: u64,
    ) -> Result<CommandReceiptView> {
        self.submit_human_action_with_context(request, issued_at_ms, None)
    }

    pub(crate) fn submit_human_action_with_context(
        &self,
        request: HumanControlActionRequest,
        issued_at_ms: u64,
        prepared: Option<crate::session_dispatch::human_context::PreparedHumanControlContext>,
    ) -> Result<CommandReceiptView> {
        request.validate()?;
        let mut state = self.lock()?;
        state.ready()?;
        if request.session_id != state.canonical.owner().session_id
            || request.turn_id != state.turn_id
        {
            return Err(error("control action belongs to another Session or turn"));
        }
        // Resolve exact idempotency before revisions, ownership changes, or
        // generating the successor. An old request remains an old receipt.
        if let Some(receipt) = state.commands.receipt(&request.command_id).map_err(error)? {
            let view = receipt.view();
            let CommandSourceRecord::Human {
                session_id,
                turn_id,
                request_evidence,
            } = &view.source
            else {
                return Err(error("control command ID belongs to another source"));
            };
            if session_id != &request.session_id || turn_id != &request.turn_id {
                return Err(error("control command ID belongs to another owner"));
            }
            let ActivationEvidenceContent::Guidance { text } = &state
                .content
                .resolve_activation_evidence(request_evidence)
                .map_err(error)?
            else {
                return Err(error("original human control request is unavailable"));
            };
            let original: HumanControlActionRequest = serde_json::from_str(text).map_err(error)?;
            if original != request {
                return Err(error(
                    "control command ID was already used for different content",
                ));
            }
            return Ok(view.clone());
        }
        state.execution_admission()?;
        let retained = state
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: serde_json::to_string(&request).map_err(error)?,
            })
            .map(|receipt| receipt.reference().clone())
            .map_err(error);
        let evidence = state.fail_closed(retained)?;
        let instruction = if let Some(text) = &request.instruction {
            let instruction_text = if request.context.is_some() {
                let prepared = prepared.as_ref().ok_or_else(|| {
                    error("Typed control context was not resolved by the authenticated host")
                })?;
                crate::session_dispatch::human_context::retain_context_instruction(
                    &mut state.content,
                    &request,
                    prepared,
                )?
            } else {
                text.clone()
            };
            let retained = state
                .content
                .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                    text: instruction_text,
                })
                .map(|receipt| receipt.reference().clone())
                .map_err(error);
            Some(state.fail_closed(retained)?)
        } else {
            None
        };
        let canonical_request = state.build_human_control(
            &request,
            issued_at_ms,
            &evidence,
            instruction.as_ref(),
            false,
        )?;
        let source = TrustedCommandSource::human(request.session_id, request.turn_id, evidence);
        // Construction, receipt lookup, source retention and submission share
        // this exact lock. Concurrent duplicate requests cannot fork identity.
        state
            .submit_control_command(canonical_request, source)
            .map(|receipt| receipt.view().clone())
    }
}

impl DispatchState {
    /// The command journal owns append order and exact current receipt state.
    /// Retaining a receipt across reload never interprets Accepted as Settled.
    pub(super) fn control_plane_commands(&self) -> Result<Vec<CommandReceiptView>> {
        self.commands
            .records()
            .map_err(error)?
            .iter()
            .filter_map(|record| {
                if let axocoatl_session::control_command::ControlCommandEvent::Requested {
                    request,
                    ..
                } = &record.event
                {
                    Some(
                        self.commands
                            .receipt(&request.command_id)
                            .map_err(error)
                            .and_then(|receipt| {
                                receipt
                                    .map(|receipt| receipt.view().clone())
                                    .ok_or_else(|| error("durable command request has no receipt"))
                            }),
                    )
                } else {
                    None
                }
            })
            .collect()
    }

    pub(super) fn build_human_control(
        &self,
        request: &HumanControlActionRequest,
        issued_at_ms: u64,
        request_evidence: &EvidenceRef,
        instruction: Option<&EvidenceRef>,
        preview: bool,
    ) -> Result<ControlCommandRequest> {
        if matches!(
            request.action,
            HumanControlAction::Continue | HumanControlAction::Retry | HumanControlAction::Revise
        ) && self.is_isolated_ways()?
        {
            return Err(error("This isolated Way uses the existing Ways controls; cooperative Retry, Revise and Continue are unavailable. Inspect its result, answer a pending approval, Stop, Compare or Keep from Ways."));
        }
        let parameters = match request.action {
            HumanControlAction::Resume => ControlParameters::ResumeBlocked {
                activation: request.required_activation()?.clone(),
                blocker_id: EvidenceRef::new(
                    request
                        .blocker_id
                        .as_ref()
                        .ok_or_else(|| error("Resume has no blocker"))?
                        .as_str(),
                )
                .map_err(error)?,
                response: match request
                    .human_response
                    .as_ref()
                    .ok_or_else(|| error("Resume has no human response"))?
                {
                    HumanBlockerResponse::Approval => {
                        axocoatl_session::control_command::BlockerResponse::Approval {
                            approval: request_evidence.clone(),
                        }
                    }
                    HumanBlockerResponse::Decline { .. } => {
                        axocoatl_session::control_command::BlockerResponse::Decline {
                            reason: request_evidence.clone(),
                        }
                    }
                },
            },
            HumanControlAction::Guide => ControlParameters::SteerActivation {
                activation: request.required_activation()?.clone(),
                instruction: if preview {
                    request_evidence.clone()
                } else {
                    instruction
                        .ok_or_else(|| error("guidance instruction is not retained"))?
                        .clone()
                },
                mode: axocoatl_session::control_command::SteerMode::NextSafeBoundary,
            },
            HumanControlAction::Stop => ControlParameters::StopActivation {
                activation: request.required_activation()?.clone(),
            },
            HumanControlAction::Retry => {
                let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
                let target = request.required_activation()?;
                let previous = snapshot
                    .contract()
                    .activations()
                    .iter()
                    .find(|activation| &activation.activation == target)
                    .ok_or_else(|| error("Retry target is not recorded in this turn"))?;
                let mut input = previous.input.clone();
                // Preserve every captured input and the original savepoint.
                // Current parent/definition/grant validity is checked again by
                // the same canonical validator that admits the actual command.
                let identity =
                    serde_json::to_vec(&("human-retry-v1", snapshot.journal_id(), request))
                        .map_err(error)?;
                let digest = format!("{:x}", Sha256::digest(identity));
                input.manifest_id =
                    InputManifestId::new(format!("input-{digest}")).map_err(error)?;
                input.activation.activation_id =
                    ActivationId::new(format!("activation-{digest}")).map_err(error)?;
                input.activation.generation = input
                    .activation
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| error("activation generation overflow"))?;
                // A retry is within its existing epoch. Restart continuation
                // requires the separate explicit continuation operation.
                ControlParameters::RetryActivation {
                    activation: request.required_activation()?.clone(),
                    input: Box::new(input),
                    replay_decisions: vec![],
                }
            }
            _ => self.extended_human_parameters(request, request_evidence, instruction, preview)?,
        };
        Ok(ControlCommandRequest {
            schema_version: CONTROL_COMMAND_SCHEMA_VERSION,
            command_id: request.command_id.clone(),
            session_id: request.session_id.clone(),
            turn_id: request.turn_id.clone(),
            execution_epoch_id: request.execution_epoch_id.clone(),
            expected_turn_revision: request.expected_turn_revision,
            expected_graph_revision: request.expected_graph_revision,
            issued_at_ms,
            parameters,
        })
    }

    /// Pure capability assessment against the exact current stores and the
    /// command validator. No receipt, source evidence, or execution is created.
    pub(super) fn human_control_capabilities(
        &self,
        activation: &ActivationRef,
        issued_at_ms: u64,
    ) -> Result<ControlPlaneCapabilities> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let graph_revision = snapshot
            .contract()
            .graph()
            .ok_or_else(|| error("turn graph is unavailable"))?
            .revision;
        let epoch = snapshot
            .contract()
            .epochs()
            .last()
            .ok_or_else(|| error("turn epoch is unavailable"))?;
        let assess = |action| -> ControlPlaneCapability {
            let result = (|| {
                self.execution_admission()?;
                let request = HumanControlActionRequest {
                    schema_version: CONTROL_COMMAND_SCHEMA_VERSION,
                    command_id: CommandId::new(format!(
                        "capability-{:x}",
                        Sha256::digest(serde_json::to_vec(&(activation, action)).map_err(error)?)
                    ))
                    .map_err(error)?,
                    session_id: snapshot.owner().session_id.clone(),
                    turn_id: snapshot.turn_id().clone(),
                    execution_epoch_id: epoch.id.clone(),
                    expected_turn_revision: snapshot.contract().revision(),
                    expected_graph_revision: graph_revision,
                    activation: Some(activation.clone()),
                    action,
                    instruction: matches!(
                        action,
                        HumanControlAction::Revise | HumanControlAction::Guide
                    )
                    .then(|| "Capability assessment".into()),
                    include_previous_output: action == HumanControlAction::Revise,
                    context: None,
                    continuation: None,
                    blocker_id: None,
                    human_response: None,
                    partial_finish: None,
                };
                request.validate()?;
                let canonical = self.build_human_control(
                    &request,
                    issued_at_ms,
                    snapshot
                        .request_ref()
                        .ok_or_else(|| error("turn request is unavailable"))?,
                    None,
                    true,
                )?;
                let source = CommandSourceRecord::Human {
                    session_id: request.session_id,
                    turn_id: request.turn_id,
                    request_evidence: snapshot
                        .request_ref()
                        .ok_or_else(|| error("turn request is unavailable"))?
                        .clone(),
                };
                self.preview_human_control(&CommandReceiptView {
                    request: canonical,
                    source,
                    revision: 0,
                    state: ControlCommandState::Requested,
                    last_transition: None,
                })
            })();
            match result {
                Ok(()) => ControlPlaneCapability {
                    requires_revalidation: false,
                    enabled: true,
                    reason: String::new(),
                },
                Err(error) => ControlPlaneCapability {
                    requires_revalidation: false,
                    enabled: false,
                    reason: error.to_string(),
                },
            }
        };
        Ok(ControlPlaneCapabilities {
            inspect: true,
            human_responses: self.human_response_capabilities(activation, issued_at_ms)?,
            stop: assess(HumanControlAction::Stop),
            retry: assess(HumanControlAction::Retry),
            guide: assess(HumanControlAction::Guide),
            revise: assess(HumanControlAction::Revise),
            revise_invalidates: self
                .human_revision_invalidation(activation)
                .unwrap_or_default(),
        })
    }
}

#[path = "session_dispatch_host_control_extended.rs"]
mod extended;
pub(super) use extended::revision_selections;
pub use extended::{HumanCheckChoice, HumanContinuationChoice, HumanTurnControls};
