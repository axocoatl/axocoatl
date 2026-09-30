use super::*;
use axocoatl_session::control_command::{
    ControlCommandRequest, ControlCommandState, ControlParameters, DurableCommandReceipt,
    TrustedCommandSource, CONTROL_COMMAND_SCHEMA_VERSION,
};
use std::time::Duration;
use tokio::sync::Notify;

#[path = "session_dispatch_graph_tests.rs"]
mod graph_tests;

#[path = "session_dispatch_execution_lifetime_tests.rs"]
mod execution_lifetime_tests;

#[derive(Clone)]
struct DriverPlan {
    node: InputNode,
    provider: Arc<InputProvider>,
    wait: Option<Arc<Notify>>,
    tool: Option<Arc<CountingTool>>,
}

struct DriverFactory {
    plans: Mutex<HashMap<(String, u32), DriverPlan>>,
    resolved: Mutex<Vec<ActivationInputManifest>>,
    started: Notify,
}

impl DriverFactory {
    fn new(plans: Vec<DriverPlan>) -> Arc<Self> {
        Arc::new(Self {
            plans: Mutex::new(
                plans
                    .into_iter()
                    .map(|plan| {
                        (
                            (
                                plan.node.input.activation.node_id.as_str().to_owned(),
                                plan.node.input.activation.generation,
                            ),
                            plan,
                        )
                    })
                    .collect(),
            ),
            resolved: Mutex::new(vec![]),
            started: Notify::new(),
        })
    }
}

#[async_trait]
impl AutonomousActivationFactory for DriverFactory {
    async fn resources(
        &self,
        input: &ActivationInputManifest,
    ) -> std::result::Result<AutonomousActivationResources, String> {
        self.resolved.lock().unwrap().push(input.clone());
        let key = (
            input.activation.node_id.as_str().to_owned(),
            input.activation.generation,
        );
        let plan = self
            .plans
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .ok_or_else(|| format!("unexpected resource resolution {key:?}"))?;
        self.started.notify_one();
        if let Some(wait) = plan.wait {
            wait.notified().await;
        }
        let mut resources = input_resources(&plan.node, plan.provider);
        if let Some(tool) = plan.tool {
            let mut tools = axocoatl_tools::ToolExecutor::new();
            tools.register_builtin("effect", tool);
            resources.tools = Arc::new(tools);
        }
        Ok(resources)
    }
}

fn driver_plan(node: &InputNode, provider: Arc<InputProvider>) -> DriverPlan {
    DriverPlan {
        node: node.clone(),
        provider,
        wait: None,
        tool: None,
    }
}

fn driver_seed(node: &InputNode) -> AutonomousNodeInput {
    AutonomousNodeInput {
        node_id: node.input.activation.node_id.clone(),
        guidance: node.input.guidance.clone(),
        attachments: node.input.attachments.clone(),
        repository: node.input.repository.clone(),
        budget: node.input.budget.clone(),
        grant: node.input.grant.clone(),
    }
}

fn driver_seeds(fixture: &InputFixture) -> Vec<AutonomousNodeInput> {
    vec![driver_seed(&fixture.parent), driver_seed(&fixture.child)]
}

fn driver_control(
    controller: &SessionDispatchController,
    id: &str,
    parameters: ControlParameters,
) -> DurableCommandReceipt {
    let snapshot = controller.snapshot().unwrap();
    let request = ControlCommandRequest {
        schema_version: CONTROL_COMMAND_SCHEMA_VERSION,
        command_id: CommandId::new(id).unwrap(),
        session_id: snapshot.owner().session_id.clone(),
        turn_id: snapshot.turn_id().clone(),
        execution_epoch_id: snapshot.contract().epochs().last().unwrap().id.clone(),
        expected_turn_revision: snapshot.contract().revision(),
        expected_graph_revision: snapshot.contract().graph().unwrap().revision,
        issued_at_ms: now_ms().unwrap(),
        parameters,
    };
    let attribution = controller
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: format!("Authenticated local test request {id}"),
        })
        .unwrap();
    let source = TrustedCommandSource::human(
        request.session_id.clone(),
        request.turn_id.clone(),
        attribution.clone(),
    );
    controller.submit_control_command(request, source).unwrap()
}

async fn driven(driver: AutonomousTurnDriver) -> TurnDriveOutcome {
    tokio::time::timeout(Duration::from_secs(10), driver.run())
        .await
        .expect("driver must reach a durable boundary")
        .unwrap()
}

#[tokio::test]
async fn automatic_parent_then_child_uses_exact_acceptance_and_promotes_both() {
    let fixture = input_fixture();
    let parent = InputProvider::new(PARENT_V1, true, false);
    let child = InputProvider::new(CHILD_V1, false, false);
    let factory = DriverFactory::new(vec![
        driver_plan(&fixture.parent, parent.clone()),
        driver_plan(&fixture.child, child.clone()),
    ]);
    let driver = fixture
        .controller
        .autonomous_turn_driver(driver_seeds(&fixture), factory.clone())
        .unwrap();
    let outcome = driven(driver).await;
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    let accepted = outcome.snapshot.contract().current_accepted_activations();
    assert_eq!(accepted.len(), 2);
    let parent_activation = accepted
        .iter()
        .find(|item| item.activation.node_id == fixture.parent.input.activation.node_id)
        .unwrap();
    let child_activation = accepted
        .iter()
        .find(|item| item.activation.node_id == fixture.child.input.activation.node_id)
        .unwrap();
    assert_eq!(child_activation.input.parents.len(), 1);
    assert_eq!(
        child_activation.input.parents[0].activation,
        parent_activation.activation
    );
    assert_eq!(
        Some(&child_activation.input.parents[0].checkpoint),
        parent_activation.checkpoint.as_ref()
    );
    assert_eq!(
        Some(&child_activation.input.parents[0].output),
        parent_activation.output.as_ref()
    );
    let resolved = factory.resolved.lock().unwrap();
    assert_eq!(resolved.len(), 2);
    assert_eq!(
        resolved[0].activation.node_id,
        fixture.parent.input.activation.node_id
    );
    assert_eq!(resolved[1], child_activation.input);
    assert_eq!(parent.calls.load(Ordering::SeqCst), 2);
    assert_eq!(child.calls.load(Ordering::SeqCst), 1);
    assert_projected_only(
        &child,
        &[PARENT_V1, REQUEST],
        &["parent-private-tool-argument"],
    );
    let promotion = outcome.finalized.as_ref().unwrap().promotion();
    assert_eq!(promotion.selected.len(), 2);
    let state = fixture.controller.lock().unwrap();
    for selected in &promotion.selected {
        let candidate = state.memory.checkpoint(&selected.accepted).unwrap();
        assert_eq!(
            serde_json::to_value(state.memory.checkpoint(&selected.committed).unwrap()).unwrap(),
            serde_json::to_value(&candidate).unwrap()
        );
        let expected = if selected.node_id == fixture.parent.input.activation.node_id {
            &parent
        } else {
            &child
        };
        assert!(candidate
            .session_messages
            .iter()
            .any(|message| message.content == expected.first_input()));
    }
}

#[tokio::test]
async fn factory_stop_is_durable_and_retry_dispatches_only_the_new_generation() {
    let fixture = input_fixture();
    let original = InputProvider::new("must-never-be-called", false, false);
    let retried = InputProvider::new(PARENT_V2, false, false);
    let child = InputProvider::new(CHILD_V1, false, false);
    let mut pending = driver_plan(&fixture.parent, original.clone());
    pending.wait = Some(Arc::new(Notify::new()));
    let next = next_node(&fixture.parent);
    let factory = DriverFactory::new(vec![
        pending,
        driver_plan(&next, retried.clone()),
        driver_plan(&fixture.child, child.clone()),
    ]);
    let driver = fixture
        .controller
        .autonomous_turn_driver(driver_seeds(&fixture), factory.clone())
        .unwrap();
    let run = tokio::spawn(driver.run());
    tokio::time::timeout(Duration::from_secs(5), factory.started.notified())
        .await
        .unwrap();
    let first = factory.resolved.lock().unwrap()[0].clone();
    assert_eq!(original.calls.load(Ordering::SeqCst), 0);
    let stopped = driver_control(
        &fixture.controller,
        "driver-stop-factory",
        ControlParameters::StopActivation {
            activation: first.activation.clone(),
        },
    );
    assert_eq!(stopped.view().state, ControlCommandState::Settled);
    // No resource wait is released: Stop can acknowledge from durable canonical
    // and no-dispatch evidence without waiting for the factory to return.
    assert_eq!(original.calls.load(Ordering::SeqCst), 0);
    let mut retry = first.clone();
    retry.activation.activation_id = next.input.activation.activation_id.clone();
    retry.activation.generation += 1;
    retry.manifest_id = next.input.manifest_id.clone();
    let retry_receipt = driver_control(
        &fixture.controller,
        "driver-explicit-retry",
        ControlParameters::RetryActivation {
            activation: first.activation.clone(),
            input: Box::new(retry),
            replay_decisions: vec![],
        },
    );
    assert_eq!(retry_receipt.view().state, ControlCommandState::Settled);
    assert_eq!(
        retried.calls.load(Ordering::SeqCst),
        0,
        "a control receipt does not itself dispatch"
    );
    let outcome = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    assert_eq!(original.calls.load(Ordering::SeqCst), 0);
    assert_eq!(retried.calls.load(Ordering::SeqCst), 1);
    assert_eq!(child.calls.load(Ordering::SeqCst), 1);
    let generations: Vec<_> = outcome
        .snapshot
        .contract()
        .activations()
        .iter()
        .filter(|item| item.activation.node_id == first.activation.node_id)
        .map(|item| (item.activation.generation, item.state))
        .collect();
    assert_eq!(
        generations,
        vec![(1, ActivationState::Failed), (2, ActivationState::Accepted)]
    );
    assert_eq!(factory.resolved.lock().unwrap().len(), 3);
    let id = stopped.view().request.command_id.clone();
    assert_eq!(
        fixture
            .controller
            .control_command_receipt(&id)
            .unwrap()
            .unwrap()
            .view(),
        stopped.view()
    );
}

#[tokio::test]
async fn revised_parent_automatically_rebases_superseded_child_despite_old_bound_id() {
    let fixture = input_fixture();
    start_input(&fixture.controller, &fixture.parent);
    let parent_old = run_input(
        &fixture.controller,
        &fixture.parent,
        InputProvider::new(PARENT_V1, false, false),
    )
    .await;
    let mut child_old = fixture.child.clone();
    child_old.input.parents = vec![accepted_parent(&parent_old)];
    start_input(&fixture.controller, &child_old);
    let child_result = run_input(
        &fixture.controller,
        &child_old,
        InputProvider::new(CHILD_V1, false, false),
    )
    .await;
    assert!(fixture
        .controller
        .lock()
        .unwrap()
        .bound
        .contains_key(&child_result.activation.activation_id));
    let instruction = fixture
        .controller
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: "Revise the parent and propagate its accepted result".into(),
        })
        .unwrap();
    let mut revised = next_node(&fixture.parent);
    revised.input.guidance.push(instruction.clone());
    revised.input.revision_context = Some(RevisionContext {
        activation: parent_old.activation.clone(),
        output: parent_old.output.reference().clone(),
    });
    let receipt = driver_control(
        &fixture.controller,
        "driver-parent-revision",
        ControlParameters::ReviseActivation {
            activation: parent_old.activation.clone(),
            input: Box::new(revised.input.clone()),
            instruction: instruction.clone(),
            invalidate: vec![child_result.activation.clone()],
        },
    );
    assert_eq!(receipt.view().state, ControlCommandState::Settled);
    let parent_new = InputProvider::new(PARENT_V2, false, false);
    let child_new = InputProvider::new(CHILD_V2, false, false);
    let factory = DriverFactory::new(vec![
        driver_plan(&revised, parent_new.clone()),
        driver_plan(&next_node(&child_old), child_new.clone()),
    ]);
    let outcome = driven(
        fixture
            .controller
            .autonomous_turn_driver(driver_seeds(&fixture), factory)
            .unwrap(),
    )
    .await;
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    let accepted = outcome.snapshot.contract().current_accepted_activations();
    assert_eq!(accepted.len(), 2);
    assert!(accepted.iter().all(|item| item.activation.generation == 2));
    assert_projected_only(&parent_new, &[PARENT_V1, REQUEST], &[CHILD_V1]);
    assert_projected_only(&child_new, &[PARENT_V2, REQUEST], &[PARENT_V1, CHILD_V1]);
    let state = fixture.controller.lock().unwrap();
    assert!(state
        .bound
        .contains_key(&child_result.activation.activation_id));
    let rebases: Vec<_> = state
        .canonical
        .records()
        .unwrap()
        .iter()
        .filter_map(|event| match &event.event {
            TurnContractEvent::RebaseActivation { previous, input } => Some((previous, input)),
            _ => None,
        })
        .collect();
    assert_eq!(rebases.len(), 1);
    assert_eq!(rebases[0].0, &child_result.activation);
    assert_eq!(rebases[0].1.parents[0].activation.generation, 2);
    assert!(outcome.finalized.as_ref().unwrap().promotion().selected.iter()
        .all(|item| matches!(&item.accepted.source, CheckpointSource::Accepted { activation } if activation.generation == 2)));
}

#[tokio::test]
async fn paused_revision_reopens_one_epoch_rebases_descendants_and_reruns_required_review() {
    use crate::session_dispatch::{HumanControlAction, HumanControlActionRequest};
    // The required host review checks actual accepted generation identities.
    // This exercises condition freshness, not an external repository process.
    fn review(controller: &SessionDispatchController) -> ConditionOutcome {
        let snapshot = controller.snapshot().unwrap();
        let accepted = snapshot.contract().current_accepted_activations();
        assert_eq!(accepted.len(), 2);
        let activations: Vec<_> = accepted.iter().map(|item| item.activation.clone()).collect();
        let outcome = if activations.iter().all(|activation| activation.generation == 2) {
            ConditionOutcome::Passed
        } else { ConditionOutcome::Failed };
        let evidence = controller.retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: serde_json::json!({"criterion":"both accepted generations equal two", "observed":activations,"outcome":outcome}).to_string(),
        }).unwrap();
        apply_input_event(controller, TurnContractEvent::RecordCondition {
            epoch_id: snapshot.contract().epochs().last().unwrap().id.clone(),
            condition_id: ConditionId::new("both-results-reviewed").unwrap(), activations, outcome, evidence,
        });
        outcome
    }
    let fixture = input_fixture_with_review(true);
    start_input(&fixture.controller, &fixture.parent);
    let first_parent = run_input(&fixture.controller, &fixture.parent, InputProvider::new(PARENT_V1, false, false)).await;
    let mut first_child = fixture.child.clone();
    first_child.input.parents = vec![accepted_parent(&first_parent)];
    start_input(&fixture.controller, &first_child);
    let first_child_result = run_input(&fixture.controller, &first_child, InputProvider::new(CHILD_V1, false, false)).await;
    assert_eq!(review(&fixture.controller), ConditionOutcome::Failed);
    let paused = driven(fixture.controller.autonomous_turn_driver(driver_seeds(&fixture), DriverFactory::new(vec![])).unwrap()).await;
    assert_eq!(paused.snapshot.contract().state(), Some(LogicalTurnState::NeedsAttention));
    let view = fixture.controller.control_plane().unwrap();
    let target = view.nodes.iter().find(|node| node.node_id == first_parent.activation.node_id.as_str()).unwrap();
    assert!(target.activations[0].capabilities.revise.enabled, "{}", target.activations[0].capabilities.revise.reason);
    assert_eq!(target.activations[0].capabilities.revise_invalidates, vec![first_child_result.activation.clone()]);
    let request = HumanControlActionRequest {
        schema_version: 1, command_id: CommandId::new("paused-parent-revision").unwrap(),
        session_id: first_parent.activation.session_id.clone(), turn_id: first_parent.activation.turn_id.clone(),
        execution_epoch_id: paused.snapshot.contract().epochs().last().unwrap().id.clone(),
        expected_turn_revision: paused.snapshot.contract().revision(), expected_graph_revision: paused.snapshot.contract().graph().unwrap().revision,
        activation: Some(first_parent.activation.clone()), action: HumanControlAction::Revise,
        instruction: Some("Revise the result and repeat its dependent review".into()), include_previous_output: true,
        context: None, continuation: None, blocker_id: None, human_response: None,
        partial_finish: None,
    };
    // A process restart while paused retains the accepted frontier and the
    // failed review. Reopening a running successor would correctly interrupt
    // it; this test never treats disk state as a new execution owner.
    let InputFixture { _root, controller, parent, child } = fixture;
    let owner = controller.snapshot().unwrap().owner().clone();
    let turn = first_parent.activation.turn_id.clone();
    // Exercise actual graceful shutdown, not only dropping storage handles.
    // The old controller cannot admit anything after its lifecycle fence.
    let usage = controller.lock().unwrap().authority.usage("input-grant").unwrap();
    controller.close_registered_repository_admission().unwrap();
    assert!(controller.lock().unwrap().execution_admission().is_err());
    drop(controller);
    let canonical = SessionExecutionStore::open_existing(Arc::new(
        axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(_root.path()).unwrap()
    ), owner.clone()).unwrap();
    let controller = SessionDispatchController::open(canonical, turn.clone()).unwrap();
    assert_eq!(controller.snapshot().unwrap().contract().state(), Some(LogicalTurnState::NeedsAttention));
    assert_eq!(controller.lock().unwrap().authority.usage("input-grant").unwrap(), usage);
    let receipt = controller.submit_human_action(request.clone(), now_ms().unwrap()).unwrap();
    assert_eq!(receipt.state, ControlCommandState::Settled);
    let ControlParameters::ContinueTurn { plan, .. } = &receipt.request.parameters else { panic!("atomic revision continuation") };
    assert_ne!(plan.epoch_id, plan.source_epoch_id);
    assert_eq!(plan.condition_runs, vec![ConditionId::new("both-results-reviewed").unwrap()]);
    assert!(matches!(&plan.selections[0], ContinuationSelection::Revise { previous, invalidated_descendants, .. }
        if previous == &first_parent.activation && invalidated_descendants == &vec![first_child_result.activation.clone()]));
    let replacement = controller.snapshot().unwrap().contract().activations().last().unwrap().input.clone();
    assert_eq!(replacement.starting_savepoint, parent.input.starting_savepoint);
    assert_eq!(replacement.grant, parent.input.grant);
    assert_eq!(controller.submit_human_action(request.clone(), now_ms().unwrap()).unwrap(), receipt);
    let mut stale = request.clone();
    stale.command_id = CommandId::new("stale-paused-revision").unwrap();
    assert_eq!(controller.submit_human_action(stale, now_ms().unwrap()).unwrap().state, ControlCommandState::Rejected);
    let after = controller.snapshot().unwrap();
    assert_eq!(after.contract().epochs().len(), 2);
    assert!(after.contract().activations().iter().take(2).all(|item| item.state == ActivationState::Superseded));
    assert_eq!(after.contract().activations()[0].output.as_ref(), Some(first_parent.output.reference()));
    assert_eq!(after.contract().activations()[1].output.as_ref(), Some(first_child_result.output.reference()));
    let parent_new = InputProvider::new(PARENT_V2, false, false);
    let child_new = InputProvider::new(CHILD_V2, false, false);
    let factory = DriverFactory::new(vec![driver_plan(&next_node(&parent), parent_new.clone()), driver_plan(&next_node(&child), child_new.clone())]);
    let driver = controller.autonomous_turn_driver(vec![driver_seed(&parent), driver_seed(&child)], factory.clone()).unwrap();
    for expected in [&parent.input.activation.node_id, &child.input.activation.node_id] {
        let (ready, _) = driver.allocate_ready().unwrap();
        assert_eq!(ready.len(), 1, "expected {expected:?}; {:?}", controller.snapshot().unwrap().contract());
        assert_eq!(&ready[0].activation.node_id, expected);
        assert_eq!(ready[0].activation.execution_epoch_id, plan.epoch_id);
        let settled = controller.prepare_autonomous_activation(ready[0].activation.clone(), factory.resources(&ready[0]).await.unwrap()).unwrap().run().await.unwrap();
        assert!(settled.accepted, "{:?}", settled.failure);
    }
    assert_eq!(review(&controller), ConditionOutcome::Passed);
    let outcome = driven(driver).await;
    assert_eq!(outcome.snapshot.contract().state(), Some(LogicalTurnState::Completed));
    assert_eq!(outcome.snapshot.contract().conditions().len(), 2);
    let accepted = outcome.snapshot.contract().current_accepted_activations();
    let child_accepted = accepted.iter().find(|item| item.activation.node_id == child.input.activation.node_id).unwrap();
    assert_eq!(child_accepted.input.parents[0].activation.generation, 2);
    assert_projected_only(&child_new, &[PARENT_V2, REQUEST], &[PARENT_V1, CHILD_V1]);
    assert_eq!(factory.resolved.lock().unwrap().len(), 2);
    drop(controller);
    let canonical = SessionExecutionStore::open_existing(Arc::new(
        axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(_root.path()).unwrap()
    ), owner).unwrap();
    let reopened = SessionDispatchController::open(canonical, turn).unwrap();
    assert_eq!(reopened.snapshot().unwrap().contract().state(), Some(LogicalTurnState::Completed));
    assert_eq!(reopened.snapshot().unwrap().contract().epochs().len(), 2);
    assert_eq!(reopened.submit_human_action(request, now_ms().unwrap()).unwrap(), receipt);
}

#[test]
fn revision_continuation_requires_separate_revision_permission_for_every_invalidated_node() {
    use axocoatl_session::control_authority::{DelegationPolicy, DelegatedGraphLimits, DelegatedReplayPolicy};
    use axocoatl_session::control_command::{CommandReceiptView, CommandSourceRecord};
    let fixture = input_fixture();
    let state = fixture.controller.lock().unwrap();
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    let parent = &fixture.parent.input.activation;
    let child = &fixture.child.input.activation;
    let mut grant = state.authority.grant_policy("input-grant").unwrap();
    let nodes = vec![parent.node_id.clone(), child.node_id.clone()];
    grant.delegation = Some(Box::new(DelegationPolicy {
        schema_version: 1,
        scope: DelegationScope { session_id: parent.session_id.clone(), turn_id: parent.turn_id.clone(),
            task: snapshot.request_ref().unwrap().clone(), approved_graph: grant.issuer_evidence.clone() },
        operations: vec![DelegatedOperationPermission { operation: DelegatedOperation::ContinueTurn,
            targets: DelegatedTargetScope::Nodes { nodes: nodes.clone() } }],
        templates: vec![fixture.parent.input.definition.clone(), fixture.child.input.definition.clone()],
        resource_policy: grant.issuer_evidence.clone(), graph_limits: DelegatedGraphLimits { max_nodes: 2, max_edges: 1 },
        required_conditions: vec![], completion_criteria: vec![grant.issuer_evidence.clone()], machine_blockers: vec![],
        replay_policy: DelegatedReplayPolicy::RequireProvedEffectSafety,
    }));
    let mut next = next_node(&fixture.parent).input;
    next.activation.execution_epoch_id = ExecutionEpochId::new("scoped-revision-next").unwrap();
    let view = CommandReceiptView {
        request: ControlCommandRequest {
            schema_version: 1, command_id: CommandId::new("scoped-revision-test").unwrap(),
            session_id: parent.session_id.clone(), turn_id: parent.turn_id.clone(), execution_epoch_id: parent.execution_epoch_id.clone(),
            expected_turn_revision: snapshot.contract().revision(), expected_graph_revision: 1, issued_at_ms: now_ms().unwrap(),
            parameters: ControlParameters::ContinueTurn { plan: ContinuationPlan {
                source_epoch_id: parent.execution_epoch_id.clone(), epoch_id: next.activation.execution_epoch_id.clone(),
                selections: vec![ContinuationSelection::Revise { previous: parent.clone(), input: Box::new(next),
                    invalidated_descendants: vec![child.clone()], evidence: grant.issuer_evidence.clone() },
                    ContinuationSelection::AwaitDependencies { node_id: child.node_id.clone() }], condition_runs: vec![],
            }, replay_decisions: vec![] },
        },
        // This read-only target-permission test does not attest a live source,
        // dispatch any work, or grant the mutable policy below to an executor.
        source: CommandSourceRecord::Agent { activation: parent.clone(), grant_id: grant.id.clone(),
            grant_revision: grant.revision, grant_evidence: grant.issuer_evidence.clone(), live_scope: "permission-fixture-only".into() },
        state: ControlCommandState::Requested, revision: 1, last_transition: None,
    };
    assert!(state.validate_delegated_control_scope(&view, &grant).unwrap_err().to_string().contains("operation is outside"));
    grant.delegation.as_mut().unwrap().operations.push(DelegatedOperationPermission {
        operation: DelegatedOperation::ReviseActivation,
        targets: DelegatedTargetScope::Nodes { nodes: vec![parent.node_id.clone()] },
    });
    assert!(state.validate_delegated_control_scope(&view, &grant).unwrap_err().to_string().contains("outside its exact delegated subtree"));
    grant.delegation.as_mut().unwrap().operations.last_mut().unwrap().targets = DelegatedTargetScope::Nodes { nodes };
    state.validate_delegated_control_scope(&view, &grant).unwrap();
}

#[tokio::test]
async fn dropped_driver_drains_started_tool_and_never_promotes_interrupted_output() {
    let fixture = input_fixture();
    let provider = InputProvider::new("must-not-accept-after-drop", true, false);
    let child = InputProvider::new("must-not-start-child", false, false);
    let release = Arc::new(Notify::new());
    let tool = Arc::new(CountingTool {
        release: Some(release.clone()),
        ..Default::default()
    });
    let mut parent_plan = driver_plan(&fixture.parent, provider.clone());
    parent_plan.tool = Some(tool.clone());
    let factory = DriverFactory::new(vec![
        parent_plan,
        driver_plan(&fixture.child, child.clone()),
    ]);
    let driver = fixture
        .controller
        .autonomous_turn_driver(driver_seeds(&fixture), factory)
        .unwrap();
    let run = tokio::spawn(driver.run());
    tokio::time::timeout(Duration::from_secs(5), tool.started.notified())
        .await
        .unwrap();
    let invocation = fixture
        .controller
        .snapshot()
        .unwrap()
        .contract()
        .invocations()[0]
        .invocation_id
        .clone();
    run.abort();
    assert!(run.await.err().expect("aborted driver task").is_cancelled());
    let interrupted = fixture.controller.snapshot().unwrap();
    assert_eq!(
        interrupted.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert!(interrupted
        .contract()
        .current_accepted_activations()
        .is_empty());
    assert!(fixture
        .controller
        .lock()
        .unwrap()
        .audit
        .invocation(&invocation)
        .unwrap()
        .unwrap()
        .final_evidence
        .is_none());
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let settled = fixture
                .controller
                .lock()
                .unwrap()
                .audit
                .invocation(&invocation)
                .unwrap()
                .unwrap()
                .final_evidence
                .is_some();
            if settled {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("driver Drop must drain the actual in-flight tool outcome");
    assert_eq!(tool.count.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(child.calls.load(Ordering::SeqCst), 0);
    let snapshot = fixture.controller.snapshot().unwrap();
    assert_eq!(
        snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert!(snapshot
        .contract()
        .current_accepted_activations()
        .is_empty());
    assert!(fixture.controller.finalize_closed_turn().is_err());
    let state = fixture.controller.lock().unwrap();
    assert!(state
        .memory
        .committed_reference(&fixture.parent.input.conversation_id)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn driver_is_exclusive_and_rechecks_revision_before_quiescence() {
    let fixture = input_fixture();
    start_input(&fixture.controller, &fixture.parent);
    let failed = run_input(
        &fixture.controller,
        &fixture.parent,
        InputProvider::new(CHILD_FAILED, false, true),
    )
    .await;
    assert!(!failed.accepted);
    let next = next_node(&fixture.parent);
    let provider = InputProvider::new(PARENT_V2, false, false);
    let factory = DriverFactory::new(vec![
        driver_plan(&next, provider.clone()),
        driver_plan(&fixture.child, InputProvider::new(CHILD_V1, false, false)),
    ]);
    let mut driver = fixture
        .controller
        .autonomous_turn_driver(driver_seeds(&fixture), factory.clone())
        .unwrap();
    assert!(fixture
        .controller
        .autonomous_turn_driver(driver_seeds(&fixture), factory.clone())
        .is_err());
    let (allocated, inspected_revision) = driver.allocate_ready().unwrap();
    assert!(
        allocated.is_empty(),
        "failed parent and blocked child are quiescent"
    );
    let retry = driver_control(
        &fixture.controller,
        "driver-quiescence-retry",
        ControlParameters::RetryActivation {
            activation: failed.activation,
            input: Box::new(next.input),
            replay_decisions: vec![],
        },
    );
    assert_eq!(retry.view().state, ControlCommandState::Settled);
    assert!(driver
        .finish_quiescent(inspected_revision)
        .unwrap()
        .is_none());
    assert_eq!(
        fixture.controller.snapshot().unwrap().contract().state(),
        Some(LogicalTurnState::Running)
    );
    let outcome = driven(driver).await;
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    // Successful completion releases the exclusive token. Inspecting a closed
    // turn can recover its exact finalization without dispatching another actor.
    let repeat = driven(
        fixture
            .controller
            .autonomous_turn_driver(driver_seeds(&fixture), factory)
            .unwrap(),
    )
    .await;
    assert_eq!(
        repeat.finalized.unwrap().promotion(),
        outcome.finalized.as_ref().unwrap().promotion()
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

struct BranchFixture {
    _root: tempfile::TempDir,
    controller: SessionDispatchController,
    nodes: Vec<InputNode>,
}

fn branch_fixture() -> BranchFixture {
    let root = tempfile::tempdir().unwrap();
    let ownership = Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let owner = ExecutionStoreOwner {
        workspace_id: "input-workspace".into(),
        session_id: SessionId::new("input-session").unwrap(),
    };
    let mut canonical = SessionExecutionStore::open(ownership, owner.clone()).unwrap();
    let mut content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let turn_id = LogicalTurnId::new("input-turn").unwrap();
    let request = content
        .retain_request(ExecutionRequestContent {
            turn_id: turn_id.clone(),
            recorded_at_unix_ms: now_ms().unwrap(),
            display_input: REQUEST.into(),
            effective_input: REQUEST.into(),
            context: vec![],
            target_definition: None,
            model: None,
        })
        .unwrap();
    let limits = GrantLimits {
        activations: 16,
        invocations: 32,
        tokens: 10_000,
        cost_microunits: 0,
    };
    let budget = content
        .retain_activation_evidence(ActivationEvidenceContent::Budget {
            limits: limits.clone(),
        })
        .unwrap();
    let approval = content
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: "Host approved this bounded parent and child graph".into(),
        })
        .unwrap();
    let mut nodes = Vec::new();
    for name in ["parent", "child", "independent"] {
        let config = AgentConfig {
            id: AgentId::new(format!("{name}-conversation")),
            name: name.into(),
            provider: "controlled".into(),
            model: "controlled-model".into(),
            tools: vec!["effect".into()],
            ..Default::default()
        };
        let profile = ExecutionProfile {
            definition: name.into(),
            provider: config.provider.clone(),
            model: config.model.clone(),
            isolation: "in-process".into(),
            tools: config.tools.clone(),
        };
        let definition_id = AgentDefinitionId::new(name).unwrap();
        let definition = content
            .retain_activation_evidence(ActivationEvidenceContent::Definition {
                definition_id: definition_id.clone(),
                revision: 1,
                profile: profile.clone(),
                configuration: serde_json::to_string(&config).unwrap(),
            })
            .unwrap();
        let guidance = content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: format!("{name}-only-initial-guidance"),
            })
            .unwrap();
        nodes.push(InputNode {
            input: ActivationInputManifest {
                manifest_id: InputManifestId::new(format!("{name}-input-1")).unwrap(),
                activation: ActivationRef {
                    session_id: owner.session_id.clone(),
                    turn_id: turn_id.clone(),
                    execution_epoch_id: ExecutionEpochId::new("input-epoch").unwrap(),
                    node_id: TurnNodeId::new(name).unwrap(),
                    generation: 1,
                    activation_id: ActivationId::new(format!("{name}-activation-1")).unwrap(),
                },
                definition: DefinitionSnapshotRef {
                    definition_id,
                    snapshot: definition.reference().clone(),
                },
                conversation_id: NodeConversationId::new(config.id.0.clone()).unwrap(),
                starting_savepoint: ConversationSavepoint::Empty,
                parents: vec![],
                guidance: vec![request.reference().clone(), guidance.reference().clone()],
                attachments: vec![],
                repository: RepositoryInput::Unavailable,
                budget: budget.reference().clone(),
                grant: None,
                revision_context: None,
            },
            config,
            profile,
        });
    }
    let policy = AuthorityGrant {
        id: "input-grant".into(),
        revision: 1,
        issuer_evidence: approval.reference().clone(),
        holder: nodes[0].input.activation.node_id.clone(),
        descendants: vec![nodes[1].input.activation.node_id.clone()],
        allow_stop_descendants: false,
        delegation: None,
        conditions: vec![],
        profiles: nodes.iter().map(|node| node.profile.clone()).collect(),
        limits: limits.clone(),
        expires_at_ms: now_ms().unwrap() + 3_600_000,
    };
    let grant = content
        .retain_activation_evidence(ActivationEvidenceContent::Grant {
            policy: policy.clone(),
        })
        .unwrap();
    for node in &mut nodes[..2] {
        node.input.grant = Some(GrantSnapshotRef {
            grant_id: GrantId::new("input-grant").unwrap(),
            revision: 1,
            evidence: grant.reference().clone(),
        });
    }
    let independent_policy = AuthorityGrant {
        id: "independent-input-grant".into(),
        revision: 1,
        issuer_evidence: approval.reference().clone(),
        holder: nodes[2].input.activation.node_id.clone(),
        descendants: vec![],
        allow_stop_descendants: false,
        delegation: None,
        conditions: vec![],
        profiles: vec![nodes[2].profile.clone()],
        limits,
        expires_at_ms: policy.expires_at_ms,
    };
    let independent_grant = content
        .retain_activation_evidence(ActivationEvidenceContent::Grant {
            policy: independent_policy.clone(),
        })
        .unwrap();
    nodes[2].input.grant = Some(GrantSnapshotRef {
        grant_id: GrantId::new(&independent_policy.id).unwrap(),
        revision: 1,
        evidence: independent_grant.reference().clone(),
    });
    canonical
        .begin_with_request(
            TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("input-begin").unwrap(),
                expected_revision: 0,
                session_id: owner.session_id,
                turn_id: turn_id.clone(),
                event: TurnContractEvent::Begin {
                    epoch_id: nodes[0].input.activation.execution_epoch_id.clone(),
                    predecessor: None,
                    graph: TurnGraphSnapshot {
                        snapshot_id: GraphSnapshotId::new("input-graph").unwrap(),
                        revision: 1,
                        nodes: nodes
                            .iter()
                            .map(|node| GraphNode {
                                node_id: node.input.activation.node_id.clone(),
                                slot_id: SessionTeamSlotId::new(format!(
                                    "{}-slot",
                                    node.input.activation.node_id.as_str()
                                ))
                                .unwrap(),
                                definition: node.input.definition.clone(),
                                conversation_id: node.input.conversation_id.clone(),
                                starting_savepoint: ConversationSavepoint::Empty,
                                required: true,
                            })
                            .collect(),
                        dependencies: vec![DependencyEdge {
                            parent: nodes[0].input.activation.node_id.clone(),
                            child: nodes[1].input.activation.node_id.clone(),
                        }],
                        conditions: vec![],
                    },
                },
            },
            &request,
        )
        .unwrap();
    drop(content);
    let controller = SessionDispatchController::open(canonical, turn_id).unwrap();
    controller.install_grant(policy).unwrap();
    controller.install_grant(independent_policy).unwrap();
    BranchFixture {
        _root: root,
        controller,
        nodes,
    }
}

#[tokio::test]
async fn independent_branch_finishes_when_failed_parent_blocks_its_descendant() {
    let fixture = branch_fixture();
    let parent = InputProvider::new("failed-parent-draft", false, true);
    let child = InputProvider::new("blocked-child-must-not-run", false, false);
    let independent = InputProvider::new("independent-accepted-output", false, false);
    let factory = DriverFactory::new(vec![
        driver_plan(&fixture.nodes[0], parent.clone()),
        driver_plan(&fixture.nodes[1], child.clone()),
        driver_plan(&fixture.nodes[2], independent.clone()),
    ]);
    let seeds = fixture.nodes.iter().map(driver_seed).collect::<Vec<_>>();
    let outcome = driven(
        fixture
            .controller
            .autonomous_turn_driver(seeds.clone(), factory.clone())
            .unwrap(),
    )
    .await;
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert!(outcome.finalized.is_none());
    assert_eq!(parent.calls.load(Ordering::SeqCst), 1);
    assert_eq!(child.calls.load(Ordering::SeqCst), 0);
    assert_eq!(independent.calls.load(Ordering::SeqCst), 1);
    let activations = outcome.snapshot.contract().activations();
    assert_eq!(activations.len(), 2);
    assert!(activations.iter().any(|item| item.activation.node_id
        == fixture.nodes[0].input.activation.node_id
        && item.state == ActivationState::Failed));
    let accepted = outcome.snapshot.contract().current_accepted_activations();
    assert_eq!(accepted.len(), 1);
    assert_eq!(
        accepted[0].activation.node_id,
        fixture.nodes[2].input.activation.node_id
    );
    assert_projected_only(&independent, &[REQUEST], &["failed-parent-draft"]);
    // Merely reconstructing a driver at this boundary cannot retry the failed
    // branch, materialize its blocked child, or replay the accepted branch.
    let repeat = driven(
        fixture
            .controller
            .autonomous_turn_driver(seeds, factory.clone())
            .unwrap(),
    )
    .await;
    assert_eq!(
        repeat.snapshot.contract().revision(),
        outcome.snapshot.contract().revision()
    );
    assert!(repeat.finalized.is_none());
    assert_eq!(factory.resolved.lock().unwrap().len(), 2);
    assert!(fixture
        .controller
        .lock()
        .unwrap()
        .memory
        .committed_reference(&fixture.nodes[2].input.conversation_id)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn dropped_factory_wait_requires_explicit_continue_and_preserves_proven_zero_dispatch() {
    let fixture = input_fixture();
    let original = InputProvider::new("no-dispatch-before-drop", false, false);
    let mut pending = driver_plan(&fixture.parent, original.clone());
    pending.wait = Some(Arc::new(Notify::new()));
    let factory = DriverFactory::new(vec![pending]);
    let run = tokio::spawn(
        fixture
            .controller
            .autonomous_turn_driver(driver_seeds(&fixture), factory.clone())
            .unwrap()
            .run(),
    );
    tokio::time::timeout(Duration::from_secs(5), factory.started.notified())
        .await
        .unwrap();
    let original_input = factory.resolved.lock().unwrap()[0].clone();
    run.abort();
    assert!(run.await.err().expect("aborted driver task").is_cancelled());
    assert_eq!(original.calls.load(Ordering::SeqCst), 0);
    let interrupted = fixture.controller.snapshot().unwrap();
    assert_eq!(
        interrupted.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert_eq!(
        interrupted.contract().activations()[0].state,
        ActivationState::Interrupted
    );
    let revision = interrupted.contract().revision();
    let passive = driven(
        fixture
            .controller
            .autonomous_turn_driver(driver_seeds(&fixture), factory)
            .unwrap(),
    )
    .await;
    assert_eq!(passive.snapshot.contract().revision(), revision);
    assert!(passive.finalized.is_none());
    let mut next = next_node(&fixture.parent);
    next.input = original_input.clone();
    next.input.manifest_id = InputManifestId::new("continued-driver-input").unwrap();
    next.input.activation.activation_id = ActivationId::new("continued-driver-activation").unwrap();
    next.input.activation.generation = 2;
    next.input.activation.execution_epoch_id =
        ExecutionEpochId::new("continued-driver-epoch").unwrap();
    let continuation = driver_control(
        &fixture.controller,
        "explicit-driver-continue",
        ControlParameters::ContinueTurn {
            plan: ContinuationPlan {
                source_epoch_id: original_input.activation.execution_epoch_id.clone(),
                epoch_id: next.input.activation.execution_epoch_id.clone(),
                selections: vec![
                    ContinuationSelection::Retry {
                        previous: original_input.activation.clone(),
                        input: Box::new(next.input.clone()),
                    },
                    ContinuationSelection::AwaitDependencies {
                        node_id: fixture.child.input.activation.node_id.clone(),
                    },
                ],
                condition_runs: vec![],
            },
            replay_decisions: vec![],
        },
    );
    assert_eq!(continuation.view().state, ControlCommandState::Settled);
    let retried = InputProvider::new(PARENT_V2, false, false);
    let child = InputProvider::new(CHILD_V1, false, false);
    let factory = DriverFactory::new(vec![
        driver_plan(&next, retried.clone()),
        driver_plan(&fixture.child, child.clone()),
    ]);
    let outcome = driven(
        fixture
            .controller
            .autonomous_turn_driver(driver_seeds(&fixture), factory)
            .unwrap(),
    )
    .await;
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    assert_eq!(original.calls.load(Ordering::SeqCst), 0);
    assert_eq!(retried.calls.load(Ordering::SeqCst), 1);
    assert_eq!(child.calls.load(Ordering::SeqCst), 1);
    let state = fixture.controller.lock().unwrap();
    let usage = state
        .authority
        .provider_usage(&original_input.activation)
        .unwrap();
    assert_eq!(usage.tokens.usage.input_tokens, 0);
    assert_eq!(usage.tokens.usage.output_tokens, 0);
    assert!(usage.tokens.complete);
    assert_eq!(usage.cost_microunits, 0);
    assert!(usage.cost_known);
}

fn separate_root_grants(fixture: &mut BranchFixture, refuse_at_provider_claim: bool) {
    for index in [0, 2] {
        let node = &mut fixture.nodes[index];
        let limits = GrantLimits {
            activations: 8,
            invocations: 16,
            tokens: if index == 0 && refuse_at_provider_claim {
                50
            } else {
                10_000
            },
            cost_microunits: 0,
        };
        let approval = fixture
            .controller
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: format!(
                    "Independent bounded root grant for {}",
                    node.input.activation.node_id.as_str()
                ),
            })
            .unwrap();
        let policy = AuthorityGrant {
            id: format!("separate-{}", node.input.activation.node_id.as_str()),
            revision: 1,
            issuer_evidence: approval,
            holder: node.input.activation.node_id.clone(),
            descendants: vec![],
            allow_stop_descendants: false,
            delegation: None,
            conditions: vec![],
            profiles: vec![node.profile.clone()],
            limits: limits.clone(),
            expires_at_ms: now_ms().unwrap() + 3_600_000,
        };
        node.input.budget = fixture
            .controller
            .retain_activation_evidence(ActivationEvidenceContent::Budget { limits })
            .unwrap();
        let evidence = fixture
            .controller
            .retain_activation_evidence(ActivationEvidenceContent::Grant {
                policy: policy.clone(),
            })
            .unwrap();
        node.input.grant = Some(GrantSnapshotRef {
            grant_id: GrantId::new(policy.id.clone()).unwrap(),
            revision: policy.revision,
            evidence,
        });
        fixture.controller.install_grant(policy).unwrap();
    }
    if !refuse_at_provider_claim {
        let state = fixture.controller.lock().unwrap();
        state
            .authority
            .revoke_grant("separate-parent", state.authority.revision().unwrap())
            .unwrap();
    }
}

#[tokio::test]
async fn one_roots_revoked_grant_or_call_budget_refusal_does_not_cancel_an_independent_root() {
    for refuse_at_provider_claim in [false, true] {
        let mut fixture = branch_fixture();
        separate_root_grants(&mut fixture, refuse_at_provider_claim);
        let refused = InputProvider::new("refused-root-must-not-dispatch", false, false);
        let child = InputProvider::new("refused-descendant-must-not-dispatch", false, false);
        let independent = InputProvider::new("independent-survives-refusal", false, false);
        let factory = DriverFactory::new(vec![
            driver_plan(&fixture.nodes[0], refused.clone()),
            driver_plan(&fixture.nodes[1], child.clone()),
            driver_plan(&fixture.nodes[2], independent.clone()),
        ]);
        let outcome = driven(
            fixture
                .controller
                .autonomous_turn_driver(fixture.nodes.iter().map(driver_seed).collect(), factory)
                .unwrap(),
        )
        .await;
        assert_eq!(
            outcome.snapshot.contract().state(),
            Some(LogicalTurnState::NeedsAttention)
        );
        assert!(outcome.finalized.is_none());
        assert_eq!(refused.calls.load(Ordering::SeqCst), 0);
        assert_eq!(child.calls.load(Ordering::SeqCst), 0);
        assert_eq!(independent.calls.load(Ordering::SeqCst), 1);
        let failed = outcome
            .snapshot
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation.node_id == fixture.nodes[0].input.activation.node_id)
            .unwrap();
        assert_eq!(failed.state, ActivationState::Failed);
        let accepted = outcome.snapshot.contract().current_accepted_activations();
        assert_eq!(accepted.len(), 1);
        assert_eq!(
            accepted[0].activation.node_id,
            fixture.nodes[2].input.activation.node_id
        );
        let state = fixture.controller.lock().unwrap();
        let usage = state.authority.provider_usage(&failed.activation).unwrap();
        assert_eq!(usage.calls, 0);
        assert_eq!(usage.unsettled_calls, 0);
        assert_eq!(usage.tokens.usage, TokenUsageStats::default());
        assert!(usage.tokens.complete);
        assert!(usage.cost_known);
        assert_eq!(usage.cost_microunits, 0);
    }
}

#[tokio::test]
async fn normal_finish_waits_through_parent_completion_and_child_scheduling_until_promotion() {
    let fixture = input_fixture();
    let parent = InputProvider::new(PARENT_V1, false, false);
    let child = InputProvider::new(CHILD_V1, false, false);
    let release_parent = Arc::new(Notify::new());
    let release_child = Arc::new(Notify::new());
    let mut parent_plan = driver_plan(&fixture.parent, parent.clone());
    parent_plan.wait = Some(release_parent.clone());
    let mut child_plan = driver_plan(&fixture.child, child.clone());
    child_plan.wait = Some(release_child.clone());
    let factory = DriverFactory::new(vec![parent_plan, child_plan]);
    let driver = fixture
        .controller
        .autonomous_turn_driver(driver_seeds(&fixture), factory.clone())
        .unwrap();
    let run = tokio::spawn(driver.run());
    tokio::time::timeout(Duration::from_secs(5), factory.started.notified())
        .await
        .unwrap();
    let finish = driver_control(
        &fixture.controller,
        "driver-finish-pending-graph",
        ControlParameters::FinishTurn {
            mode: axocoatl_session::control_command::FinishMode::Normal,
        },
    );
    assert_eq!(finish.view().state, ControlCommandState::Accepted);
    assert_eq!(parent.calls.load(Ordering::SeqCst), 0);
    assert_eq!(child.calls.load(Ordering::SeqCst), 0);
    release_parent.notify_one();
    tokio::time::timeout(Duration::from_secs(5), factory.started.notified())
        .await
        .unwrap();
    let midway = fixture.controller.snapshot().unwrap();
    assert_eq!(midway.contract().state(), Some(LogicalTurnState::Running));
    assert_eq!(midway.contract().current_accepted_activations().len(), 1);
    assert_eq!(midway.contract().activations().len(), 2);
    assert!(midway
        .contract()
        .activations()
        .iter()
        .any(
            |item| item.activation.node_id == fixture.child.input.activation.node_id
                && item.state == ActivationState::Running
        ));
    assert_eq!(
        fixture
            .controller
            .control_command_receipt(&finish.view().request.command_id)
            .unwrap()
            .unwrap()
            .view()
            .state,
        ControlCommandState::Accepted
    );
    assert!(fixture
        .controller
        .lock()
        .unwrap()
        .memory
        .committed_reference(&fixture.parent.input.conversation_id)
        .unwrap()
        .is_none());
    release_child.notify_one();
    let outcome = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    assert_eq!(
        outcome
            .finalized
            .as_ref()
            .unwrap()
            .promotion()
            .selected
            .len(),
        2
    );
    assert_eq!(parent.calls.load(Ordering::SeqCst), 1);
    assert_eq!(child.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture
            .controller
            .control_command_receipt(&finish.view().request.command_id)
            .unwrap()
            .unwrap()
            .view()
            .state,
        ControlCommandState::Settled
    );
    let finalized = fixture.controller.finalize_closed_turn().unwrap();
    assert_eq!(
        finalized.promotion(),
        outcome.finalized.as_ref().unwrap().promotion()
    );
}

#[tokio::test]
async fn parent_revision_rebases_an_unstarted_child_revision_without_inventing_missing_usage() {
    let fixture = input_fixture();
    start_input(&fixture.controller, &fixture.parent);
    let original_parent = run_input(
        &fixture.controller,
        &fixture.parent,
        InputProvider::new(PARENT_V1, false, false),
    )
    .await;
    let mut child = fixture.child.clone();
    child.input.parents = vec![accepted_parent(&original_parent)];
    start_input(&fixture.controller, &child);
    let original_child = run_input(
        &fixture.controller,
        &child,
        InputProvider::new(CHILD_V1, false, false),
    )
    .await;
    let child_instruction = fixture
        .controller
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: "Retain this child revision instruction across parent rebase".into(),
        })
        .unwrap();
    let mut child_prepared = next_node(&child);
    child_prepared
        .input
        .guidance
        .push(child_instruction.clone());
    child_prepared.input.revision_context = Some(RevisionContext {
        activation: original_child.activation.clone(),
        output: original_child.output.reference().clone(),
    });
    let child_revision = driver_control(
        &fixture.controller,
        "prepare-child-revision-before-parent",
        ControlParameters::ReviseActivation {
            activation: original_child.activation.clone(),
            input: Box::new(child_prepared.input.clone()),
            instruction: child_instruction,
            invalidate: vec![],
        },
    );
    assert_eq!(child_revision.view().state, ControlCommandState::Settled);
    assert_eq!(
        fixture
            .controller
            .snapshot()
            .unwrap()
            .contract()
            .activations()
            .last()
            .unwrap()
            .state,
        ActivationState::Unstarted
    );
    let parent_instruction = fixture
        .controller
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: "Revise parent before the prepared child starts".into(),
        })
        .unwrap();
    let mut parent_revised = next_node(&fixture.parent);
    parent_revised
        .input
        .guidance
        .push(parent_instruction.clone());
    parent_revised.input.revision_context = Some(RevisionContext {
        activation: original_parent.activation.clone(),
        output: original_parent.output.reference().clone(),
    });
    let parent_revision = driver_control(
        &fixture.controller,
        "invalidate-unstarted-child-revision",
        ControlParameters::ReviseActivation {
            activation: original_parent.activation,
            input: Box::new(parent_revised.input.clone()),
            instruction: parent_instruction,
            invalidate: vec![child_prepared.input.activation.clone()],
        },
    );
    assert_eq!(parent_revision.view().state, ControlCommandState::Settled);
    {
        let state = fixture.controller.lock().unwrap();
        assert!(
            state
                .authority
                .provider_usage(&child_prepared.input.activation)
                .is_err(),
            "a never-started prepared generation has no fabricated authority record"
        );
        assert!(!state
            .canonical
            .records()
            .unwrap()
            .iter()
            .any(|record| match &record.event {
                TurnContractEvent::StartActivation { input } =>
                    input.activation == child_prepared.input.activation,
                TurnContractEvent::StartPreparedActivation { activation } =>
                    activation == &child_prepared.input.activation,
                _ => false,
            }));
    }
    let new_parent = InputProvider::new(PARENT_V2, false, false);
    let rebased_child = InputProvider::new(CHILD_V2, false, false);
    let factory = DriverFactory::new(vec![
        driver_plan(&parent_revised, new_parent.clone()),
        driver_plan(&next_node(&child_prepared), rebased_child.clone()),
    ]);
    let outcome = driven(
        fixture
            .controller
            .autonomous_turn_driver(driver_seeds(&fixture), factory.clone())
            .unwrap(),
    )
    .await;
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    let accepted = outcome.snapshot.contract().current_accepted_activations();
    let final_child = accepted
        .iter()
        .find(|item| item.activation.node_id == child.input.activation.node_id)
        .unwrap();
    assert_eq!(final_child.activation.generation, 3);
    assert_eq!(final_child.input.parents[0].activation.generation, 2);
    assert!(final_child.input.revision_context.is_none());
    assert_projected_only(
        &rebased_child,
        &[
            PARENT_V2,
            "Retain this child revision instruction across parent rebase",
        ],
        &[PARENT_V1, CHILD_V1],
    );
    assert_eq!(factory.resolved.lock().unwrap().len(), 2);
    assert_eq!(new_parent.calls.load(Ordering::SeqCst), 1);
    assert_eq!(rebased_child.calls.load(Ordering::SeqCst), 1);
    let state = fixture.controller.lock().unwrap();
    let checkpoint = state
        .memory
        .checkpoint(final_child.checkpoint.as_ref().unwrap())
        .unwrap();
    assert_eq!(
        checkpoint.cumulative_token_usage,
        TokenUsageStats::new(20, 4)
    );
    assert!(checkpoint.cumulative_token_usage_known);
    assert_eq!(
        outcome
            .finalized
            .as_ref()
            .unwrap()
            .promotion()
            .selected
            .len(),
        2
    );
}

#[path = "session_dispatch_turn_stop_driver_tests.rs"]
mod turn_stop_driver_tests;

#[path = "session_dispatch_partial_finish_tests.rs"]
mod partial_finish_tests;
