//! Dynamic graph and typed-wait contracts inside the existing turn fold.
//! Every evidence reference requires host resolution; these types authenticate
//! neither admission nor a human/machine response and dispatch no work.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GraphMutation {
    Add {
        node_id: TurnNodeId,
    },
    ReplaceFuture {
        previous: TurnNodeId,
        replacement: TurnNodeId,
        rewire_dependents: Vec<TurnNodeId>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GraphRevisionRecord {
    pub previous: TurnGraphSnapshot,
    pub resulting_snapshot: GraphSnapshotId,
    pub mutation: GraphMutation,
    pub admission_evidence: EvidenceRef,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplacedTurnNode {
    pub previous: TurnNodeId,
    pub replacement: TurnNodeId,
    pub graph_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnBlockerKind {
    /// The trusted registry, not this reference or an Agent assertion, proves
    /// that this definition is machine-resolvable and excludes human approval.
    Machine {
        blocker_type: BlockerTypeId,
        definition: EvidenceRef,
        response_schema: EvidenceRef,
    },
    HumanApproval {
        approval_request: EvidenceRef,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypedTurnBlocker {
    pub schema_version: u32,
    pub blocker_id: BlockerId,
    pub activation: ActivationRef,
    pub kind: TurnBlockerKind,
    /// Exact request, grant revision and retained parameters, not just prose.
    pub command_id: CommandId,
    pub invocation_id: Option<InvocationId>,
    pub grant: Option<GrantSnapshotRef>,
    pub parameters: EvidenceRef,
    /// Retained exact safe-boundary/wait evidence; never a recreated live handle.
    pub safe_boundary: EvidenceRef,
    pub evidence: EvidenceRef,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnBlockerResponse {
    MachineEvidence {
        definition: EvidenceRef,
        response_schema: EvidenceRef,
        evidence: EvidenceRef,
    },
    HumanApproval {
        approval_request: EvidenceRef,
        approval_evidence: EvidenceRef,
    },
    /// Resolves the human wait with a negative decision. This is neither an
    /// approval nor invocation settlement: the host must deny the exact request
    /// and retain authoritative non-dispatch/outcome evidence separately. The
    /// actor may then choose another permitted action.
    HumanDecline {
        approval_request: EvidenceRef,
        reason: EvidenceRef,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TurnBlockerState {
    Pending,
    /// The response settles the wait, not an invocation or permission decision.
    /// HumanDecline remains a distinct retained negative response.
    Resolved {
        response: TurnBlockerResponse,
    },
    Interrupted {
        epoch_id: ExecutionEpochId,
    },
    Abandoned {
        evidence: EvidenceRef,
    },
    Replaced {
        replacement: TurnNodeId,
    },
    Closed {
        closure: TurnClosure,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContractBlocker {
    pub blocker: TypedTurnBlocker,
    pub state: TurnBlockerState,
}

fn invalid<T>(why: &'static str) -> Result<T, TurnContractError> {
    Err(TurnContractError::InvalidTransition(why))
}

impl TurnContract {
    pub fn graph_history(&self) -> &[GraphRevisionRecord] {
        &self.graph_history
    }
    pub fn replaced_nodes(&self) -> &[ReplacedTurnNode] {
        &self.replaced_nodes
    }
    pub fn blockers(&self) -> &[ContractBlocker] {
        &self.blockers
    }
    pub fn pending_blocker_count(&self) -> usize {
        self.blockers
            .iter()
            .filter(|item| item.state == TurnBlockerState::Pending)
            .count()
    }
    /// The current live controller refuses these histories until it implements
    /// the graph/resource/authority and typed response proof joins.
    pub fn has_dynamic_contract_history(&self) -> bool {
        !self.graph_history.is_empty() || !self.blockers.is_empty()
    }
    /// Retained generations remain counted after replacement. Preserve capacity
    /// for declared first materializations and the currently known fresh
    /// generations needed by required work and its dependency ancestors. This
    /// is no guarantee that arbitrary future failures or retries will fit.
    pub(super) fn validate_dynamic_graph_capacity(&self) -> Result<(), TurnContractError> {
        if self.graph_history.is_empty() {
            return Ok(());
        }
        let graph = self
            .graph
            .as_ref()
            .ok_or(TurnContractError::InvalidTransition("turn has no graph"))?;
        let materialized = self
            .activations
            .iter()
            .map(|item| &item.activation.node_id)
            .collect::<HashSet<_>>();
        let retained_and_declared = materialized
            .iter()
            .copied()
            .chain(graph.nodes.iter().map(|node| &node.node_id))
            .collect::<HashSet<_>>();
        if retained_and_declared.len() > MAX_CONTRACT_NODES {
            return Err(TurnContractError::LimitExceeded(
                "retained and declared graph node count",
            ));
        }
        let mut required = graph
            .nodes
            .iter()
            .filter(|node| node.required)
            .map(|node| &node.node_id)
            .chain(
                graph
                    .conditions
                    .iter()
                    .flat_map(|condition| &condition.nodes),
            )
            .collect::<HashSet<_>>();
        loop {
            let previous = required.len();
            for edge in &graph.dependencies {
                if required.contains(&edge.child) {
                    required.insert(&edge.parent);
                }
            }
            if required.len() == previous {
                break;
            }
        }
        let needed_generations = graph
            .nodes
            .iter()
            .filter(|node| match self.latest_activation(&node.node_id) {
                None => true,
                Some(latest) => {
                    required.contains(&node.node_id)
                        && matches!(
                            latest.state,
                            ActivationState::Failed
                                | ActivationState::Interrupted
                                | ActivationState::Superseded
                        )
                }
            })
            .count();
        if self.activations.len().saturating_add(needed_generations) > MAX_CONTRACT_ACTIVATIONS {
            return Err(TurnContractError::LimitExceeded(
                "declared graph activation capacity",
            ));
        }
        Ok(())
    }
    fn ever_started(&self, node: &TurnNodeId) -> bool {
        self.commands
            .values()
            .any(|envelope| match &envelope.event {
                TurnContractEvent::StartActivation { input } => input.activation.node_id == *node,
                TurnContractEvent::StartPreparedActivation { activation } => {
                    activation.node_id == *node
                }
                _ => false,
            })
    }
    pub(super) fn revise_graph(
        &mut self,
        epoch: &ExecutionEpochId,
        previous_graph: &GraphSnapshotId,
        next: &TurnGraphSnapshot,
        mutation: &GraphMutation,
        evidence: &EvidenceRef,
    ) -> Result<(), TurnContractError> {
        self.require_epoch(epoch)?;
        let old = self
            .graph
            .as_ref()
            .ok_or(TurnContractError::InvalidTransition("turn has no graph"))?
            .clone();
        if old.snapshot_id != *previous_graph
            || old.revision.checked_add(1) != Some(next.revision)
            || old.snapshot_id == next.snapshot_id
            || self
                .graph_history
                .iter()
                .any(|item| item.previous.snapshot_id == next.snapshot_id)
        {
            return invalid(
                "graph revision must name the exact current snapshot and a fresh successor",
            );
        }
        // Reuse every existing shape/size/acyclicity/savepoint rule; revision
        // succession above is independent from the initial graph's revision-one rule.
        let mut shape = next.clone();
        shape.revision = 1;
        shape.validate(
            self.session_id
                .as_ref()
                .ok_or(TurnContractError::InvalidIdentity)?,
        )?;
        let (new_id, removed) = match mutation {
            GraphMutation::Add { node_id } => (node_id, None),
            GraphMutation::ReplaceFuture {
                previous,
                replacement,
                ..
            } => (replacement, Some(previous)),
        };
        let fresh = next
            .nodes
            .iter()
            .find(|node| node.node_id == *new_id)
            .ok_or(TurnContractError::InvalidTransition(
                "new graph node is missing",
            ))?;
        if old.nodes.iter().any(|node| node.node_id == *new_id)
            || self
                .graph_history
                .iter()
                .flat_map(|item| &item.previous.nodes)
                .any(|node| {
                    node.node_id == *new_id
                        || node.conversation_id == fresh.conversation_id
                        || node.slot_id == fresh.slot_id
                })
            || old.nodes.iter().any(|node| {
                node.conversation_id == fresh.conversation_id || node.slot_id == fresh.slot_id
            })
            || fresh.starting_savepoint != ConversationSavepoint::Empty
        {
            return invalid("dynamic node requires fresh node, turn-local slot, conversation and empty savepoint");
        }
        let surviving_old = old
            .nodes
            .iter()
            .filter(|node| Some(&node.node_id) != removed)
            .collect::<Vec<_>>();
        let surviving_new = next
            .nodes
            .iter()
            .filter(|node| node.node_id != *new_id)
            .collect::<Vec<_>>();
        if surviving_new != surviving_old {
            return invalid("graph edit changed unrelated node identity, definition or order");
        }
        let mut expected_conditions = old.conditions.clone();
        match mutation {
            GraphMutation::Add { .. } => {
                let unchanged = next
                    .dependencies
                    .iter()
                    .filter(|edge| edge.child != *new_id)
                    .collect::<Vec<_>>();
                if unchanged != old.dependencies.iter().collect::<Vec<_>>()
                    || next.dependencies.iter().any(|edge| edge.parent == *new_id)
                {
                    return invalid(
                        "Add changes only the new dependent node's incoming dependencies",
                    );
                }
            }
            GraphMutation::ReplaceFuture {
                previous,
                replacement,
                rewire_dependents,
            } => {
                let previous_node = old
                    .nodes
                    .iter()
                    .find(|node| node.node_id == *previous)
                    .ok_or(TurnContractError::InvalidTransition(
                        "replacement target does not exist",
                    ))?;
                if self.ever_started(previous) || (previous_node.required && !fresh.required) {
                    return invalid(
                        "replacement requires never-started work and cannot remove required work",
                    );
                }
                let expected = old
                    .dependencies
                    .iter()
                    .filter(|edge| edge.parent == *previous)
                    .map(|edge| &edge.child)
                    .collect::<HashSet<_>>();
                if rewire_dependents.iter().collect::<HashSet<_>>() != expected
                    || rewire_dependents.len() != expected.len()
                    || rewire_dependents.iter().any(|node| self.ever_started(node))
                {
                    return invalid("replacement must explicitly rewire every never-started dependent exactly once");
                }
                let expected_edges = old
                    .dependencies
                    .iter()
                    .map(|edge| DependencyEdge {
                        parent: if edge.parent == *previous {
                            replacement.clone()
                        } else {
                            edge.parent.clone()
                        },
                        child: if edge.child == *previous {
                            replacement.clone()
                        } else {
                            edge.child.clone()
                        },
                    })
                    .collect::<Vec<_>>();
                if next.dependencies != expected_edges {
                    return invalid("replacement must preserve exact incoming and rewired dependency identities");
                }
                for condition in &mut expected_conditions {
                    for node in &mut condition.nodes {
                        if node == previous {
                            *node = replacement.clone();
                        }
                    }
                }
                self.replaced_nodes.push(ReplacedTurnNode {
                    previous: previous.clone(),
                    replacement: replacement.clone(),
                    graph_revision: next.revision,
                });
                for item in &mut self.blockers {
                    if item.blocker.activation.node_id == *previous
                        && item.state == TurnBlockerState::Pending
                    {
                        item.state = TurnBlockerState::Replaced {
                            replacement: replacement.clone(),
                        };
                    }
                }
            }
        }
        // Existing obligations cannot disappear or change meaning. Additional
        // explicitly declared obligations must include the added/replacement node.
        if !expected_conditions
            .iter()
            .all(|condition| next.conditions.contains(condition))
            || next
                .conditions
                .iter()
                .filter(|condition| !expected_conditions.contains(condition))
                .any(|condition| !condition.nodes.contains(new_id))
        {
            return invalid("graph edit removed or changed required completion evidence");
        }
        self.graph_history.push(GraphRevisionRecord {
            previous: old,
            resulting_snapshot: next.snapshot_id.clone(),
            mutation: mutation.clone(),
            admission_evidence: evidence.clone(),
        });
        self.graph = Some(next.clone());
        Ok(())
    }
    pub(super) fn require_unblocked(
        &self,
        activation: &ActivationRef,
    ) -> Result<(), TurnContractError> {
        if self.blockers.iter().any(|item| {
            item.blocker.activation == *activation && item.state == TurnBlockerState::Pending
        }) {
            return invalid("exact activation has unresolved typed blockers");
        }
        Ok(())
    }
    pub(super) fn open_blocker(
        &mut self,
        blocker: &TypedTurnBlocker,
    ) -> Result<(), TurnContractError> {
        self.require_live_activation(&blocker.activation)?;
        self.graph_node(&blocker.activation.node_id)?;
        self.require_previous(&blocker.activation, &blocker.activation.node_id)?;
        if blocker.schema_version != 1
            || self
                .blockers
                .iter()
                .any(|item| item.blocker.blocker_id == blocker.blocker_id)
        {
            return invalid("unsupported or duplicate typed blocker identity");
        }
        let target = self
            .latest_activation(&blocker.activation.node_id)
            .ok_or(TurnContractError::InvalidIdentity)?;
        if !matches!(
            target.state,
            ActivationState::Running | ActivationState::Unstarted
        ) || blocker.grant != target.input.grant
        {
            return invalid("blocker requires exact unfinished activation and grant revision");
        }
        if let Some(invocation_id) = &blocker.invocation_id {
            if !self.invocations.iter().any(|item| {
                item.invocation_id == *invocation_id
                    && item.activation == blocker.activation
                    && item.evidence == InvocationEvidence::Intent
            }) {
                return invalid("blocker invocation is not an exact unresolved intent");
            }
        }
        self.blockers.push(ContractBlocker {
            blocker: blocker.clone(),
            state: TurnBlockerState::Pending,
        });
        Ok(())
    }
    pub(super) fn abandon_exact_blocker(
        &mut self,
        id: &BlockerId,
        activation: &ActivationRef,
        evidence: &EvidenceRef,
    ) -> Result<(), TurnContractError> {
        self.require_live_activation(activation)?;
        self.require_previous(activation, &activation.node_id)?;
        let item = self
            .blockers
            .iter_mut()
            .find(|item| item.blocker.blocker_id == *id)
            .ok_or(TurnContractError::InvalidTransition(
                "typed blocker does not exist",
            ))?;
        if item.blocker.activation != *activation || item.state != TurnBlockerState::Pending {
            return invalid("only the exact pending live blocker can be abandoned");
        }
        item.state = TurnBlockerState::Abandoned {
            evidence: evidence.clone(),
        };
        Ok(())
    }

    pub(super) fn resolve_blocker(
        &mut self,
        id: &BlockerId,
        activation: &ActivationRef,
        response: &TurnBlockerResponse,
    ) -> Result<(), TurnContractError> {
        self.require_live_activation(activation)?;
        self.require_previous(activation, &activation.node_id)?;
        self.graph_node(&activation.node_id)?;
        let item = self
            .blockers
            .iter_mut()
            .find(|item| item.blocker.blocker_id == *id)
            .ok_or(TurnContractError::InvalidTransition(
                "typed blocker does not exist",
            ))?;
        if item.state != TurnBlockerState::Pending || item.blocker.activation != *activation {
            return invalid("typed blocker is no longer resumable by this exact activation");
        }
        let matches = match (&item.blocker.kind, response) {
            (
                TurnBlockerKind::Machine {
                    definition,
                    response_schema,
                    ..
                },
                TurnBlockerResponse::MachineEvidence {
                    definition: actual_definition,
                    response_schema: actual_schema,
                    ..
                },
            ) => definition == actual_definition && response_schema == actual_schema,
            (
                TurnBlockerKind::HumanApproval { approval_request },
                TurnBlockerResponse::HumanApproval {
                    approval_request: actual,
                    ..
                }
                | TurnBlockerResponse::HumanDecline {
                    approval_request: actual,
                    ..
                },
            ) => approval_request == actual,
            _ => false,
        };
        if !matches {
            return invalid("typed blocker response kind or exact schema/request differs");
        }
        item.state = TurnBlockerState::Resolved {
            response: response.clone(),
        };
        Ok(())
    }
    pub(super) fn interrupt_blockers(&mut self, epoch: &ExecutionEpochId) {
        for item in &mut self.blockers {
            if item.blocker.activation.execution_epoch_id == *epoch
                && item.state == TurnBlockerState::Pending
            {
                item.state = TurnBlockerState::Interrupted {
                    epoch_id: epoch.clone(),
                };
            }
        }
    }
    pub(super) fn abandon_blockers(&mut self, activation: &ActivationRef, evidence: &EvidenceRef) {
        for item in &mut self.blockers {
            if item.blocker.activation == *activation && item.state == TurnBlockerState::Pending {
                item.state = TurnBlockerState::Abandoned {
                    evidence: evidence.clone(),
                };
            }
        }
    }
}
