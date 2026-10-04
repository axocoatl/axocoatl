//! Native ordinary-turn admission through the retained Session team and existing driver.
//! All IDs, complete ingress, team choices and grants are durably retained.
use super::*;
use crate::session_dispatch::{
    AutonomousTurnDriver, SessionDispatchController, SuccessorTurn, TurnDriveOutcome,
};
use axocoatl_core::AgentConfig;
use axocoatl_session::control_authority::AuthorityGrant;
use axocoatl_session::execution_content::{
    ActivationEvidenceContent, ExecutionRequestContent, TurnAdmissionContent,
    TurnAdmissionNodeInput,
};
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::session_team::SessionTeamStore;
use axocoatl_session::turn_checks::CheckGroup;
use axocoatl_session::turn_contract::*;
use serde::{Deserialize, Serialize};

#[path = "bootstrap_native_model.rs"]
mod model;
pub(crate) use model::NativeTurnModelSelection;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeNodeEvidence {
    pub node_id: TurnNodeId,
    pub guidance: Vec<EvidenceRef>,
    pub attachments: Vec<EvidenceRef>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeFirstTurnRequest {
    pub schema_version: u32,
    /// What the removed standing-work inbox admitted this turn for. Read so
    /// those turns' history still loads; such a turn cannot start again.
    #[serde(default, rename = "standing_work", skip_serializing)]
    pub legacy_standing_work: Option<serde_json::Value>,
    /// Complete original Send identity; retained only as protected input evidence.
    /// It is never Agent guidance and grants no execution authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<serde_json::Value>,
    pub session_id: SessionId,
    pub command_id: CommandId,
    pub turn_id: LogicalTurnId,
    pub epoch_id: ExecutionEpochId,
    pub graph_snapshot_id: GraphSnapshotId,
    pub expected_team_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_definition: Option<AgentDefinitionId>,
    pub request: ExecutionRequestContent,
    /// Authenticated host input after actual user approval. Retained issuer
    /// evidence alone is not authorization to manufacture this argument.
    pub grants: Vec<AuthorityGrant>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_selections: Vec<NativeTurnModelSelection>,
    pub node_evidence: Vec<NativeNodeEvidence>,
}
impl NativeFirstTurnRequest {
    pub(super) fn source(&self) -> Result<String, DaemonError> {
        let bytes = serde_json::to_vec(self).map_err(failure)?;
        if bytes.len() > MAX_CONTRACT_ENVELOPE_BYTES
            || self.schema_version != 1
            || self.turn_id != self.request.turn_id
            || self.expected_team_revision == 0
            || self.target_definition != self.request.target_definition
        {
            return Err(failure("invalid or oversized native first-turn request"));
        }
        String::from_utf8(bytes).map_err(failure)
    }
}
fn failure(error: impl std::fmt::Display) -> DaemonError {
    DaemonError::SessionConflict(error.to_string())
}

pub(crate) enum NativeFirstTurnStart<'a> {
    Prepared(Box<PreparedNativeTurn<'a>>),
    Reattached(Box<crate::session_control_plane::SessionTurnControlPlane>),
}
/// Dropping before/during run drops the existing controller-owned driver, whose
/// interruption/settlement protocol retains unknown work and repository gates.
pub(crate) struct PreparedNativeTurn<'a> {
    driver: AutonomousTurnDriver,
    controller: SessionDispatchController,
    registry: &'a session_dispatch::SessionDispatchRegistry,
    session_id: String,
}
impl PreparedNativeTurn<'_> {
    pub(crate) fn controller(&self) -> SessionDispatchController {
        self.controller.clone()
    }
    pub(crate) async fn run(self) -> Result<TurnDriveOutcome, DaemonError> {
        let outcome = self.driver.run().await.map_err(failure)?;
        if outcome.finalized.is_some() {
            self.registry
                .release_after_turn(&self.session_id, outcome.snapshot.turn_id())?;
        }
        Ok(outcome)
    }
}
struct DefinitionSetup {
    config: AgentConfig,
    definition: DefinitionSnapshotRef,
    revision: u64,
    limits: axocoatl_session::control_authority::GrantLimits,
}
pub(super) struct AdmissionSetup {
    pub(super) content: TurnAdmissionContent,
    definitions: Vec<DefinitionSetup>,
}

impl AxocoatlDaemon {
    /// Admit one complete exact request; retries inspect the retained turn.
    pub(crate) async fn prepare_native_turn(
        &self,
        request: NativeFirstTurnRequest,
    ) -> Result<NativeFirstTurnStart<'_>, DaemonError> {
        self.require_runtime_admission()?;
        if request.legacy_standing_work.is_some() {
            return Err(failure(
                "This turn was admitted from a standing work source, which Axocoatl no longer \
                 runs; it cannot be started again",
            ));
        }
        let source = request.source()?;
        match self
            .session_dispatch_lifecycles
            .native_first_turn_existing(request.session_id.as_str(), &request.turn_id, &source)?
        {
            session_dispatch::NativeFirstTurnExisting::Registered {
                controller,
                repository,
            } => return self.finish_native_setup(controller, repository, &source),
            session_dispatch::NativeFirstTurnExisting::Retained(view) => {
                return Ok(NativeFirstTurnStart::Reattached(view))
            }
            session_dispatch::NativeFirstTurnExisting::Unstarted => {}
        }
        self.ensure_registered_native_session(request.session_id.as_str())
            .await?;
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(request.session_id.as_str())?;
        let setup = prepare_admission(
            &self.session_dispatch_lifecycles,
            &token,
            &self.data_root,
            &request,
            &source,
        )?;
        for definition in &setup.definitions {
            let captured = self
                .prepare_native_session_team_definition(
                    &token,
                    definition.config.clone(),
                    definition.definition.definition_id.clone(),
                    definition.revision,
                    definition.limits.clone(),
                )
                .await?;
            if captured.definition != definition.definition {
                return Err(failure("effective native configuration differs from the selected Session team definition"));
            }
        }
        // Re-read the actual revision/committed heads after asynchronous model
        // observation. A stale Apply or head cannot be admitted from cached data.
        let checked = prepare_admission(
            &self.session_dispatch_lifecycles,
            &token,
            &self.data_root,
            &request,
            &source,
        )?;
        if checked.content != setup.content {
            return Err(failure(
                "native first-turn preparation changed across resource observation",
            ));
        }
        let session = request.session_id.as_str();
        let pending = self
            .session_dispatch_lifecycles
            .native_pending_token(session)?;
        let owner = if let Some(pending) = &pending {
            let owner = self.pending_session_repository_owner(pending).await?;
            self.require_runtime_admission()?;
            self.validate_repository_daemon_binding(&owner)?;
            Some(owner)
        } else {
            if self
                .session_dispatch_lifecycles
                .native_reacquisition_needed(session)?
            {
                self.reacquire_session_dispatch_repository(session).await?;
            }
            None
        };
        let checked = prepare_admission(
            &self.session_dispatch_lifecycles,
            &token,
            &self.data_root,
            &request,
            &source,
        )?;
        if checked.content != setup.content {
            return Err(failure("native turn changed during repository admission"));
        }
        let expected_team_revision = request.expected_team_revision;
        let expected_graph = setup.content.graph.clone();
        let expected_turn = request.turn_id.clone();
        let target = request.target_definition.clone();
        let spec = SuccessorTurn {
            command_id: request.command_id,
            turn_id: request.turn_id,
            epoch_id: request.epoch_id,
            graph: setup.content.graph,
            request: request.request,
        };
        let validate =
            |canonical: &axocoatl_session::execution_store::SessionExecutionStore,
             content: &axocoatl_session::execution_content::ExecutionContentStore,
             memory: &axocoatl_memory::activation_state::ActivationStateStore| {
                verify_selected_team(
                    canonical,
                    content,
                    memory,
                    expected_team_revision,
                    &expected_turn,
                    &expected_graph,
                    target.as_ref(),
                )
            };
        let (controller, repository) = if let Some(pending) = pending {
            self.session_dispatch_lifecycles.begin_first_turn_checked(
                &pending,
                owner.expect("pending owner"),
                spec,
                validate,
            )?
        } else {
            self.session_dispatch_lifecycles
                .begin_native_successor_checked(session, spec, validate)?
        };
        self.finish_native_setup(controller, repository, &source)
    }
    fn finish_native_setup<'a>(
        &'a self,
        controller: SessionDispatchController,
        repository: EvidenceRef,
        source: &str,
    ) -> Result<NativeFirstTurnStart<'a>, DaemonError> {
        let factory = self.native_session_activation_factory(&controller)?;
        finish_owned_setup(
            &self.session_dispatch_lifecycles,
            controller,
            repository,
            source,
            self.stream_bus.clone(),
            factory,
        )
    }
}

pub(super) fn finish_owned_setup<'a>(
    registry: &'a session_dispatch::SessionDispatchRegistry,
    controller: SessionDispatchController,
    repository: EvidenceRef,
    source: &str,
    bus: crate::stream::StreamBus,
    factory: Arc<dyn crate::session_dispatch::AutonomousActivationFactory>,
) -> Result<NativeFirstTurnStart<'a>, DaemonError> {
    let session_id = controller
        .snapshot()
        .map_err(failure)?
        .owner()
        .session_id
        .as_str()
        .to_owned();
    controller
        .authorize_required_checks(&repository)
        .map_err(failure)?;
    match controller
        .prepare_native_host_driver(source, repository, bus, factory)
        .map_err(failure)?
    {
        Some(driver) => Ok(NativeFirstTurnStart::Prepared(Box::new(
            PreparedNativeTurn {
                driver,
                controller,
                registry,
                session_id,
            },
        ))),
        None => Ok(NativeFirstTurnStart::Reattached(Box::new(
            controller.control_plane().map_err(failure)?,
        ))),
    }
}

pub(super) fn prepare_admission(
    registry: &session_dispatch::SessionDispatchRegistry,
    token: &session_dispatch::SessionTeamToken,
    data_root: &SecureDir,
    request: &NativeFirstTurnRequest,
    source: &str,
) -> Result<AdmissionSetup, DaemonError> {
    if request.source()? != source {
        return Err(failure(
            "retained source does not match the complete native request",
        ));
    }
    registry.with_session_team_stores(token, |canonical, content, memory| {
        canonical.verify_data_root(data_root).map_err(failure)?;
        if canonical.owner().session_id != request.session_id {
            return Err(failure("native request belongs to another Session"));
        }
        if content
            .turn_admission(canonical, &request.turn_id)
            .map_err(failure)?
            .is_some_and(|(_, admission)| admission.source != source)
        {
            return Err(failure(
                "native turn ID already has a different complete request",
            ));
        }
        let team = SessionTeamStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::SessionTeam)
                .map_err(failure)?,
            canonical,
            content,
            None,
        )
        .map_err(failure)?;
        let accepted = canonical.turn(&request.turn_id).map_err(failure)?.is_some();
        let stored;
        let revision = if accepted {
            stored = team.get(request.expected_team_revision).map_err(failure)?;
            stored.as_deref()
        } else {
            team.current().map_err(failure)?
        }
        .ok_or_else(|| failure("Session has no selected applied team revision"))?;
        if revision.configuration_revision != request.expected_team_revision {
            return Err(failure("Session team changed before Begin"));
        }
        let selected_slots = selected_slots(revision, request.target_definition.as_ref())?;
        if request.grants.len() != selected_slots.len()
            || request.node_evidence.len() != selected_slots.len()
        {
            return Err(failure(
                "explicit grants and node evidence must cover every exact team slot",
            ));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(failure)?
            .as_millis();
        let mut grants = HashMap::new();
        let mut grant_ids = HashSet::new();
        for grant in &request.grants {
            grant.validate().map_err(failure)?;
            if grant.expires_at_ms as u128 <= now {
                return Err(failure(
                    "approved Session grant expired before Begin; review the team budget",
                ));
            }
            if grant.revision != 1
                || grants.insert(grant.holder.clone(), grant.clone()).is_some()
                || !grant_ids.insert(&grant.id)
            {
                return Err(failure(
                    "first-turn grants are duplicated or not initial explicit revisions",
                ));
            }
            // The host authenticated this policy; its original issuer bytes must
            // also exist in this exact Session's protected content store.
            if !matches!(
                content
                    .resolve_activation_evidence(&grant.issuer_evidence)
                    .map_err(failure)?,
                ActivationEvidenceContent::Guidance { .. }
            ) {
                return Err(failure("grant issuer must resolve retained host guidance"));
            }
        }
        let mut evidence = HashMap::new();
        for node in &request.node_evidence {
            if evidence.insert(node.node_id.clone(), node).is_some() {
                return Err(failure("duplicate initial node evidence"));
            }
            for reference in &node.guidance {
                if !matches!(
                    content
                        .resolve_activation_evidence(reference)
                        .map_err(failure)?,
                    ActivationEvidenceContent::Guidance { .. }
                ) {
                    return Err(failure(
                        "caller guidance cannot substitute or duplicate the retained Begin request",
                    ));
                }
            }
            for reference in &node.attachments {
                if !matches!(
                    content
                        .resolve_activation_evidence(reference)
                        .map_err(failure)?,
                    ActivationEvidenceContent::Attachment { .. }
                        | ActivationEvidenceContent::BinaryAttachment { .. }
                ) {
                    return Err(failure(
                        "node attachment has the wrong retained evidence role",
                    ));
                }
            }
        }
        if request.model_selections.iter().any(|selection| {
            !selected_slots
                .iter()
                .any(|slot| slot.node_id == selection.node_id)
        }) {
            return Err(failure(
                "Model selection names work outside the selected Team",
            ));
        }
        let mut definitions = Vec::new();
        let mut points = Vec::new();
        let mut names = HashMap::new();
        for slot in &selected_slots {
            let effective_definition = model::selected_definition(request, slot, content)?;
            let ActivationEvidenceContent::Definition {
                profile: approved_profile,
                ..
            } = &content
                .resolve_activation_evidence(&slot.definition.snapshot)
                .map_err(failure)?
            else {
                return Err(failure("Applied Agent profile is missing"));
            };
            let ActivationEvidenceContent::Definition {
                definition_id,
                revision: definition_revision,
                profile,
                configuration,
            } = &content
                .resolve_activation_evidence(&effective_definition.snapshot)
                .map_err(failure)?
            else {
                return Err(failure("team definition has the wrong retained role"));
            };
            let config: AgentConfig = serde_json::from_str(configuration).map_err(failure)?;
            names.insert(slot.node_id.clone(), config.name.clone());
            if config.role == axocoatl_core::AgentRole::Worker {
                return Err(failure(
                    "A Worker must run through its Coordinator's exact child admission",
                ));
            }
            if definition_id != &effective_definition.definition_id
                || config.id.0 != slot.conversation_id.as_str()
                || serde_json::to_string(&config).map_err(failure)? != *configuration
                || config.provider != profile.provider
                || config.model != profile.model
                || config.tools != profile.tools
                || config.writes != profile.write_scope
                || profile.definition != definition_id.as_str()
                || profile.isolation != "in-process"
            {
                return Err(failure(
                    "team definition is not the exact effective native slot configuration",
                ));
            }
            let grant = grants
                .get(&slot.node_id)
                .ok_or_else(|| failure("team node lacks explicit grant"))?;
            let grant_reference = slot
                .grant
                .as_ref()
                .ok_or_else(|| failure("team slot has no explicit approved execution grant"))?;
            let ActivationEvidenceContent::Grant { policy: approved } = &content
                .resolve_activation_evidence(grant_reference)
                .map_err(failure)?
            else {
                return Err(failure(
                    "team slot approved grant has the wrong evidence role",
                ));
            };
            if approved != grant {
                return Err(failure(
                    "native request grant differs from the applied approved team policy",
                ));
            }
            let ActivationEvidenceContent::Budget { limits } = &content
                .resolve_activation_evidence(&slot.budget)
                .map_err(failure)?
            else {
                return Err(failure("team slot budget has the wrong retained role"));
            };
            if &grant.limits != limits
                || !grant.profiles.contains(approved_profile)
                || !evidence.contains_key(&slot.node_id)
            {
                return Err(failure(
                    "explicit grant/evidence differs from the applied team budget/profile",
                ));
            }
            crate::session_dispatch::NativeDefinitionPreparation::new(
                config.clone(),
                definition_id.clone(),
                *definition_revision,
                limits.clone(),
            )
            .map_err(failure)?;
            let effective_grant = model::grant_for_model(
                grant,
                approved_profile,
                profile,
                request
                    .model_selections
                    .iter()
                    .find(|selection| selection.node_id == slot.node_id),
            );
            grants.insert(slot.node_id.clone(), effective_grant);
            definitions.push(DefinitionSetup {
                config,
                definition: effective_definition,
                revision: *definition_revision,
                limits: limits.clone(),
            });
            let point = memory
                .committed_reference(&slot.conversation_id)
                .map_err(failure)?
                .map_or(ConversationSavepoint::Empty, |checkpoint| {
                    ConversationSavepoint::Checkpoint {
                        checkpoint: Box::new(checkpoint),
                    }
                });
            points.push((slot.slot_id.clone(), point));
        }
        let mut graph = selected_graph(
            revision,
            canonical.owner(),
            request.graph_snapshot_id.clone(),
            &points,
            request.target_definition.as_ref(),
        )?;
        for node in &mut graph.nodes {
            if let Some(selection) = request
                .model_selections
                .iter()
                .find(|selection| selection.node_id == node.node_id)
            {
                node.definition = selection.definition.clone();
            }
        }
        // Match the controller's actual graph-bound grant scope before Begin,
        // including targeted Send, so an invalid policy cannot strand a turn.
        for grant in grants.values() {
            let mut descendants = HashSet::new();
            let mut frontier = vec![grant.holder.clone()];
            while let Some(parent) = frontier.pop() {
                for edge in graph
                    .dependencies
                    .iter()
                    .filter(|edge| edge.parent == parent)
                {
                    if descendants.insert(edge.child.clone()) {
                        frontier.push(edge.child.clone());
                    }
                }
            }
            if grant
                .descendants
                .iter()
                .any(|node| !descendants.contains(node))
            {
                return Err(failure(
                    "approved grant names work outside this selected turn; review its team scope",
                ));
            }
        }
        memory
            .validate_starting_savepoints(&graph)
            .map_err(failure)?;
        // Every slot's grant carries the same approved Apply.
        let required_checks = session_team::approved_required_checks(content, selected_slots[0])?;
        let review = session_team::approved_review(content, selected_slots[0])?;
        drop(team);
        if !required_checks.is_empty() {
            // The authority charges them to the first required Agent whose
            // own profile may use bash; without one the turn could never run
            // them, and with too small a limit it could not pay for them.
            let Some(payer) = graph
                .nodes
                .iter()
                .filter(|node| node.required)
                .find(|node| {
                    axocoatl_session::control_authority::pays_with_own_shell(
                        &grants[&node.node_id],
                        node,
                    )
                })
            else {
                return Err(failure(
                    "This Session team has required checks, and none of the Agents this turn \
                     runs may use bash to pay for them. Send to the whole team, or to an Agent \
                     with the bash tool",
                ));
            };
            let minimum =
                axocoatl_session::turn_checks::payer_minimum_invocations(required_checks.len());
            let limit = grants[&payer.node_id].limits.invocations;
            if limit < minimum {
                let name = names
                    .get(&payer.node_id)
                    .filter(|name| !name.is_empty())
                    .cloned()
                    .unwrap_or_else(|| payer.node_id.as_str().to_owned());
                return Err(failure(format!(
                    "{name} runs this Session team's required checks on its budget, and its \
                     invocation limit of {limit} is too small for them: it needs at least \
                     {minimum}. Raise its invocation limit in Team and budget, or send to the \
                     whole team"
                )));
            }
            inject_checks(content, &mut graph, &required_checks)?;
            graph.validate(&request.session_id).map_err(failure)?;
        }
        if let Some(review) = &review {
            let grant = inject_review(content, &mut graph, review, request, &grants)?;
            graph.validate(&request.session_id).map_err(failure)?;
            grants.insert(grant.holder.clone(), grant);
        }
        let retained_request = content
            .retain_request(request.request.clone())
            .map_err(failure)?;
        let mut nodes = Vec::new();
        for slot in &graph.nodes {
            let mut grant = grants[&slot.node_id].clone();
            if let Some(approved) = session_team::approved_coordinator_policy(content, &grant)? {
                use axocoatl_session::control_authority::{
                    DelegatedGraphLimits, DelegatedReplayPolicy, DelegationPolicy,
                };
                if graph.nodes.len() > approved.max_nodes as usize
                    || graph.dependencies.len() > approved.max_edges as usize
                {
                    return Err(failure(
                        "Selected turn exceeds the Coordinator's explicitly approved graph limits",
                    ));
                }
                let approved_graph = content
                    .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                        text: serde_json::to_string(&graph).map_err(failure)?,
                    })
                    .map_err(failure)?
                    .reference()
                    .clone();
                let resource_policy = content
                    .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                        text: serde_json::to_string(&approved.resource).map_err(failure)?,
                    })
                    .map_err(failure)?
                    .reference()
                    .clone();
                grant.delegation = Some(Box::new(DelegationPolicy {
                    schema_version: 1,
                    scope: DelegationScope {
                        session_id: request.session_id.clone(),
                        turn_id: request.turn_id.clone(),
                        task: retained_request.reference().clone(),
                        approved_graph,
                    },
                    operations: approved
                        .operations
                        .iter()
                        .map(|operation| DelegatedOperationPermission {
                            operation: *operation,
                            targets: DelegatedTargetScope::Subtree {
                                root: slot.node_id.clone(),
                                include_future_descendants: true,
                            },
                        })
                        .collect(),
                    templates: approved
                        .workers
                        .iter()
                        .map(|worker| worker.definition.clone())
                        .collect(),
                    resource_policy,
                    graph_limits: DelegatedGraphLimits {
                        max_nodes: approved.max_nodes,
                        max_edges: approved.max_edges,
                    },
                    required_conditions: graph
                        .conditions
                        .iter()
                        .filter(|condition| {
                            condition.nodes.iter().all(|node| {
                                node == &grant.holder || grant.descendants.contains(node)
                            })
                        })
                        .cloned()
                        .collect(),
                    completion_criteria: vec![],
                    machine_blockers: vec![],
                    replay_policy: DelegatedReplayPolicy::RequireProvedEffectSafety,
                }));
                grant.validate().map_err(failure)?;
            }
            // The host's own reviewer has no caller-supplied evidence.
            let (supplied_guidance, supplied_attachments) = evidence
                .get(&slot.node_id)
                .map(|supplied| (supplied.guidance.clone(), supplied.attachments.clone()))
                .unwrap_or_default();
            let retained_grant = content
                .retain_activation_evidence(ActivationEvidenceContent::Grant {
                    policy: grant.clone(),
                })
                .map_err(failure)?;
            let budget = content
                .retain_activation_evidence(ActivationEvidenceContent::Budget {
                    limits: grant.limits.clone(),
                })
                .map_err(failure)?;
            let mut guidance = Vec::with_capacity(supplied_guidance.len() + 1);
            guidance.push(retained_request.reference().clone());
            guidance.extend(supplied_guidance);
            nodes.push(TurnAdmissionNodeInput {
                node_id: slot.node_id.clone(),
                guidance,
                attachments: supplied_attachments,
                budget: budget.reference().clone(),
                grant: GrantSnapshotRef {
                    grant_id: GrantId::new(&grant.id).map_err(failure)?,
                    revision: grant.revision,
                    evidence: retained_grant.reference().clone(),
                },
            });
        }
        let admission = TurnAdmissionContent {
            schema_version: 1,
            command_id: request.command_id.clone(),
            turn_id: request.turn_id.clone(),
            epoch_id: request.epoch_id.clone(),
            source: source.to_owned(),
            graph,
            request: retained_request.reference().clone(),
            nodes,
        };
        content
            .retain_turn_admission(canonical, admission.clone())
            .map_err(failure)?;
        Ok(AdmissionSetup {
            content: admission,
            definitions,
        })
    })
}

/// Add the required checks to an admitted graph: a repository capture, each
/// command and a capture, all over the graph's required nodes, then the
/// readiness review. Retries retain the same definitions and criterion.
fn inject_checks(
    content: &mut axocoatl_session::execution_content::ExecutionContentStore,
    graph: &mut TurnGraphSnapshot,
    checks: &[Vec<String>],
) -> Result<(), DaemonError> {
    let group = CheckGroup::required();
    let readiness = axocoatl_session::turn_checks::readiness_text(checks);
    let nodes: Vec<_> = graph
        .nodes
        .iter()
        .filter(|node| node.required)
        .map(|node| node.node_id.clone())
        .collect();
    let definitions = axocoatl_session::turn_checks::check_definitions(checks).map_err(failure)?;
    if definitions.is_empty() {
        return Ok(());
    }
    for (index, definition) in definitions.iter().enumerate() {
        let reference = content
            .retain_repository_check_definition(definition.clone())
            .map_err(failure)?
            .reference()
            .clone();
        graph.conditions.push(CompletionCondition {
            condition_id: ConditionId::new(group.condition_id(index)).map_err(failure)?,
            kind: ConditionKind::RepositoryCheck {
                definition: reference,
            },
            nodes: nodes.clone(),
        });
    }
    let criterion = content
        .retain_activation_evidence(ActivationEvidenceContent::Guidance { text: readiness })
        .map_err(failure)?
        .reference()
        .clone();
    graph.conditions.push(CompletionCondition {
        condition_id: ConditionId::new(group.ready_id()).map_err(failure)?,
        kind: ConditionKind::Review { criterion },
        nodes,
    });
    Ok(())
}

/// Add the required reviewer to an admitted graph: one optional node in a
/// fresh conversation that no Agent depends on, and the review condition over
/// the graph's required nodes. Returns the reviewer's grant: the approved
/// reviewer budget for this turn, issued by the same Apply and expiring with
/// the earliest grant of the turn. Retries derive the same node and grant.
fn inject_review(
    content: &mut axocoatl_session::execution_content::ExecutionContentStore,
    graph: &mut TurnGraphSnapshot,
    review: &session_team::ApprovedReview,
    request: &NativeFirstTurnRequest,
    grants: &HashMap<TurnNodeId, AuthorityGrant>,
) -> Result<AuthorityGrant, DaemonError> {
    use axocoatl_session::turn_review::{ReviewCriterion, REVIEW_CONDITION_ID, REVIEW_NODE_ID};
    use sha2::Digest;
    let ActivationEvidenceContent::Definition {
        profile,
        configuration,
        ..
    } = &content
        .resolve_activation_evidence(&review.definition.snapshot)
        .map_err(failure)?
    else {
        return Err(failure("The approved reviewer definition is missing"));
    };
    let profile = profile.clone();
    let config: AgentConfig = serde_json::from_str(configuration).map_err(failure)?;
    if config.role != axocoatl_core::AgentRole::Worker
        || !crate::session_dispatch::changing_tools(&profile.tools, profile.write_scope.as_deref())
            .is_empty()
    {
        return Err(failure(
            "The approved reviewer is not a read-only Worker; apply the team again",
        ));
    }
    let nodes: Vec<_> = graph
        .nodes
        .iter()
        .filter(|node| node.required)
        .map(|node| node.node_id.clone())
        .collect();
    let payer = nodes
        .first()
        .and_then(|node| grants.get(node))
        .ok_or_else(|| failure("A required review needs a required Agent to review"))?;
    let digest = format!(
        "{:x}",
        sha2::Sha256::digest(
            serde_json::to_vec(&("required-review-v1", &request.session_id, &request.turn_id))
                .map_err(failure)?
        )
    );
    let node_id = TurnNodeId::new(REVIEW_NODE_ID).map_err(failure)?;
    graph.nodes.push(GraphNode {
        node_id: node_id.clone(),
        slot_id: SessionTeamSlotId::new(REVIEW_NODE_ID).map_err(failure)?,
        definition: review.definition.clone(),
        conversation_id: NodeConversationId::new(format!("review-conversation-{}", &digest[..40]))
            .map_err(failure)?,
        starting_savepoint: ConversationSavepoint::Empty,
        required: false,
    });
    let criterion = content
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: serde_json::to_string(&ReviewCriterion::new(
                &review.template_id,
                review.max_rounds,
            ))
            .map_err(failure)?,
        })
        .map_err(failure)?
        .reference()
        .clone();
    graph.conditions.push(CompletionCondition {
        condition_id: ConditionId::new(REVIEW_CONDITION_ID).map_err(failure)?,
        kind: ConditionKind::Review { criterion },
        nodes,
    });
    let grant = AuthorityGrant {
        id: format!("review-grant-{}", &digest[..40]),
        revision: 1,
        issuer_evidence: payer.issuer_evidence.clone(),
        holder: node_id,
        descendants: vec![],
        allow_stop_descendants: false,
        delegation: None,
        profiles: vec![profile],
        conditions: vec![],
        limits: review.limits.clone(),
        expires_at_ms: grants
            .values()
            .map(|grant| grant.expires_at_ms)
            .min()
            .unwrap_or(payer.expires_at_ms),
    };
    grant.validate().map_err(failure)?;
    Ok(grant)
}

pub(super) fn verify_selected_team(
    canonical: &axocoatl_session::execution_store::SessionExecutionStore,
    content: &axocoatl_session::execution_content::ExecutionContentStore,
    memory: &axocoatl_memory::activation_state::ActivationStateStore,
    revision: u64,
    turn: &LogicalTurnId,
    graph: &TurnGraphSnapshot,
    target: Option<&AgentDefinitionId>,
) -> Result<(), DaemonError> {
    let team = SessionTeamStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::SessionTeam)
            .map_err(failure)?,
        canonical,
        content,
        None,
    )
    .map_err(failure)?;
    let stored;
    let selected = if canonical.turn(turn).map_err(failure)?.is_some() {
        stored = team.get(revision).map_err(failure)?;
        stored.as_deref()
    } else {
        team.current().map_err(failure)?
    }
    .ok_or_else(|| failure("selected Session team is unavailable"))?;
    if selected.configuration_revision != revision {
        return Err(failure("Session team changed before exact Begin"));
    }
    // The reviewer admission adds is no team slot.
    let reviewer = axocoatl_session::turn_review::review_node(graph).map(|node| &node.node_id);
    let points = graph
        .nodes
        .iter()
        .filter(|node| Some(&node.node_id) != reviewer)
        .map(|node| (node.slot_id.clone(), node.starting_savepoint.clone()))
        .collect::<Vec<_>>();
    let mut expected = selected_graph(
        selected,
        canonical.owner(),
        graph.snapshot_id.clone(),
        &points,
        target,
    )?;
    if let Some((_, admission)) = content.turn_admission(canonical, turn).map_err(failure)? {
        let request: NativeFirstTurnRequest =
            serde_json::from_str(&admission.source).map_err(failure)?;
        for node in &mut expected.nodes {
            let slot = selected
                .graph
                .slots
                .iter()
                .find(|slot| slot.node_id == node.node_id)
                .ok_or_else(|| failure("Selected model lost its Team slot"))?;
            node.definition = model::selected_definition(&request, slot, content)?;
        }
    }
    // The team graph carries no check conditions or reviewer; admission adds
    // them.
    let mut admitted = graph.clone();
    if let Some((group, _)) = axocoatl_session::turn_checks::group_of(graph) {
        admitted
            .conditions
            .retain(|condition| !group.contains(&condition.condition_id));
    }
    if let Some(reviewer) = axocoatl_session::turn_review::review_node(graph) {
        admitted
            .nodes
            .retain(|node| node.node_id != reviewer.node_id);
        admitted.conditions.retain(|condition| {
            condition.condition_id.as_str() != axocoatl_session::turn_review::REVIEW_CONDITION_ID
        });
    }
    if expected != admitted {
        return Err(failure(
            "first-turn graph differs from the actual applied team revision",
        ));
    }
    memory.validate_starting_savepoints(graph).map_err(failure)
}

fn selected_slots<'a>(
    revision: &'a axocoatl_session::session_team::SessionTeamRevision,
    target: Option<&AgentDefinitionId>,
) -> Result<Vec<&'a axocoatl_session::session_team::SessionTeamSlot>, DaemonError> {
    let slots = revision
        .graph
        .slots
        .iter()
        .filter(|slot| target.is_none_or(|id| &slot.definition.definition_id == id))
        .collect::<Vec<_>>();
    if target.is_some() && slots.len() != 1 {
        return Err(failure(
            "target Agent must identify exactly one applied team slot",
        ));
    }
    Ok(slots)
}
fn selected_graph(
    revision: &axocoatl_session::session_team::SessionTeamRevision,
    owner: &axocoatl_session::execution_store::ExecutionStoreOwner,
    id: GraphSnapshotId,
    points: &[(SessionTeamSlotId, ConversationSavepoint)],
    target: Option<&AgentDefinitionId>,
) -> Result<TurnGraphSnapshot, DaemonError> {
    if target.is_none() {
        return revision.initial_graph(owner, id, points).map_err(failure);
    }
    let slots = selected_slots(revision, target)?;
    let slot = slots[0];
    let point = points
        .iter()
        .find(|(id, _)| id == &slot.slot_id)
        .ok_or_else(|| failure("target Agent lacks its committed savepoint"))?;
    let graph = TurnGraphSnapshot {
        snapshot_id: id,
        revision: 1,
        nodes: vec![GraphNode {
            node_id: slot.node_id.clone(),
            slot_id: slot.slot_id.clone(),
            definition: slot.definition.clone(),
            conversation_id: slot.conversation_id.clone(),
            required: true,
            starting_savepoint: point.1.clone(),
        }],
        dependencies: Vec::new(),
        conditions: Vec::new(),
    };
    graph.validate(&owner.session_id).map_err(failure)?;
    Ok(graph)
}
