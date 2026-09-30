//! Current-turn graph edits use the existing canonical command and driver owner.
//! New grant installation cannot execute a node absent from the canonical graph.
use super::*;
use axocoatl_session::control_command::{
    CommandReceiptView, CommandSourceRecord, ControlCommandState, ControlParameters,
};
use axocoatl_session::turn_contract::{
    DependencyEdge, GraphMutation, GraphNode, GraphSnapshotId, SessionTeamSlotId,
};

impl DispatchState {
    pub(super) fn graph_control_input<'a>(
        &self,
        view: &'a CommandReceiptView,
    ) -> Option<&'a ActivationInputManifest> {
        match &view.request.parameters {
            ControlParameters::AddAgent { input, .. }
            | ControlParameters::ReplaceFutureAgent { input, .. } => Some(input),
            _ => None,
        }
    }
    /// The graph revision a command makes. Replays re-derive it, so a node's
    /// `required` flag follows the retained proposal kind, never current state.
    pub(super) fn graph_control_event(
        &self,
        view: &CommandReceiptView,
    ) -> Result<Option<TurnContractEvent>> {
        graph_event(
            &self.canonical,
            &self.turn_id,
            view,
            !self.is_delegate_child(view)?,
        )
    }
    pub(super) fn validate_graph_control(&self, view: &CommandReceiptView) -> Result<()> {
        if self.is_isolated_ways()? {
            return Err(error("Isolated Ways use their existing candidate roster; cooperative graph edits are unavailable"));
        }
        let input = self
            .graph_control_input(view)
            .ok_or_else(|| error("missing graph input"))?;
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        if input.starting_savepoint != ConversationSavepoint::Empty
            || !input.parents.is_empty()
            || input.revision_context.is_some()
            || input.activation.generation != 1
        {
            return Err(error(
                "new current-turn work requires fresh input and an empty conversation",
            ));
        }
        let resolved = self
            .content
            .validate_proposed_input(&snapshot, input)
            .map_err(error)?;
        let ActivationEvidenceContent::Definition {
            profile,
            configuration,
            ..
        } = &resolved.definition
        else {
            return Err(error("graph definition is missing"));
        };
        let config: axocoatl_core::AgentConfig =
            serde_json::from_str(configuration).map_err(error)?;
        let child = matches!(view.source, CommandSourceRecord::Agent { .. });
        if (if child {
            config.role != axocoatl_core::AgentRole::Worker
        } else {
            !matches!(config.role, axocoatl_core::AgentRole::Autonomous)
                || config.id.0 != input.conversation_id.as_str()
        }) || config.provider != profile.provider
            || config.model != profile.model
            || config.tools != profile.tools
            || config.writes != profile.write_scope
            || profile.isolation != "in-process"
        {
            return Err(error(
                "graph definition differs from the supported native execution profile",
            ));
        }
        let reference = input
            .grant
            .as_ref()
            .ok_or_else(|| error("graph edit requires explicit human-approved limits"))?;
        let policy = resolved
            .grant
            .as_ref()
            .ok_or_else(|| error("graph grant is unavailable"))?;
        policy.validate().map_err(error)?;
        let issuer_matches = match &view.source {
            CommandSourceRecord::Human {
                request_evidence, ..
            } => policy.issuer_evidence == *request_evidence,
            CommandSourceRecord::Agent { .. } => {
                self.validate_child_graph(view)?;
                true
            }
        };
        if policy.revision != 1
            || policy.id != reference.grant_id.as_str()
            || reference.revision != 1
            || policy.holder != input.activation.node_id
            || !issuer_matches
            || !policy.descendants.is_empty()
            || policy.allow_stop_descendants
            || policy.delegation.is_some()
            || policy.profiles != vec![profile.clone()]
            || policy.limits != resolved.budget
            || policy.expires_at_ms <= now_ms()?
            || policy.limits.activations == 0
            || policy.limits.tokens == 0
        {
            return Err(error(
                "graph grant differs from the exact approved node, profile, budget or expiry",
            ));
        }
        // Require the actual provider profile captured by daemon preparation.
        if self
            .content
            .resolve_provider_profile(&self.canonical, &input.definition.snapshot)
            .map_err(error)?
            .is_none()
        {
            return Err(error(
                "graph definition has no captured native provider profile",
            ));
        }
        match &input.repository {
            RepositoryInput::Recorded {
                snapshot: reference,
            } => {
                let owner = self
                    .repository_owners
                    .get(reference)
                    .ok_or_else(|| error("graph edit has no retained repository owner"))?;
                repository::validate_retained_repository(self, owner, reference)?;
            }
            RepositoryInput::Unavailable if !profile.tools.is_empty() => {
                return Err(error("graph tools require an actual repository owner"))
            }
            _ => {}
        }
        let event = self
            .graph_control_event(view)?
            .ok_or_else(|| error("graph mutation is unavailable"))?;
        let mut preview = snapshot.contract().clone();
        preview
            .apply(&TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new(format!(
                    "graph-preview-{:x}",
                    Sha256::digest(serde_json::to_vec(&view.request).map_err(error)?)
                ))
                .map_err(error)?,
                expected_revision: view.request.expected_turn_revision,
                session_id: view.request.session_id.clone(),
                turn_id: view.request.turn_id.clone(),
                event,
            })
            .map_err(error)?;
        Ok(())
    }
    pub(super) fn apply_graph_control(&mut self, view: &CommandReceiptView) -> Result<()> {
        self.validate_graph_control(view)?;
        let input = self
            .graph_control_input(view)
            .ok_or_else(|| error("missing graph input"))?;
        let reference = input
            .grant
            .as_ref()
            .ok_or_else(|| error("missing graph grant"))?;
        let ActivationEvidenceContent::Grant { policy } = self
            .content
            .resolve_activation_evidence(&reference.evidence)
            .map_err(error)?
        else {
            return Err(error("missing graph policy"));
        };
        match &view.source {
            CommandSourceRecord::Human { .. } => self
                .authority
                .install_grant(policy.clone(), self.authority.revision().map_err(error)?)
                .map_err(error)?,
            CommandSourceRecord::Agent { activation, .. } => {
                let bound = self
                    .bound
                    .get(&activation.activation_id)
                    .filter(|bound| bound.activation == *activation)
                    .ok_or_else(|| error("child source lost its live owner"))?;
                self.authority
                    .reserve_child_grant(
                        &bound.lease,
                        policy.clone(),
                        self.child_reservation(view)?,
                        self.authority.revision().map_err(error)?,
                        now_ms()?,
                    )
                    .map_err(error)?;
            }
        }
        let event = self
            .graph_control_event(view)?
            .ok_or_else(|| error("missing graph operation"))?;
        let envelope = self.control_envelope(view, event)?;
        self.canonical.append(envelope.clone()).map_err(error)?;
        self.changed.notify_waiters();
        self.settle_control_event(view, &envelope)
    }
    pub(super) fn dynamic_node_input(
        &self,
        node: &TurnNodeId,
    ) -> Result<Option<AutonomousNodeInput>> {
        for view in self.control_plane_commands()? {
            let Some(input) = self.graph_control_input(&view) else {
                continue;
            };
            if input.activation.node_id != *node
                || !matches!(
                    view.state,
                    ControlCommandState::Applied | ControlCommandState::Settled
                )
            {
                continue;
            }
            if self.applied_control(&view)?.is_none() {
                return Err(error("dynamic node has no canonical graph admission"));
            }
            return Ok(Some(AutonomousNodeInput {
                node_id: node.clone(),
                guidance: input.guidance.clone(),
                attachments: input.attachments.clone(),
                repository: input.repository.clone(),
                budget: input.budget.clone(),
                grant: input.grant.clone(),
            }));
        }
        Ok(None)
    }
    pub(super) fn validate_retained_graph_controls(&self) -> Result<()> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        for item in snapshot.contract().graph_history() {
            let mut found = false;
            for view in self.control_plane_commands()? {
                if !matches!(
                    view.state,
                    ControlCommandState::Accepted
                        | ControlCommandState::Applied
                        | ControlCommandState::Settled
                ) {
                    continue;
                }
                let Some(event) = self.graph_control_event(&view)? else {
                    continue;
                };
                if let TurnContractEvent::ReviseGraph {
                    graph,
                    admission_evidence,
                    ..
                } = &event
                {
                    if graph.snapshot_id != item.resulting_snapshot {
                        continue;
                    }
                    if *admission_evidence != item.admission_evidence
                        || !matches!(
                            view.state,
                            ControlCommandState::Accepted
                                | ControlCommandState::Applied
                                | ControlCommandState::Settled
                        )
                        || self.applied_control(&view)?.is_none()
                    {
                        return Err(error("retained graph lacks its exact accepted control"));
                    }
                    let input = self
                        .graph_control_input(&view)
                        .ok_or_else(|| error("retained graph input is absent"))?;
                    self.content
                        .validate_proposed_input(&snapshot, input)
                        .map_err(error)?;
                    let grant = input
                        .grant
                        .as_ref()
                        .ok_or_else(|| error("retained graph grant is absent"))?;
                    let installed = self
                        .authority
                        .grant_status(grant.grant_id.as_str())
                        .map_err(error)?;
                    let ActivationEvidenceContent::Grant { policy } = self
                        .content
                        .resolve_activation_evidence(&grant.evidence)
                        .map_err(error)?
                    else {
                        return Err(error("retained graph grant is missing"));
                    };
                    // A later revocation is allowed; it cannot rewrite original approval.
                    if installed.policy.id != policy.id {
                        return Err(error("retained graph grant identity differs"));
                    }
                    if matches!(view.source, CommandSourceRecord::Agent { .. })
                        && self
                            .authority
                            .delegated_parent(&policy.id)
                            .map_err(error)?
                            .as_ref()
                            != Some(&self.child_reservation(&view)?)
                    {
                        return Err(error(
                            "retained child lacks its exact parent budget reservation",
                        ));
                    }
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(error(
                    "retained graph lacks its authenticated control proof",
                ));
            }
        }
        Ok(())
    }
}

use crate::bootstrap::session_graph::{
    HumanGraphEditAction, HumanGraphEditPreview, HumanGraphEditRequest,
};
use axocoatl_session::control_command::{ControlCommandRequest, TrustedCommandSource};
impl SessionDispatchController {
    pub(crate) fn repeated_human_graph_edit(
        &self,
        request: &HumanGraphEditRequest,
        review: Option<&str>,
    ) -> Result<Option<HumanGraphEditPreview>> {
        let state = self.lock()?;
        state.ready()?;
        if request.session_id != state.canonical.owner().session_id
            || request.turn_id != state.turn_id
        {
            return Err(error("graph edit belongs to another current turn"));
        }
        let Some(receipt) = state.commands.receipt(&request.command_id).map_err(error)? else {
            return Ok(None);
        };
        let view = receipt.view();
        let CommandSourceRecord::Human {
            request_evidence, ..
        } = &view.source
        else {
            return Err(error("graph command belongs to another source"));
        };
        let ActivationEvidenceContent::Guidance { text } = state
            .content
            .resolve_activation_evidence(request_evidence)
            .map_err(error)?
        else {
            return Err(error("original graph approval is missing"));
        };
        let original: HumanGraphEditRequest = serde_json::from_str(text).map_err(error)?;
        if original != *request {
            return Err(error("graph command identity already has a different body"));
        }
        let event = state
            .graph_control_event(view)?
            .ok_or_else(|| error("saved graph command has no graph"))?;
        let TurnContractEvent::ReviseGraph { graph, .. } = event else {
            return Err(error("saved graph command differs"));
        };
        let review_digest = graph_review_digest(request, &view.request.parameters, &graph)?;
        if review.is_some_and(|digest| digest != review_digest) {
            return Err(error("saved graph review differs from this Apply"));
        }
        Ok(Some(HumanGraphEditPreview {
            request: request.clone(),
            review_digest,
            graph,
            receipt: Some(view.clone()),
        }))
    }
    pub(crate) fn prepare_human_graph_edit(
        &self,
        request: HumanGraphEditRequest,
        definition: DefinitionSnapshotRef,
        review: Option<&str>,
    ) -> Result<HumanGraphEditPreview> {
        if let Some(receipt) = self.repeated_human_graph_edit(&request, review)? {
            return Ok(receipt);
        }
        let mut state = self.lock()?;
        state.execution_admission()?;
        request.validate().map_err(error)?;
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        if request.session_id != snapshot.owner().session_id
            || request.turn_id != *snapshot.turn_id()
        {
            return Err(error("graph edit belongs to another current turn"));
        }
        let id = request.identity().map_err(error)?;
        let approval = state
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: serde_json::to_string(&request).map_err(error)?,
            })
            .map_err(error)?
            .reference()
            .clone();
        let task = state
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: request.task.clone(),
            })
            .map_err(error)?
            .reference()
            .clone();
        let profile = match state
            .content
            .resolve_activation_evidence(&definition.snapshot)
            .map_err(error)?
        {
            ActivationEvidenceContent::Definition { profile, .. } => profile.clone(),
            _ => return Err(error("graph definition is missing")),
        };
        let node_id = TurnNodeId::new(format!("dynamic-node-{id}")).map_err(error)?;
        let limits = request
            .agent
            .limits
            .clone()
            .ok_or_else(|| error("graph budget is missing"))?;
        let policy = AuthorityGrant {
            id: format!("dynamic-grant-{id}"),
            revision: 1,
            issuer_evidence: approval.clone(),
            holder: node_id.clone(),
            descendants: vec![],
            allow_stop_descendants: false,
            delegation: None,
            profiles: vec![profile],
            conditions: vec![],
            limits: limits.clone(),
            expires_at_ms: request
                .agent
                .expires_at_ms
                .ok_or_else(|| error("graph expiry is missing"))?,
        };
        let budget = state
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Budget { limits })
            .map_err(error)?
            .reference()
            .clone();
        let grant_evidence = state
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Grant {
                policy: policy.clone(),
            })
            .map_err(error)?
            .reference()
            .clone();
        let repository = match state
            .repository_owners
            .keys()
            .collect::<Vec<_>>()
            .as_slice()
        {
            [reference] => RepositoryInput::Recorded {
                snapshot: (*reference).clone(),
            },
            [] => RepositoryInput::Unavailable,
            _ => return Err(error("graph resource is ambiguous")),
        };
        let input = Box::new(ActivationInputManifest {
            manifest_id: InputManifestId::new(format!("dynamic-input-{id}")).map_err(error)?,
            activation: ActivationRef {
                session_id: request.session_id.clone(),
                turn_id: request.turn_id.clone(),
                execution_epoch_id: request.execution_epoch_id.clone(),
                node_id,
                activation_id: ActivationId::new(format!("dynamic-activation-{id}"))
                    .map_err(error)?,
                generation: 1,
            },
            definition,
            conversation_id: NodeConversationId::new(format!("dynamic-conversation-{id}"))
                .map_err(error)?,
            starting_savepoint: ConversationSavepoint::Empty,
            parents: vec![],
            guidance: vec![
                snapshot
                    .request_ref()
                    .ok_or_else(|| error("turn request is missing"))?
                    .clone(),
                task,
            ],
            attachments: vec![],
            repository,
            budget,
            grant: Some(GrantSnapshotRef {
                grant_id: GrantId::new(policy.id).map_err(error)?,
                revision: 1,
                evidence: grant_evidence,
            }),
            revision_context: None,
        });
        let parameters = match request.action {
            HumanGraphEditAction::Add => ControlParameters::AddAgent {
                input,
                dependencies: request.dependencies.clone(),
            },
            HumanGraphEditAction::Replace => ControlParameters::ReplaceFutureAgent {
                target: request
                    .replacement
                    .clone()
                    .ok_or_else(|| error("replacement target missing"))?,
                input,
                rewire_dependents: request.rewire_dependents.clone(),
            },
        };
        let canonical = ControlCommandRequest {
            schema_version: 1,
            command_id: request.command_id.clone(),
            session_id: request.session_id.clone(),
            turn_id: request.turn_id.clone(),
            execution_epoch_id: request.execution_epoch_id.clone(),
            expected_turn_revision: request.expected_turn_revision,
            expected_graph_revision: request.expected_graph_revision,
            issued_at_ms: now_ms()?,
            parameters,
        };
        let source = TrustedCommandSource::human(
            request.session_id.clone(),
            request.turn_id.clone(),
            approval,
        );
        let view = CommandReceiptView {
            request: canonical.clone(),
            source: source.record().clone(),
            revision: 0,
            state: ControlCommandState::Requested,
            last_transition: None,
        };
        state.validate_control(&view)?;
        let TurnContractEvent::ReviseGraph { graph, .. } = state
            .graph_control_event(&view)?
            .ok_or_else(|| error("missing graph proposal"))?
        else {
            return Err(error("wrong graph proposal"));
        };
        let review_digest = graph_review_digest(&request, &canonical.parameters, &graph)?;
        if review.is_some_and(|digest| digest != review_digest) {
            return Err(error("graph review changed; Preview again before Apply"));
        }
        let receipt = if review.is_some() {
            Some(
                state
                    .submit_control_command(canonical, source)?
                    .view()
                    .clone(),
            )
        } else {
            None
        };
        Ok(HumanGraphEditPreview {
            request,
            review_digest,
            graph,
            receipt,
        })
    }
}
fn graph_review_digest(
    request: &HumanGraphEditRequest,
    parameters: &ControlParameters,
    graph: &TurnGraphSnapshot,
) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(request, parameters, graph)).map_err(error)?)
    ))
}

fn graph_input(view: &CommandReceiptView) -> Option<&ActivationInputManifest> {
    match &view.request.parameters {
        ControlParameters::AddAgent { input, .. }
        | ControlParameters::ReplaceFutureAgent { input, .. } => Some(input),
        _ => None,
    }
}
fn graph_event(
    canonical: &SessionExecutionStore,
    turn: &LogicalTurnId,
    view: &CommandReceiptView,
    required: bool,
) -> Result<Option<TurnContractEvent>> {
    let Some(input) = graph_input(view) else {
        return Ok(None);
    };
    let snapshot = canonical.snapshot(turn).map_err(error)?;
    let old = snapshot
        .contract()
        .graph()
        .filter(|graph| graph.revision == view.request.expected_graph_revision)
        .or_else(|| {
            snapshot
                .contract()
                .graph_history()
                .iter()
                .map(|item| &item.previous)
                .find(|graph| graph.revision == view.request.expected_graph_revision)
        })
        .ok_or_else(|| error("graph command has no exact predecessor"))?;
    let evidence = match &view.source {
        CommandSourceRecord::Human {
            request_evidence, ..
        } => request_evidence.clone(),
        CommandSourceRecord::Agent { .. } => input
            .grant
            .as_ref()
            .ok_or_else(|| error("delegated graph grant is missing"))?
            .evidence
            .clone(),
    };
    let mut graph = old.clone();
    graph.revision = old
        .revision
        .checked_add(1)
        .ok_or_else(|| error("graph revision overflow"))?;
    graph.snapshot_id = GraphSnapshotId::new(format!(
        "graph-{:x}",
        Sha256::digest(
            serde_json::to_vec(&(snapshot.journal_id(), &view.request.command_id, "graph"))
                .map_err(error)?
        )
    ))
    .map_err(error)?;
    let node = GraphNode {
        node_id: input.activation.node_id.clone(),
        slot_id: SessionTeamSlotId::new(format!(
            "dynamic-slot-{}",
            input.activation.activation_id.as_str()
        ))
        .map_err(error)?,
        definition: input.definition.clone(),
        conversation_id: input.conversation_id.clone(),
        required,
        starting_savepoint: ConversationSavepoint::Empty,
    };
    let mutation = match &view.request.parameters {
        ControlParameters::AddAgent { dependencies, .. } => {
            graph.nodes.push(node);
            for parent in dependencies {
                graph.dependencies.push(DependencyEdge {
                    parent: parent.clone(),
                    child: input.activation.node_id.clone(),
                });
            }
            GraphMutation::Add {
                node_id: input.activation.node_id.clone(),
            }
        }
        ControlParameters::ReplaceFutureAgent {
            target,
            rewire_dependents,
            ..
        } => {
            graph.nodes.retain(|node| node.node_id != *target);
            graph.nodes.push(node);
            for edge in &mut graph.dependencies {
                if edge.parent == *target {
                    edge.parent = input.activation.node_id.clone();
                }
                if edge.child == *target {
                    edge.child = input.activation.node_id.clone();
                }
            }
            for condition in &mut graph.conditions {
                for node in &mut condition.nodes {
                    if *node == *target {
                        *node = input.activation.node_id.clone();
                    }
                }
            }
            GraphMutation::ReplaceFuture {
                previous: target.clone(),
                replacement: input.activation.node_id.clone(),
                rewire_dependents: rewire_dependents.clone(),
            }
        }
        _ => return Err(error("not a graph command")),
    };
    Ok(Some(TurnContractEvent::ReviseGraph {
        epoch_id: view.request.execution_epoch_id.clone(),
        previous_graph: old.snapshot_id.clone(),
        graph,
        mutation,
        admission_evidence: evidence,
    }))
}

pub(crate) fn pending_human_graph_receipt(
    canonical: &SessionExecutionStore,
    content: &ExecutionContentStore,
    request: &HumanGraphEditRequest,
    review: Option<&str>,
) -> Result<Option<HumanGraphEditPreview>> {
    if canonical.owner().session_id != request.session_id {
        return Err(error("graph request belongs to another Session"));
    }
    if canonical.turn(&request.turn_id).map_err(error)?.is_none() {
        return Ok(None);
    }
    let namespace = match canonical.existing_component_namespace(
        ExecutionComponent::ControlCommands {
            turn_id: request.turn_id.clone(),
        },
        std::path::Path::new("control-command.v1.json"),
    ) {
        Ok(namespace) => namespace,
        Err(axocoatl_session::execution_store::ExecutionStoreError::Io(failure))
            if failure.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(None)
        }
        Err(failure) => return Err(error(failure)),
    };
    let commands = ControlCommandStore::open_owned(namespace).map_err(error)?;
    let Some(receipt) = commands.receipt(&request.command_id).map_err(error)? else {
        return Ok(None);
    };
    let view = receipt.view();
    let CommandSourceRecord::Human {
        session_id,
        turn_id,
        request_evidence,
    } = &view.source
    else {
        return Err(error("graph command belongs to another source"));
    };
    if session_id != &request.session_id || turn_id != &request.turn_id {
        return Err(error("graph command belongs to another owner"));
    }
    let ActivationEvidenceContent::Guidance { text } = content
        .resolve_activation_evidence(request_evidence)
        .map_err(error)?
    else {
        return Err(error("original graph request is missing"));
    };
    let original: HumanGraphEditRequest = serde_json::from_str(text).map_err(error)?;
    if original != *request {
        return Err(error("graph command already has a different body"));
    }
    let TurnContractEvent::ReviseGraph { graph, .. } =
        graph_event(canonical, &request.turn_id, view, true)?
            .ok_or_else(|| error("saved command is not a graph edit"))?
    else {
        return Err(error("saved graph proposal differs"));
    };
    let review_digest = graph_review_digest(request, &view.request.parameters, &graph)?;
    if review.is_some_and(|digest| digest != review_digest) {
        return Err(error("saved graph review differs from this Apply"));
    }
    Ok(Some(HumanGraphEditPreview {
        request: request.clone(),
        review_digest,
        graph,
        receipt: Some(view.clone()),
    }))
}
