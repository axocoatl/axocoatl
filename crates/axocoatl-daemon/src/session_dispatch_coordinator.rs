//! Native hierarchical scheduling enters the same canonical driver as every
//! other Agent. Coordinator code receives a wait handle, never another executor.
use super::*;
use axocoatl_actor::{
    AdmittedChildExecution, AgentExecutionFailure, AgentRunId, AgentRunOutcome,
    ChildExecutionRequest, MeasuredAgentRunOutcome, WorkerConfig,
};
use axocoatl_core::{AgentConfig, AgentOutput, AgentRole, MeasuredTokenUsage, TokenUsageStats};
use axocoatl_session::control_authority::{DelegatedGrantReservation, GrantLimits};
use axocoatl_session::control_command::{
    CommandReceiptView, CommandSourceRecord, ControlCommandRequest, ControlCommandState,
    ControlParameters, ControlTransition,
};
use serde::{Deserialize, Serialize};

type CoordinatorWorkerPreparation = (Vec<(WorkerConfig, String)>, Vec<String>, Option<String>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeCoordinatorWorker {
    pub template_id: String,
    pub definition: DefinitionSnapshotRef,
    pub limits: GrantLimits,
    pub adhoc_allowed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeChildProposal {
    kind: String,
    parent: ActivationRef,
    request: ChildExecutionRequest,
    worker: NativeCoordinatorWorker,
    node_id: TurnNodeId,
    conversation_id: NodeConversationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replacement: Option<(TurnNodeId, Vec<TurnNodeId>)>,
}

/// The approved template owns all behavior fields. A canonical child owns its
/// separate conversation identity; deriving that one field adds no capability.
pub(super) fn instance_configuration(
    configuration: &str,
    conversation: &NodeConversationId,
) -> Result<AgentConfig> {
    let mut config: AgentConfig = serde_json::from_str(configuration).map_err(error)?;
    config.id = axocoatl_core::AgentId::new(conversation.as_str());
    Ok(config)
}
impl DispatchState {
    pub(super) fn native_child_origin(
        &self,
        node: &TurnNodeId,
    ) -> Result<Option<CommandReceiptView>> {
        for view in self.control_plane_commands()? {
            if !matches!(view.source, CommandSourceRecord::Agent { .. })
                || !matches!(
                    view.state,
                    ControlCommandState::Applied | ControlCommandState::Settled
                )
            {
                continue;
            }
            let input = match &view.request.parameters {
                ControlParameters::AddAgent { input, .. }
                | ControlParameters::ReplaceFutureAgent { input, .. } => input,
                _ => continue,
            };
            if &input.activation.node_id == node {
                if self.applied_control(&view)?.is_none() {
                    return Err(error("child command has no exact canonical admission"));
                }
                self.child_proposal(&view)?;
                return Ok(Some(view));
            }
        }
        Ok(None)
    }
    fn child_proposal(&self, view: &CommandReceiptView) -> Result<NativeChildProposal> {
        let CommandSourceRecord::Agent { activation, .. } = &view.source else {
            return Err(error("child requires a live Agent command source"));
        };
        let (input, dependencies, replacement) = match &view.request.parameters {
            ControlParameters::AddAgent {
                input,
                dependencies,
            } => (input, dependencies.clone(), None),
            ControlParameters::ReplaceFutureAgent {
                input,
                target,
                rewire_dependents,
            } => (
                input,
                vec![],
                Some((target.clone(), rewire_dependents.clone())),
            ),
            _ => return Err(error("child requires an exact graph command")),
        };
        let grant = input
            .grant
            .as_ref()
            .ok_or_else(|| error("child has no grant"))?;
        let ActivationEvidenceContent::Grant { policy } = self
            .content
            .resolve_activation_evidence(&grant.evidence)
            .map_err(error)?
        else {
            return Err(error("child grant is missing"));
        };
        let ActivationEvidenceContent::Guidance { text } = self
            .content
            .resolve_activation_evidence(&policy.issuer_evidence)
            .map_err(error)?
        else {
            return Err(error("child proposal is missing"));
        };
        let proposal: NativeChildProposal = serde_json::from_str(text).map_err(error)?;
        if proposal.kind != "native_coordinator_child_v1"
            || proposal.parent != *activation
            || proposal.node_id != input.activation.node_id
            || proposal.conversation_id != input.conversation_id
            || proposal.worker.definition != input.definition
            || proposal.worker.limits != policy.limits
            || !dependencies.is_empty()
            || proposal.replacement != replacement
        {
            return Err(error(
                "child input differs from its retained Coordinator proposal",
            ));
        }
        Ok(proposal)
    }
    pub(super) fn coordinator_workers(
        &self,
        input: &ActivationInputManifest,
        provider: Arc<dyn axocoatl_llm::LlmProvider>,
    ) -> Result<CoordinatorWorkerPreparation> {
        let grant = input
            .grant
            .as_ref()
            .ok_or_else(|| error("Coordinator needs its approved grant"))?;
        let policy = self
            .authority
            .grant_policy(grant.grant_id.as_str())
            .map_err(error)?;
        let approved =
            crate::bootstrap::session_team::approved_coordinator_policy(&self.content, &policy)
                .map_err(error)?
                .ok_or_else(|| {
                    error("Coordinator has no retained Worker and delegation approval")
                })?;
        self.validate_coordinator_resource(input, &approved.resource)?;
        let mut workers = vec![];
        let mut tools = vec![];
        for worker in &approved.workers {
            let ActivationEvidenceContent::Definition {
                profile,
                configuration,
                ..
            } = self
                .content
                .resolve_activation_evidence(&worker.definition.snapshot)
                .map_err(error)?
            else {
                return Err(error("approved Worker definition is missing"));
            };
            let config: AgentConfig = serde_json::from_str(configuration).map_err(error)?;
            if config.role != AgentRole::Worker
                || profile.definition != worker.definition.definition_id.as_str()
                || !policy.profiles.contains(profile)
                || config.provider != profile.provider
                || config.model != profile.model
                || config.tools != profile.tools
            {
                return Err(error("approved Worker differs from its captured profile"));
            }
            tools.extend(config.tools.clone());
            {
                workers.push((
                    WorkerConfig {
                        id: config.id,
                        name: config.name,
                        system_prompt: config.system_prompt.unwrap_or_default(),
                        tools: config.tools,
                        model: config.model,
                        provider: Some(provider.clone()),
                        token_budget: config.token_budget,
                        sampling: config.sampling,
                        memory: config.memory,
                        session_context: None,
                        project_instructions_root: None,
                    },
                    worker.template_id.clone(),
                ));
            }
        }
        tools.sort();
        tools.dedup();
        Ok((workers, tools, approved.htn_methods_yaml))
    }
    fn validate_coordinator_resource(
        &self,
        input: &ActivationInputManifest,
        approved: &crate::bootstrap::session_team::ApprovedCoordinatorResource,
    ) -> Result<()> {
        let RepositoryInput::Recorded { snapshot } = &input.repository else {
            return Err(error(
                "Coordinator delegation requires its reviewed Session repository",
            ));
        };
        let owner = self
            .repository_owners
            .get(snapshot)
            .ok_or_else(|| error("Coordinator repository owner is missing"))?;
        repository::validate_retained_repository(self, owner, snapshot)?;
        let actual = owner.metadata();
        let reviewed = axocoatl_core::SecureDir::open(&approved.working_dir)
            .map_err(error)?
            .inode_identity()
            .map_err(error)?;
        if actual.session_id != approved.session_id
            || actual.workspace_id != approved.workspace_id
            || actual.environment_generation != approved.environment_generation
            || actual.backend != approved.backend
            || actual.host_workspace_inode != reviewed
        {
            return Err(error("Coordinator runtime differs from the reviewed delegation resource; review its team budget"));
        }
        Ok(())
    }
    pub(super) fn child_run_control(&self, activation: &ActivationRef) -> Result<AgentRunControl> {
        if let Some(origin) = self.native_child_origin(&activation.node_id)? {
            if let CommandSourceRecord::Agent {
                activation: parent, ..
            } = origin.source
            {
                let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
                let current = snapshot
                    .contract()
                    .activations()
                    .iter()
                    .rev()
                    .find(|item| item.activation.node_id == parent.node_id)
                    .map(|item| &item.activation)
                    .unwrap_or(&parent);
                if let Some(bound) = self
                    .bound
                    .get(&current.activation_id)
                    .filter(|bound| bound.activation == *current)
                {
                    return Ok(bound
                        .control
                        .child(AgentRunId::new(activation.activation_id.as_str())));
                }
            }
        }
        Ok(AgentRunControl::new(AgentRunId::new(
            activation.activation_id.as_str(),
        )))
    }
    pub(super) fn child_reservation(
        &self,
        view: &CommandReceiptView,
    ) -> Result<DelegatedGrantReservation> {
        let proposal = self.child_proposal(view)?;
        let CommandSourceRecord::Agent {
            grant_id,
            grant_revision,
            ..
        } = &view.source
        else {
            unreachable!()
        };
        let input = self
            .graph_control_input(view)
            .ok_or_else(|| error("child input is missing"))?;
        let grant = input
            .grant
            .as_ref()
            .ok_or_else(|| error("child grant is missing"))?;
        let ActivationEvidenceContent::Grant { policy } = self
            .content
            .resolve_activation_evidence(&grant.evidence)
            .map_err(error)?
        else {
            return Err(error("child grant is missing"));
        };
        Ok(DelegatedGrantReservation {
            parent_grant_id: grant_id.clone(),
            parent_grant_revision: *grant_revision,
            parent_activation: proposal.parent,
            command_id: view.request.command_id.clone(),
            template: proposal.worker.definition,
            admission_evidence: policy.issuer_evidence.clone(),
            limits: policy.limits.clone(),
        })
    }
    pub(super) fn validate_child_graph(&self, view: &CommandReceiptView) -> Result<()> {
        let proposal = self.child_proposal(view)?;
        let bound = self
            .bound
            .get(&proposal.parent.activation_id)
            .filter(|bound| bound.activation == proposal.parent)
            .ok_or_else(|| error("Coordinator command has no current execution owner"))?;
        let parent = self.current(&proposal.parent)?;
        let parent_input = &parent
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation == proposal.parent)
            .ok_or_else(|| error("Coordinator activation is missing"))?
            .input;
        let parent_policy = self
            .authority
            .grant_policy(bound.grant.grant_id.as_str())
            .map_err(error)?;
        let approved = crate::bootstrap::session_team::approved_coordinator_policy(
            &self.content,
            &parent_policy,
        )
        .map_err(error)?
        .ok_or_else(|| error("Coordinator approval is missing"))?;
        if !approved.workers.contains(&proposal.worker) {
            return Err(error(
                "proposed Worker is outside the approved templates and limits",
            ));
        }
        self.validate_coordinator_resource(parent_input, &approved.resource)?;
        let input = self
            .graph_control_input(view)
            .ok_or_else(|| error("child input is missing"))?;
        if input.repository != parent_input.repository
            || input.attachments != parent_input.attachments
        {
            return Err(error(
                "child changed the exact parent repository or attachment selection",
            ));
        }
        let grant = input
            .grant
            .as_ref()
            .ok_or_else(|| error("child grant is missing"))?;
        let ActivationEvidenceContent::Grant { policy } = self
            .content
            .resolve_activation_evidence(&grant.evidence)
            .map_err(error)?
        else {
            return Err(error("child grant is missing"));
        };
        let reservation = self.child_reservation(view)?;
        if self
            .authority
            .delegated_parent(&policy.id)
            .ok()
            .flatten()
            .as_ref()
            == Some(&reservation)
        {
            return Ok(());
        }
        self.authority
            .validate_child_grant(&bound.lease, policy, &reservation, now_ms()?)
            .map_err(error)
    }
}

/// Incidental runtime actor ids and parent retry generations do not make an
/// accepted child a new task. Exact semantic repeats reattach it.
pub(super) fn native_child_digest(
    parent: &ActivationRef,
    request: &ChildExecutionRequest,
    worker: &NativeCoordinatorWorker,
    replacement: &Option<(TurnNodeId, Vec<TurnNodeId>)>,
) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                parent.session_id.clone(),
                parent.turn_id.clone(),
                parent.node_id.clone(),
                request.subtask_index,
                &request.task_name,
                &request.task_input,
                &request.tools,
                &request.provider_id,
                &request.model,
                &request.attachments,
                worker,
                replacement
            ))
            .map_err(error)?
        )
    ))
}

/// The child node and the command that admits it, both named by the digest.
pub(super) fn native_child_identity(digest: &str) -> Result<(TurnNodeId, CommandId)> {
    Ok((
        TurnNodeId::new(format!("child-{digest}")).map_err(error)?,
        CommandId::new(format!("child-command-{digest}")).map_err(error)?,
    ))
}

impl SessionDispatchController {
    pub(super) fn schedule_coordinator_child(
        &self,
        parent: &ActivationRef,
        request: &ChildExecutionRequest,
        control: AgentRunControl,
    ) -> Result<Box<dyn AdmittedChildExecution>> {
        self.admit_coordinator_child(parent, request, control, None)
    }
    pub(super) fn replace_coordinator_future(
        &self,
        parent: &ActivationRef,
        request: &ChildExecutionRequest,
        target: TurnNodeId,
        rewire: Vec<TurnNodeId>,
    ) -> Result<()> {
        let control = self
            .lock()?
            .bound
            .get(&parent.activation_id)
            .ok_or_else(|| error("source is missing"))?
            .control
            .clone();
        self.admit_coordinator_child(parent, request, control, Some((target, rewire)))?;
        Ok(())
    }
    pub(super) fn admit_coordinator_child(
        &self,
        parent: &ActivationRef,
        request: &ChildExecutionRequest,
        control: AgentRunControl,
        replacement: Option<(TurnNodeId, Vec<TurnNodeId>)>,
    ) -> Result<Box<dyn AdmittedChildExecution>> {
        let mut state = self.lock()?;
        state.execution_admission()?;
        let snapshot = state.current(parent)?;
        let parent_input = &snapshot
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation == *parent)
            .ok_or_else(|| error("the delegating Agent's input is unavailable"))?
            .input;
        let bound = state
            .bound
            .get(&parent.activation_id)
            .filter(|bound| bound.activation == *parent)
            .cloned()
            .ok_or_else(|| error("the delegating Agent is no longer running"))?;
        let policy = state
            .authority
            .grant_policy(bound.grant.grant_id.as_str())
            .map_err(error)?;
        let approved =
            crate::bootstrap::session_team::approved_coordinator_policy(&state.content, &policy)
                .map_err(error)?
                .ok_or_else(|| error("this Agent has no approved helper templates"))?;
        state.validate_coordinator_resource(parent_input, &approved.resource)?;
        let mut candidates = vec![];
        for template in &approved.workers {
            let ActivationEvidenceContent::Definition {
                profile,
                configuration,
                ..
            } = state
                .content
                .resolve_activation_evidence(&template.definition.snapshot)
                .map_err(error)?
            else {
                return Err(error("approved Worker definition is unavailable"));
            };
            let config: AgentConfig = serde_json::from_str(configuration).map_err(error)?;
            let selected = template.template_id == request.logical_worker_id
                || (request.logical_worker_id.starts_with("adhoc-") && template.adhoc_allowed);
            if selected
                && config.role == AgentRole::Worker
                && request.provider_id == profile.provider
                && request.model == profile.model
                && request
                    .tools
                    .iter()
                    .all(|tool| profile.tools.contains(tool))
            {
                candidates.push(template.clone());
            }
        }
        if candidates.len() != 1 {
            return Err(error(
                "the child request does not match exactly one approved Worker template and profile",
            ));
        }
        let worker = candidates.remove(0);
        let digest = native_child_digest(parent, request, &worker, &replacement)?;
        let (node_id, command_id) = native_child_identity(&digest)?;
        if state.native_child_origin(&node_id)?.is_some() {
            return Ok(Box::new(CanonicalChildWait {
                controller: self.clone(),
                node_id,
                control,
            }));
        }
        let conversation_id =
            NodeConversationId::new(format!("child-conversation-{digest}")).map_err(error)?;
        let proposal = NativeChildProposal {
            kind: "native_coordinator_child_v1".into(),
            parent: parent.clone(),
            request: request.clone(),
            worker: worker.clone(),
            node_id: node_id.clone(),
            conversation_id: conversation_id.clone(),
            replacement: replacement.clone(),
        };
        let proposal_ref = state
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: serde_json::to_string(&proposal).map_err(error)?,
            })
            .map_err(error)?
            .reference()
            .clone();
        let ActivationEvidenceContent::Definition { profile, .. } = state
            .content
            .resolve_activation_evidence(&worker.definition.snapshot)
            .map_err(error)?
        else {
            return Err(error("Worker profile is missing"));
        };
        let child_grant = AuthorityGrant {
            id: format!("child-grant-{digest}"),
            revision: 1,
            issuer_evidence: proposal_ref,
            holder: node_id.clone(),
            descendants: vec![],
            allow_stop_descendants: false,
            delegation: None,
            profiles: vec![profile.clone()],
            conditions: vec![],
            limits: worker.limits.clone(),
            expires_at_ms: policy.expires_at_ms,
        };
        let child_grant_ref = state
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Grant {
                policy: child_grant.clone(),
            })
            .map_err(error)?
            .reference()
            .clone();
        let budget = state
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Budget {
                limits: worker.limits,
            })
            .map_err(error)?
            .reference()
            .clone();
        let task = state
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: request.task_input.clone(),
            })
            .map_err(error)?
            .reference()
            .clone();
        let mut guidance = parent_input.guidance.clone();
        guidance.push(task);
        let input = ActivationInputManifest {
            manifest_id: InputManifestId::new(format!("child-input-{digest}")).map_err(error)?,
            activation: ActivationRef {
                session_id: parent.session_id.clone(),
                turn_id: parent.turn_id.clone(),
                execution_epoch_id: parent.execution_epoch_id.clone(),
                node_id: node_id.clone(),
                activation_id: ActivationId::new(format!("child-activation-{digest}"))
                    .map_err(error)?,
                generation: 1,
            },
            definition: worker.definition,
            conversation_id,
            starting_savepoint: ConversationSavepoint::Empty,
            parents: vec![],
            guidance,
            attachments: parent_input.attachments.clone(),
            repository: parent_input.repository.clone(),
            budget,
            grant: Some(GrantSnapshotRef {
                grant_id: GrantId::new(&child_grant.id).map_err(error)?,
                revision: 1,
                evidence: child_grant_ref,
            }),
            revision_context: None,
        };
        let source = state
            .authority
            .attest_control_source(&bound.lease, now_ms()?)
            .map_err(error)?;
        let receipt = state.submit_control_command(
            ControlCommandRequest {
                schema_version: 1,
                command_id,
                session_id: parent.session_id.clone(),
                turn_id: parent.turn_id.clone(),
                execution_epoch_id: parent.execution_epoch_id.clone(),
                expected_turn_revision: snapshot.contract().revision(),
                expected_graph_revision: snapshot
                    .contract()
                    .graph()
                    .ok_or_else(|| error("the turn graph is missing"))?
                    .revision,
                issued_at_ms: now_ms()?,
                parameters: match replacement {
                    Some((target, rewire_dependents)) => ControlParameters::ReplaceFutureAgent {
                        input: Box::new(input),
                        target,
                        rewire_dependents,
                    },
                    None => ControlParameters::AddAgent {
                        input: Box::new(input),
                        dependencies: vec![],
                    },
                },
            },
            source,
        )?;
        if !matches!(
            receipt.view().state,
            ControlCommandState::Applied | ControlCommandState::Settled
        ) {
            let reason = match &receipt.view().last_transition {
                Some(ControlTransition::Rejected { failure }) => failure.message.as_str(),
                _ => "its admission did not complete",
            };
            return Err(error(format!("the child Agent was not admitted: {reason}")));
        }
        Ok(Box::new(CanonicalChildWait {
            controller: self.clone(),
            node_id,
            control,
        }))
    }
}
struct CanonicalChildWait {
    controller: SessionDispatchController,
    node_id: TurnNodeId,
    control: AgentRunControl,
}
#[async_trait]
impl AdmittedChildExecution for CanonicalChildWait {
    async fn run(
        self: Box<Self>,
    ) -> std::result::Result<MeasuredAgentRunOutcome, AgentExecutionFailure> {
        let failure = |reason: String| {
            AgentExecutionFailure::new(
                reason,
                MeasuredTokenUsage {
                    usage: TokenUsageStats::default(),
                    complete: false,
                },
            )
        };
        let changed = self
            .controller
            .lock()
            .map_err(|e| failure(e.to_string()))?
            .changed
            .clone();
        let mut cancellation_forwarded = false;
        loop {
            let notification = changed.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            if self.control.is_cancelled() && !cancellation_forwarded {
                let mut state = self.controller.lock().map_err(|e| failure(e.to_string()))?;
                let snapshot = state
                    .canonical
                    .snapshot(&state.turn_id)
                    .map_err(|e| failure(e.to_string()))?;
                if let Some(item) = snapshot.contract().activations().iter().rev().find(|item| {
                    item.activation.node_id == self.node_id
                        && item.state == ActivationState::Running
                }) {
                    if let Some(bound) = state.bound.get(&item.activation.activation_id).cloned() {
                        let revision = state
                            .authority
                            .revision()
                            .map_err(|e| failure(e.to_string()))?;
                        let result = state
                            .authority
                            .stop_activation(&item.activation, revision)
                            .map_err(error);
                        bound.control.cancel();
                        state
                            .fail_closed(result)
                            .map_err(|e| failure(e.to_string()))?;
                    }
                }
                cancellation_forwarded = true;
            }
            {
                let state = self.controller.lock().map_err(|e| failure(e.to_string()))?;
                let snapshot = state
                    .canonical
                    .snapshot(&state.turn_id)
                    .map_err(|e| failure(e.to_string()))?;
                let latest = snapshot
                    .contract()
                    .activations()
                    .iter()
                    .rev()
                    .find(|item| item.activation.node_id == self.node_id);
                if let Some(item) = latest {
                    let measured = state
                        .authority
                        .provider_usage(&item.activation)
                        .map(|usage| usage.tokens)
                        .unwrap_or(MeasuredTokenUsage {
                            usage: TokenUsageStats::default(),
                            complete: false,
                        });
                    if item.state == ActivationState::Accepted {
                        let reservation = state
                            .content
                            .activation_output_reservation(&snapshot, &item.activation)
                            .map_err(|e| failure(e.to_string()))?
                            .ok_or_else(|| {
                                failure("accepted child output reservation is missing".into())
                            })?;
                        let output = state
                            .content
                            .activation_output_settlement(&reservation)
                            .map_err(|e| failure(e.to_string()))?
                            .ok_or_else(|| failure("accepted child output is missing".into()))?;
                        if item.output.as_ref() != Some(output.reference())
                            || output.complete_output().is_none()
                            || item.checkpoint.is_none()
                        {
                            return Err(failure(
                                "child has no complete canonical accepted evidence".into(),
                            ));
                        }
                        state
                            .memory
                            .checkpoint(item.checkpoint.as_ref().unwrap())
                            .map_err(|e| failure(e.to_string()))?;
                        return Ok(MeasuredAgentRunOutcome {
                            outcome: AgentRunOutcome::Completed(AgentOutput {
                                content: output.content().output.text.clone(),
                                tool_calls: vec![],
                                token_usage: measured.usage.clone(),
                            }),
                            token_usage: measured,
                        });
                    }
                    if matches!(
                        item.state,
                        ActivationState::Failed
                            | ActivationState::Interrupted
                            | ActivationState::Superseded
                    ) {
                        return Err(AgentExecutionFailure::new(
                            "the child Agent stopped or failed before giving an accepted answer",
                            measured,
                        ));
                    }
                }
                if snapshot.contract().state() != Some(LogicalTurnState::Running)
                    || (self.control.is_cancelled() && latest.is_none())
                {
                    return Err(failure(
                        "the turn stopped before the child Agent finished".into(),
                    ));
                }
            }
            if cancellation_forwarded {
                notification.await;
            } else {
                tokio::select! {_ = notification => {},_ = self.control.cancelled()=>{}}
            }
        }
    }
}

impl DispatchState {
    pub(super) fn validate_delegated_control_scope(
        &self,
        view: &CommandReceiptView,
        grant: &AuthorityGrant,
    ) -> Result<()> {
        let delegation = grant
            .delegation
            .as_ref()
            .ok_or_else(|| error("control has no delegated operation policy"))?;
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        if delegation.scope.session_id != snapshot.owner().session_id
            || delegation.scope.turn_id != self.turn_id
            || Some(&delegation.scope.task) != snapshot.request_ref()
        {
            return Err(error("delegation belongs to another accepted task"));
        }
        let operation = view
            .request
            .parameters
            .delegated_operation()
            .ok_or_else(|| error("this action requires the human channel"))?;
        let permission = delegation
            .operations
            .iter()
            .find(|permission| permission.operation == operation)
            .ok_or_else(|| error("operation is outside the explicit delegated grant"))?;
        let mut targets = Vec::new();
        let mut future = HashSet::new();
        match &view.request.parameters {
            ControlParameters::StopActivation { activation }
            | ControlParameters::SteerActivation { activation, .. }
            | ControlParameters::RetryActivation { activation, .. }
            | ControlParameters::ResumeBlocked { activation, .. } => {
                targets.push(activation.node_id.clone())
            }
            ControlParameters::ReviseActivation {
                activation,
                input,
                invalidate,
                ..
            } => {
                targets.push(activation.node_id.clone());
                targets.push(input.activation.node_id.clone());
                targets.extend(
                    invalidate
                        .iter()
                        .map(|activation| activation.node_id.clone()),
                );
            }
            ControlParameters::AddAgent {
                input,
                dependencies,
            } => {
                targets.push(input.activation.node_id.clone());
                future.insert(input.activation.node_id.clone());
                targets.extend(dependencies.clone());
            }
            ControlParameters::ReplaceFutureAgent {
                target,
                input,
                rewire_dependents,
            } => {
                targets.push(target.clone());
                targets.push(input.activation.node_id.clone());
                future.insert(input.activation.node_id.clone());
                targets.extend(rewire_dependents.clone());
            }
            ControlParameters::ContinueTurn { plan, .. } => {
                for selection in &plan.selections {
                    if let ContinuationSelection::Revise {
                        previous,
                        input,
                        invalidated_descendants,
                        evidence,
                    } = selection
                    {
                        // Continuing an epoch must not grant the separately
                        // reviewed permission to revise accepted work.
                        let mut revision = view.clone();
                        revision.request.parameters = ControlParameters::ReviseActivation {
                            activation: previous.clone(),
                            input: input.clone(),
                            instruction: evidence.clone(),
                            invalidate: invalidated_descendants.clone(),
                        };
                        self.validate_delegated_control_scope(&revision, grant)?;
                    }
                }
                targets.extend(
                    snapshot
                        .contract()
                        .graph()
                        .ok_or_else(|| error("delegated graph is absent"))?
                        .nodes
                        .iter()
                        .map(|node| node.node_id.clone()),
                );
            }
            ControlParameters::FinishTurn { .. } => targets.extend(
                snapshot
                    .contract()
                    .graph()
                    .ok_or_else(|| error("delegated graph is absent"))?
                    .nodes
                    .iter()
                    .map(|node| node.node_id.clone()),
            ),
        }
        for node in targets {
            let allowed = match &permission.targets {
                DelegatedTargetScope::Nodes { nodes } => {
                    !future.contains(&node) && nodes.contains(&node)
                }
                DelegatedTargetScope::Subtree {
                    root,
                    include_future_descendants,
                } => {
                    (future.contains(&node) && *include_future_descendants && root == &grant.holder)
                        || self.node_in_delegated_subtree(root, &node)?
                }
            };
            if !allowed {
                return Err(error(
                    "control affects work outside its exact delegated subtree",
                ));
            }
        }
        let graph = snapshot
            .contract()
            .graph()
            .ok_or_else(|| error("delegated graph is absent"))?;
        if graph.nodes.len().saturating_add(future.len())
            > delegation.graph_limits.max_nodes as usize
            || graph.dependencies.len() > delegation.graph_limits.max_edges as usize
        {
            return Err(error("control exceeds the approved graph size"));
        }
        Ok(())
    }
    fn node_in_delegated_subtree(&self, root: &TurnNodeId, target: &TurnNodeId) -> Result<bool> {
        if root == target {
            return Ok(true);
        }
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let mut frontier = vec![root.clone()];
        let mut seen = HashSet::new();
        while let Some(parent) = frontier.pop() {
            if !seen.insert(parent.clone()) {
                continue;
            }
            if let Some(graph) = snapshot.contract().graph() {
                for edge in graph
                    .dependencies
                    .iter()
                    .filter(|edge| edge.parent == parent)
                {
                    if &edge.child == target {
                        return Ok(true);
                    }
                    frontier.push(edge.child.clone());
                }
            }
            for view in self.control_plane_commands()? {
                let CommandSourceRecord::Agent { activation, .. } = &view.source else {
                    continue;
                };
                if activation.node_id != parent
                    || !matches!(
                        view.state,
                        ControlCommandState::Applied | ControlCommandState::Settled
                    )
                {
                    continue;
                }
                let input = match &view.request.parameters {
                    ControlParameters::AddAgent { input, .. }
                    | ControlParameters::ReplaceFutureAgent { input, .. } => input,
                    _ => continue,
                };
                if self.applied_control(&view)?.is_none() {
                    return Err(error("delegated child lacks canonical graph proof"));
                }
                self.child_proposal(&view)?;
                if &input.activation.node_id == target {
                    return Ok(true);
                }
                frontier.push(input.activation.node_id.clone());
            }
        }
        Ok(false)
    }
}
