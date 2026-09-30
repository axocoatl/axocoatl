//! Native identities captured around the existing isolated Ways owner.
//! Admission is not completion and cannot stand in for observed lane evidence.
use super::*;
use axocoatl_session::execution_content::{ActivationEvidenceContent, ExecutionModelRef};
use axocoatl_session::turn_contract::*;
use serde::{Deserialize, Serialize};
#[path = "bootstrap_native_ways_execution.rs"]
mod execution;
#[cfg(test)]
pub(super) use execution::NativeWayExecution;
pub(super) use execution::{NativeWaysRuntime, PreparedWay};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeWaysAdmission {
    pub schema_version: u32,
    pub session_id: String,
    pub set_id: String,
    pub source_turn_id: LogicalTurnId,
    pub request: EvidenceRef,
    pub candidates: Vec<NativeWaysCandidateAdmission>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeWaysCandidateAdmission {
    pub index: usize,
    pub run_id: String,
    pub activation: ActivationRef,
    pub definition: DefinitionSnapshotRef,
    pub model: ExecutionModelRef,
    pub grant: GrantSnapshotRef,
}
fn admission_error(error: impl std::fmt::Display) -> DaemonError {
    DaemonError::AttemptConflict(error.to_string())
}
const ADMISSION_FILE: &str = "native-admission.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeWaysSelection {
    schema_version: u32,
    set_id: String,
    candidate: NativeWaysCandidateAdmission,
    output: EvidenceRef,
    checkpoint: axocoatl_session::turn_contract::CheckpointRef,
    request: EvidenceRef,
    task: String,
    assistant: String,
    patch_sha256: String,
    preimage_tree: String,
    postimage_tree: String,
    touched_paths: Vec<String>,
}

/// Validate the exact accepted candidate independently of its peers' outcomes.
/// Keep's existing checked-apply transaction remains the selection authority.
pub(super) fn retained_native_candidate(
    snapshot: &axocoatl_session::execution_store::DurableTurnSnapshot,
    content: &axocoatl_session::execution_content::ExecutionContentStore,
    memory: &axocoatl_memory::activation_state::ActivationStateStore,
    candidate: &NativeWaysCandidateAdmission,
    assistant: &str,
) -> Result<(EvidenceRef, CheckpointRef), DaemonError> {
    let activation = snapshot
        .contract()
        .current_accepted_activations()
        .into_iter()
        .find(|item| item.activation == candidate.activation)
        .ok_or_else(|| admission_error("Keep candidate has no current accepted native result"))?;
    let reservation = content
        .activation_output_reservation(snapshot, &candidate.activation)
        .map_err(admission_error)?
        .ok_or_else(|| admission_error("Keep candidate output reservation is missing"))?;
    let output = content
        .activation_output_settlement(&reservation)
        .map_err(admission_error)?
        .ok_or_else(|| admission_error("Keep candidate output is missing"))?;
    let checkpoint = activation
        .checkpoint
        .as_ref()
        .ok_or_else(|| admission_error("Keep candidate accepted checkpoint is missing"))?;
    memory.checkpoint(checkpoint).map_err(admission_error)?;
    if activation.output.as_ref() != Some(output.reference())
        || output.complete_output().is_none()
        || output.content().output.text != assistant
    {
        return Err(admission_error(
            "Keep answer differs from the actual accepted native candidate",
        ));
    }
    Ok((output.reference().clone(), checkpoint.clone()))
}

pub(super) struct NativeWaysPreparation {
    pub admission: NativeWaysAdmission,
    pub spec: crate::session_dispatch::SuccessorTurn,
    pub inputs: Vec<axocoatl_session::execution_content::TurnAdmissionNodeInput>,
    pub grants: Vec<axocoatl_session::control_authority::AuthorityGrant>,
}

impl AxocoatlDaemon {
    /// A kept answer is explicit retained context. It does not rewrite the
    /// checkpoint of the ordinary Agent or copy a candidate's private state.
    pub(super) fn native_selected_ways_guidance(
        canonical: &axocoatl_session::execution_store::SessionExecutionStore,
        content: &mut axocoatl_session::execution_content::ExecutionContentStore,
        memory: &axocoatl_memory::activation_state::ActivationStateStore,
        conversation: &NodeConversationId,
    ) -> Result<Vec<EvidenceRef>, DaemonError> {
        let after = memory
            .committed_activation(conversation)
            .map_err(admission_error)?
            .map(|activation| activation.turn_id);
        Self::native_selected_ways_guidance_after(canonical, content, memory, after.as_ref())
    }
    fn native_selected_ways_guidance_after(
        canonical: &axocoatl_session::execution_store::SessionExecutionStore,
        content: &mut axocoatl_session::execution_content::ExecutionContentStore,
        memory: &axocoatl_memory::activation_state::ActivationStateStore,
        after: Option<&LogicalTurnId>,
    ) -> Result<Vec<EvidenceRef>, DaemonError> {
        let records = canonical.records().map_err(admission_error)?;
        let after = after
            .map(|turn| {
                records
                    .iter()
                    .position(|record| &record.turn_id == turn)
                    .ok_or_else(|| {
                        admission_error("Committed conversation source is absent from History")
                    })
            })
            .transpose()?;
        let superseded = memory.superseded_turn_ids().map_err(admission_error)?;
        let mut result = Vec::new();
        for selected in content
            .ways_selections(canonical)
            .map_err(admission_error)?
        {
            if superseded
                .iter()
                .any(|turn| turn == selected.turn_id.as_str())
            {
                continue;
            }
            let position = records
                .iter()
                .position(|record| record.turn_id == selected.turn_id)
                .ok_or_else(|| admission_error("Kept decision source is absent from History"))?;
            if after.is_some_and(|after| position <= after) {
                continue;
            }
            let ActivationEvidenceContent::Guidance { text } = content
                .resolve_activation_evidence(&selected.transcript_receipt_ref)
                .map_err(admission_error)?
            else {
                return Err(admission_error("Kept result receipt has the wrong type"));
            };
            let selection: NativeWaysSelection =
                serde_json::from_str(text).map_err(admission_error)?;
            if selection.schema_version != 1
                || selection.candidate.activation.session_id != selected.session_id
                || selection.candidate.activation.turn_id != selected.turn_id
            {
                return Err(admission_error(
                    "Kept result receipt belongs to another execution",
                ));
            }
            let retained=content.retain_activation_evidence(ActivationEvidenceContent::Guidance{text:serde_json::json!({"kind":"kept_ways_result","turn_id":selected.turn_id,"receipt_ref":selected.transcript_receipt_ref,"task":selection.task,"selected_answer":selection.assistant,"patch_sha256":selection.patch_sha256,"authority":"Previously selected result for context; no new execution authority."}).to_string()}).map_err(admission_error)?;
            result.push(retained.reference().clone());
        }
        Ok(result)
    }
    /// Isolated work starts from the committed conversation, never a legacy
    /// projection of native turns or another candidate's mutable checkpoint.
    pub(super) fn native_ways_conversation(
        &self,
        session_id: &str,
    ) -> Result<Vec<axocoatl_core::ChatMessage>, DaemonError> {
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(session_id)?;
        self.session_dispatch_lifecycles.with_session_team_stores(&token,|canonical,content,memory|{
            let team=axocoatl_session::session_team::SessionTeamStore::open_owned(canonical.component_namespace(axocoatl_session::execution_namespace::ExecutionComponent::SessionTeam).map_err(admission_error)?,canonical,content,None).map_err(admission_error)?;
            if let Some(current)=team.current().map_err(admission_error)? {
                if current.graph.slots.len()!=1{return Err(admission_error("Explore several ways requires a single autonomous Agent in the Session team"));}
                let slot=&current.graph.slots[0];
                let ActivationEvidenceContent::Definition{configuration,..}=content.resolve_activation_evidence(&slot.definition.snapshot).map_err(admission_error)?else{return Err(admission_error("Session Agent definition is missing"));};
                let config:axocoatl_core::AgentConfig=serde_json::from_str(configuration).map_err(admission_error)?;
                if config.role!=axocoatl_core::AgentRole::Autonomous{return Err(admission_error("Explore several ways requires an autonomous Session Agent"));}
                let mut conversation=axocoatl_memory::SessionMemory::new();
                if let Some(checkpoint)=memory.committed_checkpoint(&slot.conversation_id).map_err(admission_error)? {conversation.restore(checkpoint.session_messages);}
                let conversation_id=slot.conversation_id.clone();
                drop(team);
                let mut messages=conversation.as_chat_messages();
                for reference in Self::native_selected_ways_guidance(canonical,content,memory,&conversation_id)? {
                    let ActivationEvidenceContent::Guidance{text}=content.resolve_activation_evidence(&reference).map_err(admission_error)?else{unreachable!("validated retained guidance")};
                    messages.push(axocoatl_core::ChatMessage::user(text));
                }
                return Ok(messages);
            }
            drop(team);
            // Before the first team Apply, a migrated single-Agent Session may
            // have an immutable v1 frontier. These are retained historical
            // messages; no legacy writer or actor checkpoint is consulted.
            let mut messages=Vec::new();
            let superseded=memory.superseded_turn_ids().map_err(admission_error)?;
            if let Some(seal)=canonical.legacy_seal().map_err(admission_error)? {
                for turn in &content.read_legacy_history(&seal).map_err(admission_error)?.turns {
                    if turn.superseded||superseded.contains(&turn.id){continue;}
                    messages.push(axocoatl_core::ChatMessage::user(&turn.user_input));
                    if turn.agent_outputs.is_empty(){
                        if let Some(output)=turn.final_output.as_ref().or_else(||(!turn.partial_output.is_empty()).then_some(&turn.partial_output)){messages.push(axocoatl_core::ChatMessage::assistant(output));}
                    }else{for output in turn.agent_outputs.iter().filter(|output|!output.superseded){messages.push(axocoatl_core::ChatMessage::assistant(&output.output));}}
                }
            }
            for reference in Self::native_selected_ways_guidance_after(canonical,content,memory,None)? {
                let ActivationEvidenceContent::Guidance{text}=content.resolve_activation_evidence(&reference).map_err(admission_error)?else{unreachable!("validated retained guidance")};
                messages.push(axocoatl_core::ChatMessage::user(text));
            }
            Ok(messages)
        })
    }
    /// Called only after the existing exact container cleanup has succeeded.
    /// Retire all actual execution tickets before releasing the canonical files;
    /// reopening those files retains interruption and unknown external effects.
    pub(super) async fn retire_native_ways_after_cleanup(
        &self,
        session: &Session,
        set: &crate::git::AttemptSet,
    ) -> Result<(), DaemonError> {
        if !self.uses_native_session_history() {
            return Ok(());
        }
        let cleanup = self
            .session_dispatch_lifecycles
            .prepare_session_cleanup(&session.id, SESSION_DISPATCH_CLEANUP_TIMEOUT)
            .await?;
        self.session_dispatch_lifecycles
            .complete_session_cleanup(&cleanup)?;
        drop(cleanup);
        self.restore_native_lifecycle_history(
            session,
            session.status == axocoatl_session::SessionStatus::Closed,
        )?;
        let decision = self.retained_ways_decision(&session.id, &set.id)?;
        self.session_dispatch_lifecycles.close_cleaned_native_ways(
            &session.id,
            &set.id,
            decision.as_ref(),
        )?;
        Ok(())
    }
    pub(super) async fn quiesce_native_histories_for_ways(
        &self,
        session_id: &str,
    ) -> Result<(), DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| admission_error("Session is missing"))?;
        let peers = self
            .list_sessions()
            .await
            .into_iter()
            .filter(|peer| peer.workspace_id == session.workspace_id)
            .collect::<Vec<_>>();
        self.require_no_unresolved_attempt(session_id).await?;
        for peer in &peers {
            self.session_dispatch_lifecycles
                .require_native_environment_change_ready(&peer.id)?;
        }
        for peer in peers {
            let mut cleanup = self
                .session_dispatch_lifecycles
                .prepare_session_cleanup(&peer.id, SESSION_DISPATCH_CLEANUP_TIMEOUT)
                .await?;
            let _operation = match cleanup.take_operation() {
                Some(operation) => operation,
                None => self
                    .attempt_operation(&peer.id)
                    .await
                    .lock_owned()
                    .await
                    .into(),
            };
            self.stop_session_actors_checked(&peer.id).await?;
            self.stop_session_sandbox_checked(&peer.id).await?;
            self.session_dispatch_lifecycles
                .complete_session_cleanup(&cleanup)?;
            drop(cleanup);
            let current = self
                .get_session(&peer.id)
                .await
                .ok_or_else(|| admission_error("Session disappeared during Ways quiescence"))?;
            self.restore_native_lifecycle_history(
                &current,
                current.status == axocoatl_session::SessionStatus::Closed,
            )?;
        }
        Ok(())
    }
    /// The authenticated Ways form supplies every numerical limit. Effective
    /// model resolution and provider metadata are captured before clone setup.
    pub(super) async fn prepare_native_ways_admission(
        &self,
        session: &Session,
        set: &crate::git::AttemptSet,
        lanes: &[crate::git::LaneConfig],
        history: &[axocoatl_core::ChatMessage],
    ) -> Result<NativeWaysPreparation, DaemonError> {
        use axocoatl_session::control_authority::AuthorityGrant;
        use axocoatl_session::execution_content::{
            ExecutionRequestContent, TurnAdmissionContent, TurnAdmissionNodeInput,
        };
        if !self.uses_native_session_history()
            || !matches!(session.mode, SessionMode::SingleAgent { .. })
            || set.session_id != session.id
            || lanes.len() != set.lanes.len()
        {
            return Err(admission_error(
                "Native Ways require the exact single-Agent Session and candidate roster",
            ));
        }
        self.with_ways_archive(&session.id, |_| Ok(()))?;
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(&session.id)?;
        let key = crate::attempts::set_key(&set.id);
        let turn_id = LogicalTurnId::new(format!("ways-{key}")).map_err(admission_error)?;
        let epoch_id =
            ExecutionEpochId::new(format!("ways-epoch-{key}")).map_err(admission_error)?;
        let command_id = CommandId::new(format!("ways-begin-{key}")).map_err(admission_error)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(admission_error)?
            .as_millis() as u64;
        let mut captured = Vec::with_capacity(lanes.len());
        for (index, (lane, variant)) in lanes.iter().zip(&set.lanes).enumerate() {
            let approval = lane.approval.as_ref().ok_or_else(|| {
                admission_error("Enter explicit limits, output maximum and expiry for every Way")
            })?;
            if approval.limits.activations == 0
                || approval.limits.invocations == 0
                || approval.limits.tokens == 0
                || approval.max_output_tokens == 0
                || approval.expires_at_ms <= now
            {
                return Err(admission_error(
                    "Every Way needs positive execution limits and an unexpired approval",
                ));
            }
            let agent = variant
                .agent
                .as_deref()
                .ok_or_else(|| admission_error("Ways roster is missing its selected Agent"))?;
            let mut config = self
                .config
                .agents
                .iter()
                .find(|configured| configured.id == agent)
                .ok_or_else(|| admission_error("Selected Ways Agent is unavailable"))?
                .to_core();
            if config.role != axocoatl_core::AgentRole::Autonomous {
                return Err(admission_error("Ways require autonomous Agents"));
            }
            config.model = variant
                .model
                .clone()
                .ok_or_else(|| admission_error("Ways roster is missing its selected model"))?;
            config.sampling.max_tokens = Some(approval.max_output_tokens);
            config.id = AgentId::new(format!("ways-conversation-{key}-{index}"));
            let definition = self
                .prepare_native_session_team_definition(
                    &token,
                    config,
                    AgentDefinitionId::new(format!("ways-definition-{key}-{index}"))
                        .map_err(admission_error)?,
                    1,
                    approval.limits.clone(),
                )
                .await?;
            if variant.provider.as_deref() != Some(definition.profile.provider.as_str())
                || variant.model.as_deref() != Some(definition.profile.model.as_str())
            {
                return Err(admission_error(
                    "Effective Ways provider/model differs from the reviewed selection",
                ));
            }
            captured.push(definition);
        }
        self.session_dispatch_lifecycles.with_session_team_stores(&token,|canonical,content,_|{
            if canonical.unfinished_turn().map_err(admission_error)?.is_some() {return Err(admission_error("Finish or stop the current Session turn before exploring Ways"));}
            let request=ExecutionRequestContent{turn_id:turn_id.clone(),recorded_at_unix_ms:now,display_input:set.task.clone(),effective_input:set.instruction.clone(),context:vec![],target_definition:None,model:None};
            let request_ref=content.retain_request(request.clone()).map_err(admission_error)?.reference().clone();
            let history=content.retain_activation_evidence(ActivationEvidenceContent::Guidance{text:serde_json::json!({"kind":"retained_session_context_for_isolated_way","messages":history}).to_string()}).map_err(admission_error)?.reference().clone();
            let mut nodes=vec![];let mut candidates=vec![];let mut inputs=vec![];let mut grants=vec![];
            for (index,(definition,lane)) in captured.iter().zip(lanes).enumerate() {
                let approval=lane.approval.as_ref().expect("validated approval");
                let node_id=TurnNodeId::new(format!("ways-node-{key}-{index}")).map_err(admission_error)?;
                let conversation_id=NodeConversationId::new(format!("ways-conversation-{key}-{index}")).map_err(admission_error)?;
                let issuer=content.retain_activation_evidence(ActivationEvidenceContent::Guidance{text:serde_json::json!({"kind":"authenticated_ways_candidate_approval","session_id":session.id,"set_id":set.id,"index":index,"definition":definition.definition,"approval":approval}).to_string()}).map_err(admission_error)?.reference().clone();
                let grant=AuthorityGrant{id:format!("ways-grant-{key}-{index}"),revision:1,issuer_evidence:issuer,holder:node_id.clone(),descendants:vec![],allow_stop_descendants:false,delegation:None,profiles:vec![definition.profile.clone()],conditions:vec![],limits:approval.limits.clone(),expires_at_ms:approval.expires_at_ms};
                grant.validate().map_err(admission_error)?;
                let evidence=content.retain_activation_evidence(ActivationEvidenceContent::Grant{policy:grant.clone()}).map_err(admission_error)?.reference().clone();
                let grant_ref=GrantSnapshotRef{grant_id:GrantId::new(&grant.id).map_err(admission_error)?,revision:1,evidence};
                let budget=content.retain_activation_evidence(ActivationEvidenceContent::Budget{limits:approval.limits.clone()}).map_err(admission_error)?.reference().clone();
                let activation=ActivationRef{session_id:canonical.owner().session_id.clone(),turn_id:turn_id.clone(),execution_epoch_id:epoch_id.clone(),node_id:node_id.clone(),activation_id:ActivationId::new(format!("ways-activation-{key}-{index}")).map_err(admission_error)?,generation:1};
                candidates.push(NativeWaysCandidateAdmission{index,run_id:crate::attempts::run_id(&session.id,index),activation,definition:definition.definition.clone(),model:ExecutionModelRef{provider_id:definition.profile.provider.clone(),model_id:definition.profile.model.clone(),configuration_ref:definition.definition.snapshot.clone()},grant:grant_ref.clone()});
                nodes.push(GraphNode{node_id:node_id.clone(),slot_id:SessionTeamSlotId::new(format!("ways-slot-{key}-{index}")).map_err(admission_error)?,definition:definition.definition.clone(),conversation_id,starting_savepoint:ConversationSavepoint::Empty,required:true});
                inputs.push(TurnAdmissionNodeInput{node_id,guidance:vec![request_ref.clone(),history.clone()],attachments:vec![],budget,grant:grant_ref});grants.push(grant);
            }
            let graph=TurnGraphSnapshot{snapshot_id:GraphSnapshotId::new(format!("ways-graph-{key}")).map_err(admission_error)?,revision:1,nodes,dependencies:vec![],conditions:vec![]};
            let admission=NativeWaysAdmission{schema_version:1,session_id:session.id.clone(),set_id:set.id.clone(),source_turn_id:turn_id.clone(),request:request_ref.clone(),candidates};
            content.retain_turn_admission(canonical,TurnAdmissionContent{schema_version:1,command_id:command_id.clone(),turn_id:turn_id.clone(),epoch_id:epoch_id.clone(),source:serde_json::to_string(&admission).map_err(admission_error)?,graph:graph.clone(),request:request_ref,nodes:inputs.clone()}).map_err(admission_error)?;
            Ok(NativeWaysPreparation{admission,spec:crate::session_dispatch::SuccessorTurn{command_id,turn_id,epoch_id,graph,request},inputs,grants})
        })
    }
    /// Called by the existing Keep transaction only after its checked apply
    /// journal has reconciled. The receipt selects an actual accepted result;
    /// it cannot turn caller text into a newly executed Agent response.
    pub(super) fn native_kept_session_turn(
        &self,
        session: &Session,
        set: &crate::git::AttemptSet,
        index: usize,
        assistant: &str,
        touched_paths: &[String],
    ) -> Result<axocoatl_session::ways_decision::WaysSelectedSessionTurn, DaemonError> {
        if !matches!(
            set.state,
            crate::git::AttemptSetState::Applied | crate::git::AttemptSetState::TranscriptRecorded
        ) || set.kept_index != Some(index)
        {
            return Err(admission_error(
                "Native transcript selection requires the exact applied Keep transaction",
            ));
        }
        let root = Self::open_attempt_root_host(&session.working_dir, &session.id, &set.id)?;
        let admission = self.native_ways_admission(&session.id, set, &root)?;
        let candidate = admission
            .candidates
            .iter()
            .find(|candidate| candidate.index == index)
            .ok_or_else(|| admission_error("Selected native candidate is missing"))?;
        let apply = Self::read_host_json_file::<StoredKeepApply>(
            &root,
            std::path::Path::new("keep-apply.json"),
        )?
        .ok_or_else(|| admission_error("Native Keep is missing its actual apply journal"))?;
        if apply.index != index {
            return Err(admission_error(
                "Native Keep journal selects another candidate",
            ));
        }
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(&session.id)?;
        self.session_dispatch_lifecycles.with_session_team_stores(
            &token,
            |canonical, content, memory| {
                let snapshot = canonical
                    .snapshot(&admission.source_turn_id)
                    .map_err(admission_error)?;
                let (output, checkpoint) =
                    retained_native_candidate(&snapshot, content, memory, candidate, assistant)?;
                let selection = NativeWaysSelection {
                    schema_version: 1,
                    set_id: set.id.clone(),
                    candidate: candidate.clone(),
                    output,
                    checkpoint,
                    request: admission.request.clone(),
                    task: set.task.clone(),
                    assistant: assistant.into(),
                    patch_sha256: apply.patch_sha256,
                    preimage_tree: apply.preimage_tree,
                    postimage_tree: apply.postimage_tree,
                    touched_paths: touched_paths.to_vec(),
                };
                let receipt = content
                    .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                        text: serde_json::to_string(&selection).map_err(admission_error)?,
                    })
                    .map_err(admission_error)?;
                let link = axocoatl_session::ways_decision::WaysSelectedSessionTurn {
                    session_id: canonical.owner().session_id.clone(),
                    turn_id: admission.source_turn_id,
                    transcript_receipt_ref: receipt.reference().clone(),
                };
                content
                    .retain_ways_selection(&snapshot, link.clone(), candidate.activation.clone())
                    .map_err(admission_error)?;
                Ok(link)
            },
        )
    }
    pub(super) fn native_ways_admission(
        &self,
        session_id: &str,
        set: &crate::git::AttemptSet,
        root: &SecureDir,
    ) -> Result<NativeWaysAdmission, DaemonError> {
        let admission = Self::read_host_json_file::<NativeWaysAdmission>(
            root,
            std::path::Path::new(ADMISSION_FILE),
        )?
        .ok_or_else(|| admission_error("This attempt set has no native admission evidence"))?;
        self.validate_native_ways_admission(session_id, set, &admission)?;
        Ok(admission)
    }

    pub(super) fn persist_native_ways_admission(
        &self,
        set: &crate::git::AttemptSet,
        root: &SecureDir,
        admission: &NativeWaysAdmission,
    ) -> Result<(), DaemonError> {
        self.validate_native_ways_admission(&set.session_id, set, admission)?;
        if let Some(existing) = Self::read_host_json_file::<NativeWaysAdmission>(
            root,
            std::path::Path::new(ADMISSION_FILE),
        )? {
            return if &existing == admission {
                Ok(())
            } else {
                Err(admission_error(
                    "Ways admission identity cannot change after capture",
                ))
            };
        }
        Self::write_host_json_file(root, std::path::Path::new(ADMISSION_FILE), admission)
    }

    fn validate_native_ways_admission(
        &self,
        session_id: &str,
        set: &crate::git::AttemptSet,
        admission: &NativeWaysAdmission,
    ) -> Result<(), DaemonError> {
        if admission.schema_version != 1
            || admission.session_id != session_id
            || set.session_id != session_id
            || admission.set_id != set.id
            || admission.candidates.len() != set.lanes.len()
            || admission.candidates.is_empty()
            || admission.candidates.len() > 100
        {
            return Err(admission_error(
                "Ways admission differs from its exact Session and attempt set",
            ));
        }
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(session_id)?;
        self.session_dispatch_lifecycles.with_session_team_stores(
            &token,
            |canonical, content, _| {
                let snapshot = canonical
                    .snapshot(&admission.source_turn_id)
                    .map_err(admission_error)?;
                if snapshot.request_ref() != Some(&admission.request) {
                    return Err(admission_error(
                        "Ways source turn does not retain its exact request",
                    ));
                }
                let graph = snapshot
                    .contract()
                    .graph()
                    .ok_or_else(|| admission_error("Ways source turn has no graph"))?;
                let mut seen = std::collections::HashSet::new();
                for candidate in &admission.candidates {
                    let lane = set
                        .lanes
                        .iter()
                        .find(|lane| lane.index == candidate.index)
                        .ok_or_else(|| {
                            admission_error("Ways candidate is outside the captured roster")
                        })?;
                    if !seen.insert(candidate.index)
                        || candidate.activation.session_id.as_str() != session_id
                        || candidate.activation.turn_id != admission.source_turn_id
                        || candidate.run_id != crate::attempts::run_id(session_id, candidate.index)
                        || candidate.activation.generation != 1
                    {
                        return Err(admission_error(
                            "Ways candidate has inconsistent native execution identity",
                        ));
                    }
                    let node = graph
                        .nodes
                        .iter()
                        .find(|node| node.node_id == candidate.activation.node_id)
                        .ok_or_else(|| admission_error("Ways candidate has no canonical node"))?;
                    if node.definition != candidate.definition {
                        return Err(admission_error(
                            "Ways candidate definition differs from its canonical node",
                        ));
                    }
                    let ActivationEvidenceContent::Definition {
                        profile,
                        configuration,
                        ..
                    } = content
                        .resolve_activation_evidence(&candidate.definition.snapshot)
                        .map_err(admission_error)?
                    else {
                        return Err(admission_error("Ways candidate definition is unavailable"));
                    };
                    let config: axocoatl_core::AgentConfig =
                        serde_json::from_str(configuration).map_err(admission_error)?;
                    let ActivationEvidenceContent::Grant { policy } = content
                        .resolve_activation_evidence(&candidate.grant.evidence)
                        .map_err(admission_error)?
                    else {
                        return Err(admission_error("Ways candidate grant is unavailable"));
                    };
                    if candidate.model.configuration_ref != candidate.definition.snapshot
                        || candidate.model.provider_id != profile.provider
                        || candidate.model.model_id != profile.model
                        || lane.provider.as_deref() != Some(profile.provider.as_str())
                        || lane.model.as_deref() != Some(profile.model.as_str())
                        || config.role != axocoatl_core::AgentRole::Autonomous
                        || config.provider != profile.provider
                        || config.model != profile.model
                        || config.tools != profile.tools
                        || config.writes != profile.write_scope
                        || policy.holder != node.node_id
                        || policy.id != candidate.grant.grant_id.as_str()
                        || policy.revision != candidate.grant.revision
                        || !policy.profiles.contains(profile)
                    {
                        return Err(admission_error(
                            "Ways candidate model, definition or grant changed after approval",
                        ));
                    }
                }
                Ok(())
            },
        )
    }
}
