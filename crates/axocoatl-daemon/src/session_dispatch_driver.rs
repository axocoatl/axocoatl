//! Concurrent autonomous execution driven by the canonical turn projection.
//! The host supplies retained initial inputs and resolves resources. Scheduling
//! never treats a process-local completion or a cached summary as acceptance.
use super::*;
use tokio::task::JoinSet;

/// Host-captured initial input choices. Ownership/definition/savepoint and direct
/// parents come from the canonical graph, never this setup object.
#[derive(Clone)]
pub struct AutonomousNodeInput {
    pub node_id: TurnNodeId,
    pub guidance: Vec<EvidenceRef>,
    pub attachments: Vec<EvidenceRef>,
    pub repository: RepositoryInput,
    pub budget: EvidenceRef,
    pub grant: Option<GrantSnapshotRef>,
}

/// Resolve a configured backend and tools; this operation must not invoke the
/// model or perform Agent work. The owned actor port admits every actual call.
#[async_trait]
pub trait AutonomousActivationFactory: Send + Sync {
    async fn resources(
        &self,
        input: &ActivationInputManifest,
    ) -> std::result::Result<AutonomousActivationResources, String>;
}

pub struct AutonomousTurnDriver {
    controller: SessionDispatchController,
    token: String,
    inputs: HashMap<TurnNodeId, AutonomousNodeInput>,
    factory: Arc<dyn AutonomousActivationFactory>,
    children: Option<JoinSet<Result<()>>>,
    started: HashSet<ActivationId>,
    finished: bool,
    // Last; detached child draining takes this ownership when run is abandoned.
    execution: Option<super::execution_lifetime::ExecutionTicket>,
}

struct OwnedDriverInput {
    controller: SessionDispatchController,
    factory: Arc<dyn AutonomousActivationFactory>,
    input: ActivationInputManifest,
    _execution: super::execution_lifetime::ExecutionTicket,
}

impl OwnedDriverInput {
    async fn run(self) -> Result<()> {
        execute_owned_input(
            self.controller.clone(),
            self.factory.clone(),
            self.input.clone(),
        )
        .await
    }
}

struct OwnedDriverDrain {
    children: JoinSet<Result<()>>,
    _execution: Option<super::execution_lifetime::ExecutionTicket>,
}

impl OwnedDriverDrain {
    async fn run(mut self) {
        while self.children.join_next().await.is_some() {}
    }
}

pub struct TurnDriveOutcome {
    pub snapshot: DurableTurnSnapshot,
    /// Conversation promotion proof, not proof that external effects rolled back.
    pub finalized: Option<FinalizedTurn>,
}

impl SessionDispatchController {
    /// Acquire the one process-local driver for the durable turn. A recovered
    /// NeedsAttention turn stays idle until an explicit Continue is admitted.
    pub fn autonomous_turn_driver(
        &self,
        inputs: Vec<AutonomousNodeInput>,
        factory: Arc<dyn AutonomousActivationFactory>,
    ) -> Result<AutonomousTurnDriver> {
        let mut state = self.lock()?;
        state.execution_admission()?;
        if state.driver.is_some() {
            return Err(error("this turn already has an execution driver"));
        }
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        if snapshot.contract().activations().iter().any(|item| {
            item.state == ActivationState::Running
                && state.bound.contains_key(&item.activation.activation_id)
        }) {
            return Err(error(
                "a running activation already has an executor outside this driver",
            ));
        }
        let graph = snapshot
            .contract()
            .graph()
            .ok_or_else(|| error("missing turn graph"))?;
        if inputs.len() != graph.nodes.len() {
            return Err(error(
                "initial input choices must cover every declared node",
            ));
        }
        let mut selected = HashMap::new();
        for input in inputs {
            if !graph.nodes.iter().any(|node| node.node_id == input.node_id)
                || input.guidance.len().saturating_add(input.attachments.len())
                    > MAX_INPUT_REFERENCES
                || selected.insert(input.node_id.clone(), input).is_some()
            {
                return Err(error(
                    "initial node input is duplicated, oversized or outside the graph",
                ));
            }
        }
        // Reject malformed host setup before acquiring driver ownership or
        // appending a generation. Actual parents/current authority are checked
        // again when that node becomes ready.
        for node in &graph.nodes {
            let captured = &selected[&node.node_id];
            let proposed = ActivationInputManifest {
                manifest_id: InputManifestId::new(format!("preflight:{}", uuid::Uuid::new_v4()))
                    .map_err(error)?,
                activation: next_activation(&snapshot, node, 1)?,
                definition: node.definition.clone(),
                conversation_id: node.conversation_id.clone(),
                starting_savepoint: node.starting_savepoint.clone(),
                parents: vec![],
                guidance: captured.guidance.clone(),
                attachments: captured.attachments.clone(),
                repository: captured.repository.clone(),
                budget: captured.budget.clone(),
                grant: captured.grant.clone(),
                revision_context: None,
            };
            state
                .content
                .validate_proposed_input(&snapshot, &proposed)
                .map_err(error)?;
            let grant = proposed
                .grant
                .as_ref()
                .ok_or_else(|| error("initial node input has no captured grant"))?;
            state
                .authority
                .grant_policy(grant.grant_id.as_str())
                .map_err(error)?;
        }
        let token = uuid::Uuid::new_v4().to_string();
        let execution = state.acquire_execution_ticket(self)?;
        state.driver = Some(token.clone());
        Ok(AutonomousTurnDriver {
            controller: self.clone(),
            token,
            inputs: selected,
            factory,
            children: Some(JoinSet::new()),
            started: HashSet::new(),
            finished: false,
            execution: Some(execution),
        })
    }
}

impl AutonomousTurnDriver {
    /// Child tasks await provider/tool work while external command submitters
    /// retain access to the same serialized authority and canonical journal.
    pub async fn run(mut self) -> Result<TurnDriveOutcome> {
        self.controller.lock()?.ready()?;
        let changed = self.controller.lock()?.changed.clone();
        loop {
            // Register before inspecting state: command completion between the
            // snapshot and select must not be lost as a notification race.
            let notification = changed.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            self.controller.reconcile_control_commands()?;
            let snapshot = self.controller.snapshot()?;
            let mut inspected_revision = snapshot.contract().revision();
            if snapshot.contract().state() == Some(LogicalTurnState::Running)
                && snapshot.contract().stop_requested().is_none()
            {
                let (allocations, revision) = self.allocate_ready()?;
                inspected_revision = revision;
                for input in allocations {
                    let controller = self.controller.clone();
                    let factory = self.factory.clone();
                    let execution = controller.lock()?.acquire_settlement_ticket(&controller)?;
                    self.started.insert(input.activation.activation_id.clone());
                    let owned = OwnedDriverInput {
                        controller,
                        factory,
                        input,
                        _execution: execution,
                    };
                    self.children.as_mut().unwrap().spawn(owned.run());
                }
            }
            if self.children.as_ref().unwrap().is_empty() {
                if self.controller.drive_turn_checks().await? {
                    continue;
                }
                if self.controller.drive_turn_review()? {
                    continue;
                }
                if let Some(outcome) = self.finish_quiescent(inspected_revision)? {
                    return Ok(outcome);
                }
                if self
                    .controller
                    .snapshot()?
                    .contract()
                    .stop_requested()
                    .is_some()
                {
                    notification.await;
                }
                continue;
            }
            tokio::select! {
                _ = &mut notification => {},
                result = self.children.as_mut().unwrap().join_next() => {
                    match result {
                        Some(Ok(Ok(()))) => {},
                        Some(Ok(Err(failure))) => return Err(failure),
                        Some(Err(failure)) => return Err(error(format!("activation task was lost: {failure}"))),
                        None => {},
                    }
                }
            }
        }
    }

    pub(super) fn allocate_ready(&self) -> Result<(Vec<ActivationInputManifest>, u64)> {
        let mut state = self.controller.lock()?;
        state.ready()?;
        if state
            .canonical
            .snapshot(&state.turn_id)
            .map_err(error)?
            .contract()
            .stop_requested()
            .is_some()
        {
            let revision = state
                .canonical
                .snapshot(&state.turn_id)
                .map_err(error)?
                .contract()
                .revision();
            return Ok((vec![], revision));
        }
        state.execution_admission()?;
        if state.driver.as_deref() != Some(&self.token) {
            return Err(error("execution driver no longer owns this turn"));
        }
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        let contract = snapshot.contract();
        let graph = contract
            .graph()
            .ok_or_else(|| error("missing turn graph"))?;
        let epoch = contract
            .epochs()
            .last()
            .ok_or_else(|| error("missing execution epoch"))?;
        if contract.state() != Some(LogicalTurnState::Running) || epoch.state != EpochState::Running
        {
            return Ok((vec![], contract.revision()));
        }
        let accepted = contract.current_accepted_activations();
        let reviewer = axocoatl_session::turn_review::review_node(graph).map(|node| &node.node_id);
        let mut ready = vec![];
        for node in &graph.nodes {
            let latest = contract
                .activations()
                .iter()
                .rev()
                .find(|activation| activation.activation.node_id == node.node_id);
            // The host starts each review round itself, with the result it
            // judges, once the required Agents and checks are done.
            if reviewer == Some(&node.node_id)
                && latest.is_none_or(|item| item.state == ActivationState::Superseded)
            {
                continue;
            }
            if let Some(item) = latest {
                if matches!(item.state, ActivationState::Accepted | ActivationState::Failed | ActivationState::Interrupted)
                    || (matches!(item.state, ActivationState::Running | ActivationState::Unstarted)
                        && (self.started.contains(&item.activation.activation_id)
                            || state.bound.contains_key(&item.activation.activation_id)))
                    || epoch.continuation.as_ref().is_some_and(|plan| plan.selections.iter().any(|selection|
                        matches!(selection, ContinuationSelection::LeaveBlocked { activation, .. } if *activation == item.activation)))
                {
                    continue;
                }
            }
            let required = graph
                .dependencies
                .iter()
                .filter(|edge| edge.child == node.node_id);
            let mut parents = vec![];
            let mut blocked = false;
            for edge in required {
                let Some(parent) = accepted
                    .iter()
                    .find(|parent| parent.activation.node_id == edge.parent)
                else {
                    blocked = true;
                    break;
                };
                parents.push(AcceptedParentInput {
                    activation: parent.activation.clone(),
                    checkpoint: parent
                        .checkpoint
                        .clone()
                        .ok_or_else(|| error("accepted parent has no checkpoint"))?,
                    output: parent
                        .output
                        .clone()
                        .ok_or_else(|| error("accepted parent has no output"))?,
                });
            }
            if blocked {
                continue;
            }
            let input = match latest {
                Some(item)
                    if matches!(
                        item.state,
                        ActivationState::Running | ActivationState::Unstarted
                    ) =>
                {
                    if item.activation.execution_epoch_id != epoch.id {
                        continue;
                    }
                    item.input.clone()
                }
                Some(item) if item.state == ActivationState::Superseded => {
                    let mut input = item.input.clone();
                    input.activation = next_activation(
                        &snapshot,
                        node,
                        item.activation
                            .generation
                            .checked_add(1)
                            .ok_or_else(|| error("activation generation overflow"))?,
                    )?;
                    input.manifest_id =
                        InputManifestId::new(format!("input:{}", uuid::Uuid::new_v4()))
                            .map_err(error)?;
                    input.parents = parents;
                    input.revision_context = None;
                    // Superseded descendants preserve the original semantic
                    // input/savepoint, changing only their accepted parents.
                    let envelope = driver_event(
                        &state,
                        "rebase",
                        TurnContractEvent::RebaseActivation {
                            previous: item.activation.clone(),
                            input: Box::new(input.clone()),
                        },
                    )?;
                    state.canonical.append(envelope).map_err(error)?;
                    input
                }
                None => {
                    if epoch.continuation.as_ref().is_some_and(|plan| plan.selections.iter().any(|selection|
                        matches!(selection, ContinuationSelection::LeaveUnmaterializedBlocked { node_id, .. } if *node_id == node.node_id))) {
                        continue;
                    }
                    let captured = self
                        .inputs
                        .get(&node.node_id)
                        .cloned()
                        .or(state.dynamic_node_input(&node.node_id)?)
                        .ok_or_else(|| error("ready graph node has no retained input admission"))?;
                    let input = ActivationInputManifest {
                        manifest_id: InputManifestId::new(format!(
                            "input:{}",
                            uuid::Uuid::new_v4()
                        ))
                        .map_err(error)?,
                        activation: next_activation(&snapshot, node, 1)?,
                        definition: node.definition.clone(),
                        conversation_id: node.conversation_id.clone(),
                        starting_savepoint: node.starting_savepoint.clone(),
                        parents,
                        guidance: captured.guidance.clone(),
                        attachments: captured.attachments.clone(),
                        repository: captured.repository.clone(),
                        budget: captured.budget.clone(),
                        grant: captured.grant.clone(),
                        revision_context: None,
                    };
                    let envelope = driver_event(
                        &state,
                        "start",
                        TurnContractEvent::StartActivation {
                            input: Box::new(input.clone()),
                        },
                    )?;
                    state.canonical.append(envelope).map_err(error)?;
                    input
                }
                _ => continue,
            };
            let current = state.canonical.snapshot(&state.turn_id).map_err(error)?;
            if current.contract().activations().iter().any(|item| {
                item.activation == input.activation && item.state == ActivationState::Unstarted
            }) {
                let envelope = driver_event(
                    &state,
                    "start-prepared",
                    TurnContractEvent::StartPreparedActivation {
                        activation: input.activation.clone(),
                    },
                )?;
                state.canonical.append(envelope).map_err(error)?;
            }
            ready.push(input);
        }
        if !ready.is_empty() {
            state.changed.notify_waiters();
        }
        let revision = state
            .canonical
            .snapshot(&state.turn_id)
            .map_err(error)?
            .contract()
            .revision();
        Ok((ready, revision))
    }

    pub(super) fn finish_quiescent(
        &mut self,
        inspected_revision: u64,
    ) -> Result<Option<TurnDriveOutcome>> {
        let mut state = self.controller.lock()?;
        state.ready()?;
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        // Retry/Revise may have arrived after ready-work allocation. Decide
        // quiescence under the same gate and inspect that new work before pause.
        if snapshot.contract().revision() != inspected_revision {
            return Ok(None);
        }
        if snapshot.contract().stop_requested().is_some()
            && !snapshot
                .contract()
                .state()
                .is_some_and(LogicalTurnState::is_closed)
        {
            // An externally prepared actor or check still owns settlement.
            return Ok(None);
        }
        let finalized = match snapshot.contract().state() {
            Some(turn_state) if turn_state.is_closed() => {
                // Closure may have come from a command. The finalizer's recovery
                // path establishes exact promotion before returning the proof.
                state.reconcile_promotions()?;
                Some(state.finalized_turn()?)
            }
            Some(LogicalTurnState::Running) if snapshot.contract().completion_satisfied() => {
                let envelope = driver_event(
                    &state,
                    "complete",
                    TurnContractEvent::Close {
                        closure: TurnClosure::Completed,
                    },
                )?;
                Some(state.close_and_promote(envelope)?)
            }
            Some(LogicalTurnState::Running) => {
                let epoch_id = snapshot.contract().epochs().last().unwrap().id.clone();
                let envelope =
                    driver_event(&state, "pause", TurnContractEvent::PauseEpoch { epoch_id })?;
                state.canonical.append(envelope).map_err(error)?;
                state.changed.notify_waiters();
                None
            }
            _ => None,
        };
        state.reconcile_control_commands()?;
        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
        self.finished = true;
        if state.driver.as_deref() == Some(&self.token) {
            state.driver = None;
        }
        Ok(Some(TurnDriveOutcome {
            snapshot,
            finalized,
        }))
    }
}

impl Drop for AutonomousTurnDriver {
    fn drop(&mut self) {
        if !self.finished {
            if let Ok(mut state) = self.controller.lock() {
                if state.driver.as_deref() == Some(&self.token) {
                    let result = (|| {
                        let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
                        // A factory wait has no executor and cannot have spent
                        // anything. Retain that fact before interrupting it;
                        // missing authority history is never interpreted as zero.
                        for item in snapshot.contract().activations().iter().filter(|item| {
                            item.state == ActivationState::Running
                                && !state.bound.contains_key(&item.activation.activation_id)
                        }) {
                            record_undispatched(&state, &snapshot, &item.input)?;
                        }
                        for bound in state.bound.values().filter(|bound| {
                            snapshot.contract().activations().iter().any(|item| {
                                item.activation == bound.activation
                                    && item.state == ActivationState::Running
                            })
                        }) {
                            let revision = state.authority.revision().map_err(error)?;
                            state
                                .authority
                                .stop_activation(&bound.activation, revision)
                                .map_err(error)?;
                            bound.control.cancel();
                        }
                        if snapshot.contract().state() == Some(LogicalTurnState::Running) {
                            let epoch_id = snapshot.contract().epochs().last().unwrap().id.clone();
                            let envelope = driver_event(
                                &state,
                                "driver-lost",
                                TurnContractEvent::InterruptEpoch { epoch_id },
                            )?;
                            state.canonical.append(envelope).map_err(error)?;
                        }
                        Ok(())
                    })();
                    let _ = state.fail_closed(result);
                    state.driver = None;
                    state.changed.notify_waiters();
                }
            }
        }
        // Preserve late tool/provider settlement when the caller abandons its
        // wait. Dropping JoinSet directly would abort those owned child futures.
        if let Some(children) = self.children.take() {
            if !children.is_empty() {
                let drain = OwnedDriverDrain {
                    children,
                    _execution: self.execution.take(),
                };
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(drain.run());
                }
                // Without a runtime the JoinSet requests abort. Every child
                // owns its own ticket until its future actually drops; losing
                // the drain therefore cannot announce a false idle boundary.
            }
        }
    }
}

async fn execute_owned_input(
    controller: SessionDispatchController,
    factory: Arc<dyn AutonomousActivationFactory>,
    input: ActivationInputManifest,
) -> Result<()> {
    let changed = controller.lock()?.changed.clone();
    let preparation = async {
        let resources = factory.resources(&input).await.map_err(error)?;
        match &input.repository {
            RepositoryInput::Recorded { snapshot } => {
                let repository = controller.repository_activation_resource(snapshot)?;
                repository.validate_current().await?;
                // Preparation repeats exact manifest, registration and authority
                // checks after the asynchronous live-resource lookup.
                controller.prepare_repository_activation(
                    input.activation.clone(),
                    resources,
                    repository,
                )
            }
            RepositoryInput::Unavailable => {
                controller.prepare_autonomous_activation(input.activation.clone(), resources)
            }
        }
    };
    tokio::pin!(preparation);
    let prepared = loop {
        let notification = changed.notified();
        tokio::pin!(notification);
        notification.as_mut().enable();
        {
            let state = controller.lock()?;
            if state.execution_admission().is_err() || state.current(&input.activation).is_err() {
                return Ok(());
            }
        }
        tokio::select! {
            prepared = &mut preparation => break prepared,
            _ = &mut notification => {},
        }
    };
    match prepared {
        Ok(prepared) => {
            prepared.run().await?;
        }
        Err(failure) => {
            let mut state = controller.lock()?;
            state.ready()?;
            if state.current(&input.activation).is_ok() {
                let result = (|| {
                    let snapshot = state.current(&input.activation)?;
                    record_undispatched(&state, &snapshot, &input)?;
                    // Diagnostics are bounded separately from the immutable inputs.
                    let diagnostic: String = failure.to_string().chars().take(4096).collect();
                    let evidence = state
                        .content
                        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                            text: format!("Activation resource preparation failed: {diagnostic}"),
                        })
                        .map_err(error)?;
                    let envelope = driver_event(
                        &state,
                        "prepare-failed",
                        TurnContractEvent::FailActivation {
                            activation: input.activation.clone(),
                            evidence: evidence.reference().clone(),
                        },
                    )?;
                    state.canonical.append(envelope).map_err(error)?;
                    Ok(())
                })();
                state.fail_closed(result)?;
            }
        }
    }
    controller.reconcile_control_commands()?;
    controller.lock()?.changed.notify_waiters();
    Ok(())
}

pub(super) fn record_undispatched(
    state: &DispatchState,
    snapshot: &DurableTurnSnapshot,
    input: &ActivationInputManifest,
) -> Result<()> {
    let ActivationEvidenceContent::Definition { profile, .. } = &state
        .content
        .resolve_activation_evidence(&input.definition.snapshot)
        .map_err(error)?
    else {
        return Err(error("failed setup has no retained execution definition"));
    };
    let revision = state.authority.revision().map_err(error)?;
    state
        .authority
        .record_undispatched_activation(snapshot, &input.activation, profile.clone(), revision)
        .map_err(error)
}

fn next_activation(
    snapshot: &DurableTurnSnapshot,
    node: &GraphNode,
    generation: u32,
) -> Result<ActivationRef> {
    Ok(ActivationRef {
        session_id: snapshot.owner().session_id.clone(),
        turn_id: snapshot.turn_id().clone(),
        execution_epoch_id: snapshot
            .contract()
            .epochs()
            .last()
            .ok_or_else(|| error("missing execution epoch"))?
            .id
            .clone(),
        node_id: node.node_id.clone(),
        generation,
        activation_id: ActivationId::new(format!("activation:{}", uuid::Uuid::new_v4()))
            .map_err(error)?,
    })
}

pub(super) fn driver_event(
    state: &DispatchState,
    operation: &str,
    event: TurnContractEvent,
) -> Result<TurnContractEnvelope> {
    let snapshot = state.canonical.snapshot(&state.turn_id).map_err(error)?;
    Ok(TurnContractEnvelope {
        schema_version: TURN_CONTRACT_SCHEMA_VERSION,
        command_id: CommandId::new(format!("driver:{operation}:{}", uuid::Uuid::new_v4()))
            .map_err(error)?,
        expected_revision: snapshot.contract().revision(),
        session_id: state.canonical.owner().session_id.clone(),
        turn_id: state.turn_id.clone(),
        event,
    })
}
