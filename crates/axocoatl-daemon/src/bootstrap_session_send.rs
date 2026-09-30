//! The normal Session Send entrypoint for the owned execution controller.
//! Reconnect compares the original request before consulting mutable context or
//! team configuration. Stream frames are notifications; History owns the result.
use super::native_turn::{NativeFirstTurnRequest, NativeFirstTurnStart, NativeNodeEvidence};
use super::*;
use axocoatl_core::{AgentOutput, MeasuredTokenUsage, TokenUsageStats};
use axocoatl_session::execution_content::{
    ActivationEvidenceContent, ContentResolution, ExecutionRequestContent, ExecutionTurnView,
};
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::session_history::SessionHistoryEntry;
use axocoatl_session::session_team::SessionTeamStore;
use axocoatl_session::turn_contract::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeSessionSend {
    pub session_id: String,
    pub turn_id: String,
    pub idempotency_key: Option<String>,
    pub display_input: Option<String>,
    pub input: String,
    pub reference_ids: Vec<String>,
    pub context_references: Vec<SessionTurnContextReference>,
    pub model_override: Option<String>,
    pub target_agent: Option<String>,
}
fn failed(error: impl std::fmt::Display) -> DaemonError {
    DaemonError::SessionConflict(error.to_string())
}

impl AxocoatlDaemon {
    pub(super) async fn execute_native_session_send(
        &self,
        source: NativeSessionSend,
    ) -> Result<AgentRunOutcome, DaemonError> {
        let prepared = self.prepare_native_send_request(&source).await;
        let request = match prepared {
            Ok(request) => request,
            Err(error) => {
                self.reject_native_send(&source, &error);
                return Err(error);
            }
        };
        let start = match self.prepare_native_turn(request).await {
            Ok(start) => start,
            Err(error) => {
                self.reject_native_send(&source, &error);
                return Err(error);
            }
        };
        match start {
            NativeFirstTurnStart::Reattached(view) => {
                if view.session_id != source.session_id || view.turn_id != source.turn_id {
                    return Err(failed(
                        "Reattached history belongs to a different Session request",
                    ));
                }
                // Reading the original result never takes another execution
                // lease, changes the current team, or consumes Once context.
                let history = self
                    .versioned_session_history_snapshot(&source.session_id)
                    .await?;
                let Some(SessionHistoryEntry::ExecutionV2(turn)) = history.get(&source.turn_id)
                else {
                    return Err(failed(
                        "The accepted turn is missing from its Session history",
                    ));
                };
                if turn.state == LogicalTurnState::Running {
                    return Err(DaemonError::SessionTurnReattached {
                        session: source.session_id,
                        turn: source.turn_id,
                    });
                }
                let usage = history_usage(turn);
                publish_native_disposition(&self.stream_bus, turn, &usage);
                native_outcome(turn, usage)
            }
            NativeFirstTurnStart::Prepared(prepared) => {
                let controller = prepared.controller();
                // Native Begin is already durable. Pin/consume the same exact
                // selected references before any actual Agent execution.
                let accepted = controller.history_snapshot().map_err(failed)?;
                let Some(SessionHistoryEntry::ExecutionV2(turn)) = accepted.get(&source.turn_id)
                else {
                    return Err(failed("Native Begin did not retain its request"));
                };
                let ContentResolution::Available {
                    content: request, ..
                } = &turn.request
                else {
                    return Err(failed("The accepted request body is unavailable"));
                };
                let selected: Vec<String> = request
                    .context
                    .iter()
                    .filter(|item| item.kind == "upload")
                    .map(|item| item.reference_id.clone())
                    .collect();
                if let Err(error) = self.session_attachment_store.lock().await.mark_consumed(
                    &source.session_id,
                    &selected,
                    &source.turn_id,
                ) {
                    drop(prepared); // Existing driver interruption retains this admitted turn.
                    let error = failed(format!("Could not retain accepted context: {error}"));
                    if let Ok(history) = controller.history_snapshot() {
                        if let Some(SessionHistoryEntry::ExecutionV2(turn)) =
                            history.get(&source.turn_id)
                        {
                            publish_native_disposition(
                                &self.stream_bus,
                                turn,
                                &history_usage(turn),
                            );
                        }
                    }
                    return Err(error);
                }
                self.remember_session_last_turn_files(&source.session_id, Vec::new());
                let result = prepared.run().await;
                let history = controller.history_snapshot().map_err(failed)?;
                let Some(SessionHistoryEntry::ExecutionV2(turn)) = history.get(&source.turn_id)
                else {
                    return Err(failed(
                        "The executed turn is missing from its canonical history",
                    ));
                };
                let mut usage = MeasuredTokenUsage::known(TokenUsageStats::default());
                for activation in &turn.activations {
                    match controller.activation_provider_usage(&activation.activation.activation) {
                        Ok(measured) => {
                            usage.usage.merge(&measured.tokens.usage);
                            usage.complete &= measured.tokens.complete;
                        }
                        Err(_) => {
                            usage.complete = false;
                        }
                    }
                }
                publish_native_disposition(&self.stream_bus, turn, &usage);
                // Completion can race an explicit Close. Updating recency may
                // not reopen the Session after Close has persisted its state.
                let workspace = {
                    let mut sessions = self.session_store.lock().await;
                    sessions
                        .get(&source.session_id)
                        .filter(|session| session.status != axocoatl_session::SessionStatus::Closed)
                        .and_then(|session| {
                            sessions
                                .touch(&source.session_id)
                                .ok()
                                .map(|_| session.workspace_id)
                        })
                };
                if let Some(workspace) = workspace {
                    let _ = self.workspace_store.lock().await.touch(&workspace);
                }
                if let Err(error) = result {
                    return Err(DaemonError::session_execution_measured(
                        error,
                        usage.usage,
                        usage.complete,
                    ));
                }
                native_outcome(turn, usage)
            }
        }
    }

    fn reject_native_send(&self, source: &NativeSessionSend, error: &DaemonError) {
        let _ = self
            .stream_bus
            .send(crate::stream::StreamFrame::SessionRequestRejected {
                session: source.session_id.clone(),
                turn_id: source.turn_id.clone(),
                error: error.to_string(),
            });
    }

    pub(super) async fn prepare_native_send_request(
        &self,
        source: &NativeSessionSend,
    ) -> Result<NativeFirstTurnRequest, DaemonError> {
        self.require_runtime_admission()?;
        let turn_id = LogicalTurnId::new(&source.turn_id).map_err(failed)?;
        let session_id = SessionId::new(&source.session_id).map_err(failed)?;
        let ingress = serde_json::to_value(source).map_err(failed)?;
        if serde_json::to_vec(&ingress).map_err(failed)?.len() > MAX_CONTRACT_ENVELOPE_BYTES {
            return Err(failed("Session request exceeds the supported input limit"));
        }
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(&source.session_id)?;
        let existing = self.session_dispatch_lifecycles.with_session_team_stores(
            &token,
            |canonical, content, _| {
                content
                    .turn_admission(canonical, &turn_id)
                    .map_err(failed)?
                    .map(|(_, admission)| {
                        serde_json::from_str::<NativeFirstTurnRequest>(&admission.source)
                            .map_err(failed)
                    })
                    .transpose()
            },
        )?;
        if let Some(existing) = existing {
            if existing.ingress.as_ref() != Some(&ingress) {
                return Err(failed(
                    "This turn ID was already used for a different Session request",
                ));
            }
            return Ok(existing);
        }
        self.require_no_unresolved_attempt(&source.session_id)
            .await?;
        let model_selections = self
            .prepare_native_send_model_selection(&token, source, &turn_id)
            .await?;
        let session = self
            .get_session(&source.session_id)
            .await
            .ok_or_else(|| failed("Session not found"))?;
        let (_, attachments, begin) = self
            .prepare_session_turn_context(
                &session,
                &source.turn_id,
                source.display_input.as_deref().unwrap_or(&source.input),
                &source.reference_ids,
                &source.context_references,
                source.model_override.clone(),
                source.target_agent.clone(),
            )
            .await?;
        let knowledge_context = self
            .capture_session_knowledge_context(&source.session_id, &source.input)
            .await?;
        let recorded_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(failed)?
            .as_millis()
            .try_into()
            .map_err(failed)?;
        self.session_dispatch_lifecycles.with_session_team_stores(&token, |canonical, content, memory| {
            canonical.verify_data_root(&self.data_root).map_err(failed)?;
            let team = SessionTeamStore::open_owned(canonical.component_namespace(ExecutionComponent::SessionTeam).map_err(failed)?, canonical, content, None).map_err(failed)?;
            let revision = team.current().map_err(failed)?.cloned()
                .ok_or_else(|| failed("Open Team and budget in this Session and apply the execution limits before sending"))?;
            drop(team);
            let mut grants = Vec::new();
            let mut node_evidence = Vec::new();
            let mut target = None;
            let mut attached = Vec::new();
            for attachment in &attachments {
                let evidence = ActivationEvidenceContent::from_attachment(attachment).map_err(failed)?;
                attached.push(content.retain_activation_evidence(evidence).map_err(failed)?.reference().clone());
            }
            for slot in &revision.graph.slots {
                if let Some(selected) = &source.target_agent {
                    let template = super::session_team::approved_template_for_slot(content, slot)?;
                    if selected != slot.node_id.as_str()
                        && selected != slot.definition.definition_id.as_str()
                        && template.as_ref() != Some(selected)
                    { continue; }
                }
                let grant_ref = slot.grant.as_ref().ok_or_else(|| failed("The Session team has no approved execution grant; open Team and budget"))?;
                let ActivationEvidenceContent::Grant {policy} = content.resolve_activation_evidence(grant_ref).map_err(failed)? else {
                    return Err(failed("The Session execution grant has the wrong evidence type"));
                };
                let ActivationEvidenceContent::Definition {profile, ..} = content.resolve_activation_evidence(&slot.definition.snapshot).map_err(failed)? else {
                    return Err(failed("The selected Agent definition is unavailable"));
                };
                if source.model_override.as_ref().is_some_and(|model| model != &profile.model)
                    && !model_selections.iter().any(|selection|selection.node_id==slot.node_id && selection.approved_definition==slot.definition && slot.grant.as_ref()==Some(&selection.approved_grant)) {
                    return Err(failed("The Team changed during model capture; retry the selected model"));
                }
                if source.target_agent.is_some() {
                    if target.is_some() {return Err(failed("That Agent template occurs more than once; select an exact Agent in the Session graph"));}
                    target = Some(slot.definition.definition_id.clone());
                }
                grants.push(policy.clone());
                let mut guidance=Self::native_selected_ways_guidance(canonical,content,memory,&slot.conversation_id)?;
                if let Some(text) = &knowledge_context {
                    let evidence = content.retain_activation_evidence(ActivationEvidenceContent::Guidance { text:text.clone() }).map_err(failed)?;
                    guidance.push(evidence.reference().clone());
                }
                node_evidence.push(NativeNodeEvidence {node_id:slot.node_id.clone(), guidance, attachments:attached.clone()});
            }
            if node_evidence.is_empty() {return Err(failed("The selected Agent is not part of this Session team"));}
            let request = ExecutionRequestContent {turn_id:turn_id.clone(), recorded_at_unix_ms,
                display_input:source.display_input.clone().unwrap_or_else(||source.input.clone()),
                effective_input:source.input.clone(), context:begin.context.clone(), target_definition:target.clone(), model:None};
            Ok(NativeFirstTurnRequest {schema_version:1, standing_work:None, ingress:Some(ingress), session_id,
                command_id:CommandId::new(format!("send:{}", source.turn_id)).map_err(failed)?,
                epoch_id:ExecutionEpochId::new(format!("epoch:{}", source.turn_id)).map_err(failed)?,
                graph_snapshot_id:GraphSnapshotId::new(format!("graph:{}", source.turn_id)).map_err(failed)?,
                turn_id, expected_team_revision:revision.configuration_revision, target_definition:target, request, grants, model_selections, node_evidence})
        })
    }
}

pub(super) fn publish_native_disposition(
    bus: &crate::stream::StreamBus,
    turn: &ExecutionTurnView,
    usage: &MeasuredTokenUsage,
) {
    use crate::stream::StreamFrame;
    let session = turn.owner.session_id.as_str().to_owned();
    let turn_id = turn.turn_id.as_str().to_owned();
    let input_tokens = usage.usage.input_tokens as u64;
    let output_tokens = usage.usage.output_tokens as u64;
    let reasoning_tokens = usage.usage.reasoning_tokens.unwrap_or(0) as u64;
    let token_usage_known = usage.complete;
    let frame = match turn.state {
        LogicalTurnState::Running => return,
        LogicalTurnState::Completed | LogicalTurnState::Finished => StreamFrame::SessionDone {
            session,
            turn_id: Some(turn_id),
            input_tokens,
            output_tokens,
            reasoning_tokens,
            token_usage_known,
        },
        LogicalTurnState::Cancelled => StreamFrame::SessionCancelled {
            session,
            turn_id,
            input_tokens,
            output_tokens,
            reasoning_tokens,
            token_usage_known,
        },
        LogicalTurnState::NeedsAttention => StreamFrame::SessionNeedsAttention {
            session,
            turn_id,
            input_tokens,
            output_tokens,
            reasoning_tokens,
            token_usage_known,
        },
    };
    let _ = bus.send(frame);
}

/// Historical output usage is a presentation fallback, not new budget authority.
/// A missing terminal body leaves the aggregate incomplete; partial snapshots
/// are not added repeatedly and superseded attempts still count their spend.
pub(super) fn history_usage(turn: &ExecutionTurnView) -> MeasuredTokenUsage {
    use axocoatl_session::execution_content::ExecutionUsage;
    let mut result = MeasuredTokenUsage::known(TokenUsageStats::default());
    for activation in &turn.activations {
        let output = match &activation.output {
            ContentResolution::Available { content, .. } => Some(content),
            _ => activation
                .reserved_outputs
                .iter()
                .rev()
                .find(|item| {
                    matches!(
                        item.content.slot,
                        axocoatl_session::execution_content::ActivationOutputSlot::Settlement
                    )
                })
                .map(|item| &item.content.output),
        };
        match output.map(|output| &output.usage) {
            Some(ExecutionUsage::Measured { usage }) => result.usage.merge(usage),
            Some(ExecutionUsage::Unknown { known_subtotal }) => {
                result.usage.merge(known_subtotal);
                result.complete = false;
            }
            None => result.complete = false,
        }
    }
    result
}
fn native_outcome(
    turn: &ExecutionTurnView,
    usage: MeasuredTokenUsage,
) -> Result<AgentRunOutcome, DaemonError> {
    let accepted = turn
        .activations
        .iter()
        .filter(|item| item.currently_accepted)
        .filter(|item| {
            turn.stop_requested
                .as_ref()
                .and_then(|intent| intent.partial_finish.as_ref())
                .is_none_or(|selection| {
                    selection
                        .selected_activations
                        .contains(&item.activation.activation)
                })
        })
        .filter_map(|item| match &item.output {
            ContentResolution::Available { content, .. } => Some(content.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut output = AgentOutput::text(accepted);
    output.token_usage = usage.usage.clone();
    match turn.state {
        LogicalTurnState::Completed | LogicalTurnState::Finished => Ok(AgentRunOutcome::Completed(output)),
        LogicalTurnState::Cancelled => Ok(AgentRunOutcome::Cancelled {run_id:AgentRunId::new(turn.turn_id.as_str()), partial_output:output}),
        _ => Err(DaemonError::session_execution_measured(failed("This turn needs attention; inspect its blocked or interrupted work before continuing"), usage.usage, usage.complete)),
    }
}
