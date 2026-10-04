//! A typed human wait owned by one actual native pre-tool hook. Replay reads
//! its evidence; it never recreates the hook task or a permission to dispatch.
use super::*;
use axocoatl_tools::{HookApprovalResolution, HookRegistry};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

pub(super) struct LiveHumanWait {
    pub activation: ActivationRef,
    pub parameters: EvidenceRef,
}

struct HumanWaitOwner {
    controller: SessionDispatchController,
    activation: ActivationRef,
    blocker_id: BlockerId,
    parameters: EvidenceRef,
    control: AgentRunControl,
    finished: bool,
    // Last: hook ownership, wait cleanup and any receipt write drop first.
    _execution: super::execution_lifetime::ExecutionTicket,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeWaitCall {
    actor_id: String,
    provider_id: String,
    model_id: String,
    provider_response_group: u64,
    provider_call_index: usize,
    provider_call_count: usize,
    tool_call: axocoatl_core::ToolCall,
}
impl From<&ToolInvocationRequest> for NativeWaitCall {
    fn from(request: &ToolInvocationRequest) -> Self {
        Self {
            actor_id: request.actor_id.clone(),
            provider_id: request.provider_id.clone(),
            model_id: request.model_id.clone(),
            provider_response_group: request.provider_response_group,
            provider_call_index: request.provider_call_index,
            provider_call_count: request.provider_call_count,
            tool_call: request.tool_call.clone(),
        }
    }
}
impl NativeWaitCall {
    fn same_call(&self, request: &ToolInvocationRequest) -> bool {
        self.actor_id == request.actor_id
            && self.provider_id == request.provider_id
            && self.model_id == request.model_id
            && self.provider_response_group == request.provider_response_group
            && self.provider_call_index == request.provider_call_index
            && self.provider_call_count == request.provider_call_count
            && self.tool_call.id == request.tool_call.id
            && self.tool_call.name == request.tool_call.name
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeHumanWait {
    kind: String,
    activation: ActivationRef,
    call: NativeWaitCall,
    display: axocoatl_mcp::approval::ApprovalContext,
    timeout_ms: u64,
}
fn wait_parameters(
    content: &ExecutionContentStore,
    reference: &EvidenceRef,
) -> Result<(NativeHumanWait, String)> {
    let ActivationEvidenceContent::Guidance { text } = content
        .resolve_activation_evidence(reference)
        .map_err(error)?
    else {
        return Err(error("human wait has no retained protected parameters"));
    };
    Ok((serde_json::from_str(text).map_err(error)?, text.clone()))
}

pub(super) fn validate_retained_human_waits(
    snapshot: &DurableTurnSnapshot,
    content: &ExecutionContentStore,
) -> Result<()> {
    for item in snapshot.contract().blockers() {
        if super::grant_review::is_grant_proposal(content, item)? {
            continue;
        }
        let TurnBlockerKind::HumanApproval { approval_request } = &item.blocker.kind else {
            return Err(error(
                "machine blocker execution requires its registered proof join",
            ));
        };
        let (body, bytes) = wait_parameters(content, &item.blocker.parameters)?;
        let input = &snapshot
            .contract()
            .activations()
            .iter()
            .find(|record| record.activation == item.blocker.activation)
            .ok_or_else(|| error("human wait has no exact retained activation"))?
            .input;
        let ActivationEvidenceContent::Definition { profile, .. } = content
            .resolve_activation_evidence(&input.definition.snapshot)
            .map_err(error)?
        else {
            return Err(error("human wait definition has the wrong role"));
        };
        if approval_request != &item.blocker.parameters
            || item.blocker.safe_boundary != item.blocker.parameters
            || item.blocker.evidence != item.blocker.parameters
            || body.activation != item.blocker.activation
            || body.kind != "native-mcp-human-wait-v1"
            || item.blocker.invocation_id.is_some()
            || item.blocker.activation.session_id != snapshot.owner().session_id
            || item.blocker.grant != input.grant
            || item.blocker.blocker_id.as_str()
                != format!("human-wait-{:x}", Sha256::digest(bytes.as_bytes()))
            || item.blocker.command_id.as_str()
                != format!("open-{}", item.blocker.blocker_id.as_str())
            || body.call.actor_id != input.conversation_id.as_str()
            || body.call.provider_id != profile.provider
            || body.call.model_id != profile.model
            || !profile.tools.contains(&body.call.tool_call.name)
            || body.call.provider_response_group == 0
            || body.call.provider_call_count == 0
            || body.call.provider_call_count > MAX_INPUT_REFERENCES
            || body.call.provider_call_index >= body.call.provider_call_count
            || body.display.agent_id
                != format!(
                    "{}:{}",
                    body.activation.session_id.as_str(),
                    body.call.actor_id
                )
            || body.display.tool != body.call.tool_call.name
            || body.display.server.is_empty()
            || body.timeout_ms == 0
        {
            return Err(error("typed wait lacks exact native hook provenance"));
        }
    }
    Ok(())
}

impl SessionDispatchController {
    pub(crate) fn install_hook_registry(&self, hooks: Option<Arc<HookRegistry>>) -> Result<()> {
        let mut state = self.lock()?;
        state.ready()?;
        if !state.bound.is_empty()
            || !state.human_waits.is_empty()
            || !state.execution_lifetimes.is_idle()
        {
            return Err(error(
                "hook policy must be installed before native execution ownership",
            ));
        }
        if state
            .hooks
            .as_ref()
            .zip(hooks.as_ref())
            .is_some_and(|(old, new)| !Arc::ptr_eq(old, new))
        {
            return Err(error("cannot replace the retained hook policy"));
        }
        if state.hooks.is_some() && hooks.is_none() {
            return Err(error("cannot remove a retained hook policy"));
        }
        state.hooks = hooks;
        Ok(())
    }

    pub(super) async fn wait_for_human_approval(
        &self,
        activation: &ActivationRef,
        request: &ToolInvocationRequest,
        display: Value,
        timeout: Duration,
    ) -> Result<HookApprovalResolution> {
        let mut owner = {
            let mut state = self.lock()?;
            state.execution_admission()?;
            let snapshot = state.current(activation)?;
            let bound = state
                .bound
                .get(&activation.activation_id)
                .filter(|bound| bound.activation == *activation)
                .cloned()
                .ok_or_else(|| error("approval has no exact live actor"))?;
            if bound.control.is_cancelled()
                || request.actor_id != bound.actor_id
                || request.provider_id != bound.profile.provider
                || request.model_id != bound.profile.model
                || request.provider_response_group == 0
                || timeout.is_zero()
                || request.provider_call_count == 0
                || request.provider_call_count > MAX_INPUT_REFERENCES
                || request.provider_call_index >= request.provider_call_count
                || !bound.profile.tools.contains(&request.tool_call.name)
                || display["tool"] != request.tool_call.name
                || display["agent_id"]
                    != format!("{}:{}", activation.session_id.as_str(), bound.actor_id)
            {
                return Err(error(
                    "approval request differs from actual actor, profile, or native call",
                ));
            }
            state
                .authority
                .attest_control_source(&bound.lease, now_ms()?)
                .map_err(error)?;
            for blocker in snapshot
                .contract()
                .blockers()
                .iter()
                .filter(|item| item.blocker.activation == *activation)
            {
                if super::grant_review::is_grant_proposal(&state.content, blocker)? {
                    continue;
                }
                let (previous, _) = wait_parameters(&state.content, &blocker.blocker.parameters)?;
                if previous.call.same_call(request) {
                    return Err(error(
                        "this exact native call already has a retained human decision or lost wait",
                    ));
                }
            }
            let body = NativeHumanWait {
                kind: "native-mcp-human-wait-v1".into(),
                activation: activation.clone(),
                call: NativeWaitCall::from(request),
                display: serde_json::from_value(display).map_err(error)?,
                timeout_ms: u64::try_from(timeout.as_millis()).map_err(error)?,
            };
            let bytes = serde_json::to_vec(&body).map_err(error)?;
            let blocker_id = BlockerId::new(format!("human-wait-{:x}", Sha256::digest(&bytes)))
                .map_err(error)?;
            let execution = state.acquire_execution_ticket(self)?;
            let result = (|| {
                let parameters = state
                    .content
                    .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                        text: String::from_utf8(bytes).map_err(error)?,
                    })
                    .map_err(error)?
                    .reference()
                    .clone();
                let command_id =
                    CommandId::new(format!("open-{}", blocker_id.as_str())).map_err(error)?;
                let blocker = TypedTurnBlocker {
                    schema_version: 1,
                    blocker_id: blocker_id.clone(),
                    activation: activation.clone(),
                    kind: TurnBlockerKind::HumanApproval {
                        approval_request: parameters.clone(),
                    },
                    command_id: command_id.clone(),
                    invocation_id: None,
                    // The blocker belongs to this immutable activation input. The
                    // renewed live lease above independently authorizes the wait.
                    grant: snapshot
                        .contract()
                        .activations()
                        .iter()
                        .find(|record| record.activation == *activation)
                        .ok_or_else(|| error("human wait activation disappeared"))?
                        .input
                        .grant
                        .clone(),
                    parameters: parameters.clone(),
                    safe_boundary: parameters.clone(),
                    evidence: parameters.clone(),
                };
                state.append(
                    command_id.as_str(),
                    TurnContractEvent::OpenBlocker { blocker },
                )?;
                state.human_waits.insert(
                    blocker_id.clone(),
                    LiveHumanWait {
                        activation: activation.clone(),
                        parameters: parameters.clone(),
                    },
                );
                Ok(parameters)
            })();
            let parameters = state.fail_closed(result)?;
            state.changed.notify_waiters();
            HumanWaitOwner {
                controller: self.clone(),
                activation: activation.clone(),
                blocker_id,
                parameters,
                control: bound.control,
                finished: false,
                _execution: execution,
            }
        };
        let publish = {
            self.lock()?
                .publish_human_wait_changed(activation, &owner.blocker_id)
        };
        publish?;
        let changed = self.lock()?.changed.clone();
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            let notification = changed.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            {
                let mut state = self.lock()?;
                if owner.control.is_cancelled() || state.current(activation).is_err() {
                    return Ok(HookApprovalResolution::Denied {
                        reason: "The exact approval wait ended or lost live ownership.".into(),
                    });
                }
                let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
                let blocker = snapshot
                    .contract()
                    .blockers()
                    .iter()
                    .find(|item| item.blocker.blocker_id == owner.blocker_id)
                    .ok_or_else(|| error("retained human blocker disappeared"))?;
                match &blocker.state {
                    TurnBlockerState::Pending => {}
                    TurnBlockerState::Resolved { response } => {
                        let result = match response {
                            TurnBlockerResponse::HumanApproval { .. } => {
                                HookApprovalResolution::Approved
                            }
                            TurnBlockerResponse::HumanDecline { reason, .. } => {
                                HookApprovalResolution::Denied {
                                    reason: state.human_decline_reason(reason)?,
                                }
                            }
                            _ => {
                                return Err(error(
                                    "human hook received a nonhuman blocker response",
                                ))
                            }
                        };
                        state.acknowledge_human_response(&owner.blocker_id)?;
                        state.human_waits.remove(&owner.blocker_id);
                        state.publish_human_wait_changed(activation, &owner.blocker_id)?;
                        owner.finished = true;
                        return Ok(result);
                    }
                    _ => {
                        return Ok(HookApprovalResolution::Denied {
                            reason: "This exact approval wait is no longer resumable.".into(),
                        })
                    }
                }
            }
            tokio::select! {
                _ = &mut notification => {},
                _ = owner.control.cancelled() => return Ok(HookApprovalResolution::Denied { reason: "Stopped before approval delivery.".into() }),
                _ = &mut deadline => return Ok(HookApprovalResolution::Denied { reason: "Human approval timed out; the tool was not authorized.".into() }),
            }
        }
    }
}
impl Drop for HumanWaitOwner {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Ok(mut state) = self.controller.lock() {
            state.human_waits.remove(&self.blocker_id);
            let result = (|| {
                let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
                if state.current(&self.activation).is_ok()
                    && snapshot.contract().blockers().iter().any(|item| {
                        item.blocker.blocker_id == self.blocker_id
                            && item.state == TurnBlockerState::Pending
                    })
                {
                    state.append(
                        &format!("abandon-{}", self.blocker_id.as_str()),
                        TurnContractEvent::AbandonBlocker {
                            blocker_id: self.blocker_id.clone(),
                            activation: self.activation.clone(),
                            evidence: self.parameters.clone(),
                        },
                    )?;
                }
                state.reconcile_control_commands()?;
                state.publish_human_wait_changed(&self.activation, &self.blocker_id)
            })();
            let _ = state.fail_closed(result);
            state.changed.notify_waiters();
        }
    }
}
impl DispatchState {
    /// Approval is for exact executable bytes, never just a similarly named tool.
    /// Denied/abandoned/old-generation waits cannot be used by a bypassing caller.
    pub(super) fn validate_human_tool_approval(
        &self,
        activation: &ActivationRef,
        request: &ToolInvocationRequest,
    ) -> Result<()> {
        let snapshot = self.current(activation)?;
        for item in snapshot
            .contract()
            .blockers()
            .iter()
            .filter(|item| item.blocker.activation == *activation)
        {
            if super::grant_review::is_grant_proposal(&self.content, item)? {
                continue;
            }
            let (body, _) = wait_parameters(&self.content, &item.blocker.parameters)?;
            if !body.call.same_call(request) {
                continue;
            }
            if body.call != NativeWaitCall::from(request)
                || !matches!(
                    item.state,
                    TurnBlockerState::Resolved {
                        response: TurnBlockerResponse::HumanApproval { .. }
                    }
                )
            {
                return Err(error("the exact native invocation is denied, changed after approval, or no longer resumable"));
            }
        }
        Ok(())
    }
}

impl DispatchState {
    pub(super) fn human_response_capabilities(
        &self,
        activation: &ActivationRef,
        issued_at_ms: u64,
    ) -> Result<Vec<crate::session_control_plane::HumanResponseCapability>> {
        use crate::session_control_plane::{ControlPlaneCapability, HumanResponseCapability};
        use axocoatl_session::control_command::{
            BlockerResponse, CommandReceiptView, CommandSourceRecord, ControlCommandRequest,
            ControlCommandState, ControlParameters, CONTROL_COMMAND_SCHEMA_VERSION,
        };
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        snapshot
            .contract()
            .blockers()
            .iter()
            .filter(|item| item.blocker.activation == *activation)
            .filter(|item| {
                !super::grant_review::is_grant_proposal(&self.content, item).unwrap_or(false)
            })
            .map(|item| {
                let (body, _) = wait_parameters(&self.content, &item.blocker.parameters)?;
                let assess = |approve| {
                    let result = (|| {
                        self.execution_admission()?;
                        let evidence = item.blocker.parameters.clone();
                        self.preview_human_control(&CommandReceiptView {
                            request: ControlCommandRequest {
                                schema_version: CONTROL_COMMAND_SCHEMA_VERSION,
                                command_id: CommandId::new(format!(
                                    "capability-resume-{:x}",
                                    Sha256::digest(
                                        serde_json::to_vec(&(&item.blocker.blocker_id, approve))
                                            .map_err(error)?
                                    )
                                ))
                                .map_err(error)?,
                                session_id: activation.session_id.clone(),
                                turn_id: activation.turn_id.clone(),
                                execution_epoch_id: activation.execution_epoch_id.clone(),
                                expected_turn_revision: snapshot.contract().revision(),
                                expected_graph_revision: snapshot
                                    .contract()
                                    .graph()
                                    .ok_or_else(|| error("human wait has no graph"))?
                                    .revision,
                                issued_at_ms,
                                parameters: ControlParameters::ResumeBlocked {
                                    activation: activation.clone(),
                                    blocker_id: EvidenceRef::new(item.blocker.blocker_id.as_str())
                                        .map_err(error)?,
                                    response: if approve {
                                        BlockerResponse::Approval {
                                            approval: evidence.clone(),
                                        }
                                    } else {
                                        BlockerResponse::Decline {
                                            reason: evidence.clone(),
                                        }
                                    },
                                },
                            },
                            source: CommandSourceRecord::Human {
                                session_id: activation.session_id.clone(),
                                turn_id: activation.turn_id.clone(),
                                request_evidence: evidence,
                            },
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
                        Err(reason) => ControlPlaneCapability {
                            requires_revalidation: false,
                            enabled: false,
                            reason: reason.to_string(),
                        },
                    }
                };
                Ok(HumanResponseCapability {
                    blocker_id: item.blocker.blocker_id.clone(),
                    request: item.blocker.parameters.clone(),
                    state: serde_json::to_value(&item.state).map_err(error)?,
                    display: serde_json::to_value(&body.display).map_err(error)?,
                    approve: assess(true),
                    decline: assess(false),
                })
            })
            .collect()
    }
}

impl DispatchState {
    pub(super) fn publish_human_wait_changed(
        &self,
        activation: &ActivationRef,
        blocker_id: &BlockerId,
    ) -> Result<()> {
        let Some(bus) = &self.stream_bus else {
            return Ok(());
        };
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let (_, last) = self
            .canonical
            .turn_sequences(&self.turn_id)
            .map_err(error)?
            .ok_or_else(|| error("human wait invalidation has no durable canonical record"))?;
        let (_, record) = self
            .canonical
            .records_in(last, last)
            .map_err(error)?
            .pop()
            .ok_or_else(|| error("human wait invalidation has no durable canonical record"))?;
        let _ = bus.send(crate::stream::StreamFrame::ActivationControlChanged {
            activation: activation.clone(),
            blocker_id: blocker_id.clone(),
            canonical_command_id: record.command_id.clone(),
            turn_revision: snapshot.contract().revision(),
        });
        Ok(())
    }
}
