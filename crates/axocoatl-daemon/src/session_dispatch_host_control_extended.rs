//! Human translation for existing canonical revision, continuation and Finish.
//! Partial finalization is explicitly human-only and retains its complete review.
use super::*;
use crate::session_control_plane::SessionTurnControlPlane;
use axocoatl_session::control_command::FinishMode;
use axocoatl_session::turn_checks::{group_of, CheckGroup};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HumanContinuationChoice {
    pub activation: ActivationRef,
    pub state: String,
    pub capability: ControlPlaneCapability,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HumanCheckChoice {
    pub condition_id: ConditionId,
    pub required_conditions: Vec<ConditionId>,
    pub capability: ControlPlaneCapability,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HumanTurnControls {
    pub execution_epoch_id: ExecutionEpochId,
    pub continue_turn: ControlPlaneCapability,
    pub finish: ControlPlaneCapability,
    pub continuation_choices: Vec<HumanContinuationChoice>,
    pub check_choices: Vec<HumanCheckChoice>,
    pub partial_finish: HumanPartialFinishReview,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HumanPartialFinishReview {
    pub capability: ControlPlaneCapability,
    pub available_sinks: Vec<ActivationRef>,
    pub review: HumanPartialFinishSelection,
}

fn capability(result: Result<()>) -> ControlPlaneCapability {
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
}

impl DispatchState {
    fn check_continue_conditions(&self, selected: &[ConditionId]) -> Result<Vec<ConditionId>> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        continue_conditions(selected, snapshot.contract().graph().and_then(group_of))
    }
    fn human_successor_input(
        &self,
        request: &HumanControlActionRequest,
        previous: &ContractActivation,
        epoch: &ExecutionEpochId,
    ) -> Result<ActivationInputManifest> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let bytes = serde_json::to_vec(&(
            "human-generation-v1",
            snapshot.journal_id(),
            request,
            &previous.activation,
        ))
        .map_err(error)?;
        let digest = format!("{:x}", Sha256::digest(bytes));
        let mut input = previous.input.clone();
        input.manifest_id = InputManifestId::new(format!("input-{digest}")).map_err(error)?;
        input.activation.activation_id =
            ActivationId::new(format!("activation-{digest}")).map_err(error)?;
        input.activation.execution_epoch_id = epoch.clone();
        input.activation.generation = previous
            .activation
            .generation
            .checked_add(1)
            .ok_or_else(|| error("activation generation overflow"))?;
        Ok(input)
    }

    pub(super) fn extended_human_parameters(
        &self,
        request: &HumanControlActionRequest,
        request_evidence: &EvidenceRef,
        instruction: Option<&EvidenceRef>,
        preview: bool,
    ) -> Result<ControlParameters> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let contract = snapshot.contract();
        let graph = contract
            .graph()
            .ok_or_else(|| error("turn graph is unavailable"))?;
        match request.action {
            HumanControlAction::Revise => {
                let target = request.required_activation()?;
                let previous = contract
                    .activations()
                    .iter()
                    .rev()
                    .find(|item| item.activation == *target && item.output.is_some())
                    .ok_or_else(|| error("Revise requires a recorded accepted result"))?;
                if previous
                    .input
                    .guidance
                    .len()
                    .saturating_add(previous.input.attachments.len())
                    .saturating_add(previous.input.parents.len())
                    >= MAX_INPUT_REFERENCES
                {
                    return Err(error("revision has no remaining input-reference capacity"));
                }
                let continuation_epoch =
                    if contract.state() == Some(LogicalTurnState::NeedsAttention) {
                        let bytes = serde_json::to_vec(&(
                            "human-revision-continuation-v1",
                            snapshot.journal_id(),
                            request,
                        ))
                        .map_err(error)?;
                        Some(
                            ExecutionEpochId::new(format!("epoch-{:x}", Sha256::digest(bytes)))
                                .map_err(error)?,
                        )
                    } else {
                        None
                    };
                let mut input = self.human_successor_input(
                    request,
                    previous,
                    continuation_epoch
                        .as_ref()
                        .unwrap_or(&request.execution_epoch_id),
                )?;
                let instruction = if preview {
                    // Read-only assessment uses actual retained request/context.
                    // It creates no synthetic retained evidence or control receipt.
                    snapshot
                        .request_ref()
                        .ok_or_else(|| error("turn request is unavailable"))?
                } else {
                    instruction.ok_or_else(|| error("revision instruction is not retained"))?
                };
                if !input.guidance.contains(instruction) {
                    input.guidance.push(instruction.clone());
                }
                if !preview {
                    for attachment in crate::session_dispatch::human_context::attachment_references(
                        &self.content,
                        request,
                        instruction,
                    )? {
                        if !input.attachments.contains(&attachment) {
                            input.attachments.push(attachment);
                        }
                    }
                    if input
                        .guidance
                        .len()
                        .saturating_add(input.attachments.len())
                        .saturating_add(input.parents.len())
                        > MAX_INPUT_REFERENCES
                    {
                        return Err(error("Revision context exceeds input capacity"));
                    }
                }
                input.revision_context = if preview || request.include_previous_output {
                    Some(RevisionContext {
                        activation: target.clone(),
                        output: previous
                            .output
                            .clone()
                            .ok_or_else(|| error("accepted revision output is unavailable"))?,
                    })
                } else {
                    None
                };
                let invalidate = self.human_revision_invalidation(target)?;
                if let Some(epoch_id) = continuation_epoch {
                    let mut affected = HashSet::from([target.node_id.clone()]);
                    loop {
                        let before = affected.len();
                        for edge in &graph.dependencies {
                            if affected.contains(&edge.parent) {
                                affected.insert(edge.child.clone());
                            }
                        }
                        if before == affected.len() {
                            break;
                        }
                    }
                    let mut selections = Vec::with_capacity(graph.nodes.len());
                    for node in &graph.nodes {
                        if node.node_id == target.node_id {
                            selections.push(ContinuationSelection::Revise {
                                previous: target.clone(),
                                input: Box::new(input.clone()),
                                invalidated_descendants: invalidate.clone(),
                                evidence: instruction.clone(),
                            });
                        } else if affected.contains(&node.node_id) {
                            selections.push(ContinuationSelection::AwaitDependencies {
                                node_id: node.node_id.clone(),
                            });
                        } else {
                            match contract
                                .activations()
                                .iter()
                                .rev()
                                .find(|item| item.activation.node_id == node.node_id)
                            {
                                Some(item) if item.state == ActivationState::Accepted => selections
                                    .push(ContinuationSelection::RetainAccepted {
                                        activation: item.activation.clone(),
                                    }),
                                Some(item) => {
                                    selections.push(ContinuationSelection::LeaveBlocked {
                                        activation: item.activation.clone(),
                                        blocker: request_evidence.clone(),
                                    })
                                }
                                None => selections.push(
                                    ContinuationSelection::LeaveUnmaterializedBlocked {
                                        node_id: node.node_id.clone(),
                                        blocker: request_evidence.clone(),
                                    },
                                ),
                            }
                        }
                    }
                    return Ok(ControlParameters::ContinueTurn {
                        plan: ContinuationPlan {
                            source_epoch_id: request.execution_epoch_id.clone(),
                            epoch_id,
                            selections,
                            // These are the same already approved conditions that
                            // a revision in a live epoch must satisfy again.
                            condition_runs: graph
                                .conditions
                                .iter()
                                .map(|condition| condition.condition_id.clone())
                                .collect(),
                        },
                        replay_decisions: vec![],
                    });
                }
                Ok(ControlParameters::ReviseActivation {
                    activation: target.clone(),
                    input: Box::new(input),
                    instruction: instruction.clone(),
                    invalidate,
                })
            }
            HumanControlAction::Continue => {
                let selected = request
                    .continuation
                    .as_ref()
                    .ok_or_else(|| error("Continue requires explicit work/check selections"))?;
                let bytes =
                    serde_json::to_vec(&("human-continuation-v1", snapshot.journal_id(), request))
                        .map_err(error)?;
                let epoch = ExecutionEpochId::new(format!("epoch-{:x}", Sha256::digest(bytes)))
                    .map_err(error)?;
                let mut seen = HashSet::new();
                let mut selections = Vec::with_capacity(graph.nodes.len());
                for node in &graph.nodes {
                    let previous = contract
                        .activations()
                        .iter()
                        .rev()
                        .find(|item| item.activation.node_id == node.node_id);
                    let restart = selected
                        .restart
                        .iter()
                        .find(|item| item.node_id == node.node_id);
                    match (previous, restart) {
                        (Some(previous), Some(exact)) => {
                            if previous.activation != *exact {
                                return Err(error("Continue selects an obsolete generation"));
                            }
                            seen.insert(&exact.node_id);
                            let mut input =
                                self.human_successor_input(request, previous, &epoch)?;
                            if previous.state == ActivationState::Superseded {
                                input.revision_context = None;
                                input.parents = graph
                                    .dependencies
                                    .iter()
                                    .filter(|edge| edge.child == node.node_id)
                                    .map(|edge| {
                                        let parent = contract
                                            .activations()
                                            .iter()
                                            .rev()
                                            .find(|item| item.activation.node_id == edge.parent)
                                            .filter(|item| item.state == ActivationState::Accepted)
                                            .ok_or_else(|| {
                                                error("rebased continuation parent is not accepted")
                                            })?;
                                        Ok(AcceptedParentInput {
                                            activation: parent.activation.clone(),
                                            checkpoint: parent.checkpoint.clone().ok_or_else(
                                                || error("parent checkpoint is unavailable"),
                                            )?,
                                            output: parent.output.clone().ok_or_else(|| {
                                                error("parent output is unavailable")
                                            })?,
                                        })
                                    })
                                    .collect::<Result<Vec<_>>>()?;
                                selections.push(ContinuationSelection::Rebase {
                                    previous: exact.clone(),
                                    input: Box::new(input),
                                });
                            } else {
                                selections.push(ContinuationSelection::Retry {
                                    previous: exact.clone(),
                                    input: Box::new(input),
                                });
                            }
                        }
                        (Some(previous), None) if previous.state == ActivationState::Accepted => {
                            selections.push(ContinuationSelection::RetainAccepted {
                                activation: previous.activation.clone(),
                            });
                        }
                        (Some(previous), None) => {
                            selections.push(ContinuationSelection::LeaveBlocked {
                                activation: previous.activation.clone(),
                                blocker: request_evidence.clone(),
                            })
                        }
                        (None, Some(_)) => {
                            return Err(error(
                                "Continue cannot invent an input for unmaterialized work",
                            ))
                        }
                        (None, None) => {
                            selections.push(ContinuationSelection::LeaveUnmaterializedBlocked {
                                node_id: node.node_id.clone(),
                                blocker: request_evidence.clone(),
                            })
                        }
                    }
                }
                if seen.len() != selected.restart.len() {
                    return Err(error("Continue selects an undeclared node"));
                }
                Ok(ControlParameters::ContinueTurn {
                    plan: ContinuationPlan {
                        source_epoch_id: request.execution_epoch_id.clone(),
                        epoch_id: epoch,
                        selections,
                        condition_runs: self.check_continue_conditions(&selected.checks)?,
                    },
                    replay_decisions: vec![],
                })
            }
            HumanControlAction::Finish => {
                let mode = if let Some(selection) = &request.partial_finish {
                    let offered = partial_finish_selection(contract)?;
                    validate_partial_finish_selection(selection, &offered)?;
                    let mut missing_conditions = vec![];
                    for condition in graph.conditions.iter().filter(|condition| {
                        selection
                            .missing_conditions
                            .contains(&condition.condition_id)
                    }) {
                        let reference = match &condition.kind {
                            ConditionKind::RepositoryCheck { definition } => definition,
                            ConditionKind::Review { criterion } => criterion,
                        };
                        if !missing_conditions.contains(reference) {
                            missing_conditions.push(reference.clone());
                        }
                    }
                    FinishMode::ForcePartial {
                        approval: request_evidence.clone(),
                        selected_activations: selection.selected_activations.clone(),
                        stop_activations: selection.stop_activations.clone(),
                        missing_conditions,
                        missing_condition_ids: selection.missing_conditions.clone(),
                    }
                } else {
                    FinishMode::Normal
                };
                Ok(ControlParameters::FinishTurn { mode })
            }
            _ => Err(error("unsupported extended human control")),
        }
    }

    pub(in super::super) fn human_revision_invalidation(
        &self,
        target: &ActivationRef,
    ) -> Result<Vec<ActivationRef>> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        human_revision_invalidation(snapshot.contract(), target)
    }

    pub(in super::super) fn human_turn_controls(
        &self,
        issued_at_ms: u64,
    ) -> Result<HumanTurnControls> {
        let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
        let contract = snapshot.contract();
        let graph = contract
            .graph()
            .ok_or_else(|| error("turn graph is unavailable"))?;
        let epoch = contract
            .epochs()
            .last()
            .ok_or_else(|| error("turn epoch is unavailable"))?;
        let request = |action,
                       continuation: Option<HumanContinuationSelection>,
                       partial_finish|
         -> Result<HumanControlActionRequest> {
            Ok(HumanControlActionRequest {
                schema_version: CONTROL_COMMAND_SCHEMA_VERSION,
                command_id: CommandId::new(format!(
                    "turn-capability-{:x}",
                    Sha256::digest(serde_json::to_vec(&(action, &continuation)).map_err(error)?)
                ))
                .map_err(error)?,
                session_id: snapshot.owner().session_id.clone(),
                turn_id: snapshot.turn_id().clone(),
                execution_epoch_id: epoch.id.clone(),
                expected_turn_revision: contract.revision(),
                expected_graph_revision: graph.revision,
                activation: None,
                action,
                instruction: None,
                include_previous_output: false,
                context: None,
                blocker_id: None,
                human_response: None,
                partial_finish,
                continuation,
            })
        };
        let assess = |request: Result<HumanControlActionRequest>| {
            capability((|| {
                let request = request?;
                self.execution_admission()?;
                request.validate()?;
                let evidence = snapshot
                    .request_ref()
                    .ok_or_else(|| error("turn request is unavailable"))?;
                let canonical =
                    self.build_human_control(&request, issued_at_ms, evidence, None, true)?;
                self.preview_human_control(&CommandReceiptView {
                    request: canonical,
                    source: CommandSourceRecord::Human {
                        session_id: snapshot.owner().session_id.clone(),
                        turn_id: snapshot.turn_id().clone(),
                        request_evidence: evidence.clone(),
                    },
                    revision: 0,
                    state: ControlCommandState::Requested,
                    last_transition: None,
                })
            })())
        };
        human_turn_control_choices(contract, |action, continuation, partial| {
            if action == HumanControlAction::Continue && continuation.is_none() {
                capability(Ok(()))
            } else {
                assess(request(action, continuation, partial))
            }
        })
    }
}

/// Selecting any condition of the graph's check group reruns it between
/// fresh captures and records readiness again; other commands keep their
/// results. `group` is the group and its command count.
fn continue_conditions(
    selected: &[ConditionId],
    group: Option<(CheckGroup, usize)>,
) -> Result<Vec<ConditionId>> {
    let mut conditions = selected.to_vec();
    let Some((group, count)) = group else {
        return Ok(conditions);
    };
    if selected.iter().any(|id| group.contains(id)) {
        for id in [
            group.condition_id(0),
            group.condition_id(count + 1),
            group.ready_id(),
        ] {
            let id = ConditionId::new(id).map_err(error)?;
            if !conditions.contains(&id) {
                conditions.push(id);
            }
        }
    }
    Ok(conditions)
}

/// Shared canonical target/impact derivation. It conveys no runtime authority.
fn human_revision_invalidation(
    contract: &TurnContract,
    target: &ActivationRef,
) -> Result<Vec<ActivationRef>> {
    let graph = contract
        .graph()
        .ok_or_else(|| error("turn graph is unavailable"))?;
    let mut affected = HashSet::from([target.node_id.clone()]);
    loop {
        let before = affected.len();
        for edge in &graph.dependencies {
            if affected.contains(&edge.parent) {
                affected.insert(edge.child.clone());
            }
        }
        if before == affected.len() {
            break;
        }
    }
    let invalidate = graph
        .nodes
        .iter()
        .filter(|node| node.node_id != target.node_id && affected.contains(&node.node_id))
        .filter_map(|node| {
            contract
                .activations()
                .iter()
                .rev()
                .find(|item| item.activation.node_id == node.node_id)
                .map(|item| item.activation.clone())
        })
        .collect();
    Ok(invalidate)
}

fn human_turn_control_choices(
    contract: &TurnContract,
    assess: impl Fn(
        HumanControlAction,
        Option<HumanContinuationSelection>,
        Option<HumanPartialFinishSelection>,
    ) -> ControlPlaneCapability,
) -> Result<HumanTurnControls> {
    let graph = contract
        .graph()
        .ok_or_else(|| error("turn graph is unavailable"))?;
    let epoch = contract
        .epochs()
        .last()
        .ok_or_else(|| error("turn epoch is unavailable"))?;
    let mut continuation_choices = vec![];
    for node in &graph.nodes {
        if let Some(item) = contract
            .activations()
            .iter()
            .rev()
            .find(|item| item.activation.node_id == node.node_id)
        {
            if matches!(
                item.state,
                ActivationState::Failed
                    | ActivationState::Interrupted
                    | ActivationState::Superseded
                    | ActivationState::Unstarted
            ) {
                let capability = assess(
                    HumanControlAction::Continue,
                    Some(HumanContinuationSelection {
                        restart: vec![item.activation.clone()],
                        checks: vec![],
                    }),
                    None,
                );
                continuation_choices.push(HumanContinuationChoice {
                    activation: item.activation.clone(),
                    state: format!("{:?}", item.state).to_lowercase(),
                    capability,
                });
            }
        }
    }
    // While a check group is not ready, each of its conditions can be rerun.
    let group = group_of(graph);
    let check_choices = graph
        .conditions
        .iter()
        .filter(|condition| {
            !contract.condition_satisfied(&condition.condition_id)
                || group.as_ref().is_some_and(|(group, _)| {
                    group.contains(&condition.condition_id)
                        && ConditionId::new(group.ready_id())
                            .is_ok_and(|id| !contract.condition_satisfied(&id))
                })
        })
        .map(|condition| {
            Ok(HumanCheckChoice {
                condition_id: condition.condition_id.clone(),
                required_conditions: continue_conditions(
                    std::slice::from_ref(&condition.condition_id),
                    group.clone(),
                )?
                .into_iter()
                .filter(|id| id != &condition.condition_id)
                .collect(),
                capability: assess(
                    HumanControlAction::Continue,
                    Some(HumanContinuationSelection {
                        restart: vec![],
                        checks: vec![condition.condition_id.clone()],
                    }),
                    None,
                ),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let available = continuation_choices
        .iter()
        .any(|item| item.capability.enabled || item.capability.requires_revalidation)
        || check_choices
            .iter()
            .any(|item| item.capability.enabled || item.capability.requires_revalidation);
    Ok(HumanTurnControls {
        execution_epoch_id: epoch.id.clone(),
        continue_turn: if available {
            assess(HumanControlAction::Continue, None, None)
        } else {
            capability(Err(error(
                "No exact executable continuation is available; inspect the retained blockers and effects.")))
        },
        finish: assess(HumanControlAction::Finish, None, None),
        continuation_choices,
        check_choices,
        partial_finish: {
            let review = partial_finish_selection(contract)?;
            HumanPartialFinishReview {
                capability: assess(HumanControlAction::Finish, None, Some(review.clone())),
                available_sinks: contract
                    .current_accepted_activations()
                    .iter()
                    .filter(|item| {
                        !graph
                            .dependencies
                            .iter()
                            .any(|edge| edge.parent == item.activation.node_id)
                    })
                    .map(|item| item.activation.clone())
                    .collect(),
                review,
            }
        },
    })
}

impl SessionTurnControlPlane {
    /// Preserve canonical closure/selection details after restart. These are
    /// disabled presentation controls, not executable recovery offers.
    pub(crate) fn expose_closed_turn_controls(
        &mut self,
        snapshot: &DurableTurnSnapshot,
    ) -> Result<()> {
        if snapshot
            .contract()
            .state()
            .is_some_and(|state| state.is_closed())
        {
            self.turn_controls = Some(human_turn_control_choices(
                snapshot.contract(),
                |_, _, _| {
                    capability(Err(error(
                        "This turn is closed; its recorded outcomes are read-only.",
                    )))
                },
            )?);
        }
        Ok(())
    }

    /// Reading a recovered turn never acquires a repository or live grant. These
    /// disabled capabilities only expose requests to the existing authenticated
    /// action endpoint, which attaches and revalidates all real authority.
    pub(crate) fn expose_recovery_requests(
        &mut self,
        snapshot: &DurableTurnSnapshot,
    ) -> Result<()> {
        let contract = snapshot.contract();
        if self.superseded_conversation
            || contract.state() != Some(LogicalTurnState::NeedsAttention)
        {
            return Ok(());
        }
        let reason = "Axocoatl will reconnect this Session runtime and revalidate the exact revision, remaining budget, grant and repository before applying this request. Recovery can be refused; no work starts from this read-only view.";
        let request = || ControlPlaneCapability {
            enabled: false,
            requires_revalidation: true,
            reason: reason.into(),
        };
        self.turn_controls = Some(human_turn_control_choices(contract, |_, _, _| request())?);
        for node in &mut self.nodes {
            let Some(latest) = contract
                .activations()
                .iter()
                .rev()
                .find(|item| item.activation.node_id.as_str() == node.node_id)
            else {
                continue;
            };
            for activation in &mut node.activations {
                let crate::session_control_plane::ControlPlaneActivationRef::Exact {
                    activation: exact,
                } = &activation.reference
                else {
                    continue;
                };
                if *exact == latest.activation && latest.state == ActivationState::Accepted {
                    activation.capabilities.revise = request();
                    activation.capabilities.revise_invalidates =
                        human_revision_invalidation(contract, exact)?;
                }
                activation.capabilities.retry.reason = "This epoch is paused. Use Continue in Turn controls to select failed or interrupted work for a new epoch.".into();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod check_continuation_tests {
    use super::*;
    #[test]
    fn selecting_one_required_check_refreshes_captures_and_readiness() {
        let group = || Some((CheckGroup::required(), 3));
        let selected = ConditionId::new("required-check:2").unwrap();
        let result = continue_conditions(std::slice::from_ref(&selected), group()).unwrap();
        assert_eq!(
            result.iter().map(ConditionId::as_str).collect::<Vec<_>>(),
            vec![
                "required-check:2",
                "required-check:0",
                "required-check:4",
                "required-check:ready"
            ]
        );
        // Another condition, or a turn without checks, selects only itself.
        let other = ConditionId::new("review").unwrap();
        assert_eq!(
            continue_conditions(std::slice::from_ref(&other), group()).unwrap(),
            vec![other]
        );
        assert_eq!(
            continue_conditions(std::slice::from_ref(&selected), None).unwrap(),
            vec![selected]
        );
    }

    #[test]
    fn selected_check_adds_capture_and_readiness_without_other_commands() {
        let selected = ConditionId::new("standing:receipt:2").unwrap();
        let result = continue_conditions(
            std::slice::from_ref(&selected),
            Some((CheckGroup::standing("receipt"), 3)),
        )
        .unwrap();
        assert_eq!(
            result.iter().map(ConditionId::as_str).collect::<Vec<_>>(),
            vec![
                "standing:receipt:2",
                "standing:receipt:0",
                "standing:receipt:4",
                "standing:receipt:ready"
            ]
        );
        assert_eq!(
            continue_conditions(
                std::slice::from_ref(&selected),
                Some((CheckGroup::standing("other"), 3))
            )
            .unwrap(),
            vec![selected]
        );
    }
}
