use super::*;
use crate::session_dispatch::{HumanControlAction, HumanControlActionRequest};

fn partial_request(
    controller: &SessionDispatchController,
    id: &str,
    selected: Vec<ActivationRef>,
) -> HumanControlActionRequest {
    let snapshot = controller.snapshot().unwrap();
    let controls = controller.control_plane().unwrap().turn_controls.unwrap();
    assert!(
        controls.partial_finish.capability.enabled,
        "{}",
        controls.partial_finish.capability.reason
    );
    let mut review = controls.partial_finish.review;
    review.selected_activations = selected;
    HumanControlActionRequest {
        schema_version: 1,
        command_id: CommandId::new(id).unwrap(),
        session_id: snapshot.owner().session_id.clone(),
        turn_id: snapshot.turn_id().clone(),
        execution_epoch_id: snapshot.contract().epochs().last().unwrap().id.clone(),
        expected_turn_revision: snapshot.contract().revision(),
        expected_graph_revision: snapshot.contract().graph().unwrap().revision,
        activation: None,
        action: HumanControlAction::Finish,
        instruction: None,
        include_previous_output: false,
        context: None,
        continuation: None,
        blocker_id: None,
        human_response: None,
        partial_finish: Some(review),
    }
}

#[tokio::test]
async fn partial_finish_stops_exact_factory_wait_and_durably_skips_unrun_child() {
    let fixture = input_fixture_with_review(true);
    let parent = InputProvider::new(PARENT_V1, false, false);
    let child = InputProvider::new(CHILD_V1, false, false);
    let wait = Arc::new(Notify::new());
    let mut plan = driver_plan(&fixture.parent, parent.clone());
    plan.wait = Some(wait);
    let factory = DriverFactory::new(vec![plan, driver_plan(&fixture.child, child.clone())]);
    let run = tokio::spawn(
        fixture
            .controller
            .autonomous_turn_driver(driver_seeds(&fixture), factory.clone())
            .unwrap()
            .run(),
    );
    tokio::time::timeout(Duration::from_secs(3), factory.started.notified())
        .await
        .unwrap();
    let mut ordinary = partial_request(&fixture.controller, "normal-waits-for-running", vec![]);
    ordinary.partial_finish = None;
    let ordinary_receipt = fixture
        .controller
        .submit_human_action(ordinary, now_ms().unwrap())
        .unwrap();
    assert_eq!(ordinary_receipt.state, ControlCommandState::Accepted);
    let request = partial_request(&fixture.controller, "confirmed-partial-wait", vec![]);
    assert_eq!(
        request
            .partial_finish
            .as_ref()
            .unwrap()
            .stop_activations
            .len(),
        1
    );
    assert_eq!(
        request.partial_finish.as_ref().unwrap().unrun_nodes,
        vec![fixture.child.input.activation.node_id.clone()]
    );
    let receipt = fixture
        .controller
        .submit_human_action(request.clone(), now_ms().unwrap())
        .unwrap();
    assert_eq!(
        receipt.state,
        ControlCommandState::Applied,
        "the recorded closing request is not safe settlement"
    );
    let outcome = tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Finished)
    );
    let intent = outcome.snapshot.contract().stop_requested().unwrap();
    assert_eq!(intent.closure, TurnClosure::Finished);
    assert_eq!(
        intent
            .partial_finish
            .as_ref()
            .unwrap()
            .missing_condition_ids,
        vec![ConditionId::new("both-results-reviewed").unwrap()]
    );
    assert_eq!(
        intent.unrun_nodes,
        vec![fixture.child.input.activation.node_id.clone()]
    );
    assert_eq!(outcome.snapshot.contract().activations().len(), 1);
    assert!(outcome.finalized.unwrap().promotion().selected.is_empty());
    assert_eq!(parent.calls.load(Ordering::SeqCst), 0);
    assert_eq!(child.calls.load(Ordering::SeqCst), 0);
    let settled = fixture
        .controller
        .submit_human_action(request.clone(), now_ms().unwrap())
        .unwrap();
    assert_eq!(settled.state, ControlCommandState::Settled);
    assert_eq!(
        fixture
            .controller
            .control_command_receipt(&ordinary_receipt.request.command_id)
            .unwrap()
            .unwrap()
            .view()
            .state,
        ControlCommandState::Failed
    );
    assert_eq!(
        fixture
            .controller
            .submit_human_action(request, now_ms().unwrap())
            .unwrap(),
        settled
    );
}

#[tokio::test]
async fn partial_finish_keeps_selected_sink_only_reopens_receipt_and_seeds_successor_context() {
    let fixture = input_fixture_with_review(true);
    let parent = InputProvider::new(PARENT_V1, false, false);
    let child = InputProvider::new(CHILD_V1, false, false);
    let outcome = driven(
        fixture
            .controller
            .autonomous_turn_driver(
                driver_seeds(&fixture),
                DriverFactory::new(vec![
                    driver_plan(&fixture.parent, parent),
                    driver_plan(&fixture.child, child),
                ]),
            )
            .unwrap(),
    )
    .await;
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    let accepted = outcome.snapshot.contract().current_accepted_activations();
    let selected = accepted
        .iter()
        .find(|item| item.activation.node_id == fixture.child.input.activation.node_id)
        .unwrap()
        .activation
        .clone();
    let request = partial_request(
        &fixture.controller,
        "confirmed-partial-selection",
        vec![selected.clone()],
    );
    let mut ordinary = request.clone();
    ordinary.command_id = CommandId::new("normal-cannot-bypass").unwrap();
    ordinary.partial_finish = None;
    assert_eq!(
        fixture
            .controller
            .submit_human_action(ordinary, now_ms().unwrap())
            .unwrap()
            .state,
        ControlCommandState::Rejected
    );
    let mut stale = request.clone();
    stale.command_id = CommandId::new("stale-partial-review").unwrap();
    stale.expected_turn_revision -= 1;
    assert_eq!(
        fixture
            .controller
            .submit_human_action(stale, now_ms().unwrap())
            .unwrap()
            .state,
        ControlCommandState::Rejected
    );
    let mut omitted = request.clone();
    omitted.command_id = CommandId::new("missing-check-omission").unwrap();
    omitted
        .partial_finish
        .as_mut()
        .unwrap()
        .missing_conditions
        .clear();
    assert!(fixture
        .controller
        .submit_human_action(omitted, now_ms().unwrap())
        .is_err());
    let mut parent_selected = request.clone();
    parent_selected.command_id = CommandId::new("non-sink-selection").unwrap();
    parent_selected
        .partial_finish
        .as_mut()
        .unwrap()
        .selected_activations = vec![accepted[0].activation.clone()];
    assert_eq!(
        fixture
            .controller
            .submit_human_action(parent_selected, now_ms().unwrap())
            .unwrap()
            .state,
        ControlCommandState::Rejected
    );
    // Canonical replay independently refuses omitted/extra condition IDs and
    // omitted evidence, even if a host serializer were to supply such an event.
    let condition = outcome.snapshot.contract().graph().unwrap().conditions[0].clone();
    let ConditionKind::Review { criterion } = condition.kind else {
        unreachable!()
    };
    for (ids, evidence) in [
        (vec![], vec![criterion.clone()]),
        (
            vec![ConditionId::new("foreign").unwrap()],
            vec![criterion.clone()],
        ),
        (vec![condition.condition_id.clone()], vec![]),
    ] {
        let mut contract = outcome.snapshot.contract().clone();
        assert!(contract
            .apply(&TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("forged-partial").unwrap(),
                expected_revision: contract.revision(),
                session_id: outcome.snapshot.owner().session_id.clone(),
                turn_id: outcome.snapshot.turn_id().clone(),
                event: TurnContractEvent::RequestPartialFinish {
                    approval: criterion.clone(),
                    selected_activations: vec![selected.clone()],
                    stop_activations: vec![],
                    missing_conditions: evidence,
                    missing_condition_ids: ids
                }
            })
            .is_err());
    }
    let receipt = fixture
        .controller
        .submit_human_action(request.clone(), now_ms().unwrap())
        .unwrap();
    assert_eq!(receipt.state, ControlCommandState::Settled);
    let finalized = fixture.controller.finalize_closed_turn().unwrap();
    assert_eq!(
        finalized.snapshot().contract().state(),
        Some(LogicalTurnState::Finished)
    );
    assert_eq!(
        finalized
            .snapshot()
            .contract()
            .current_accepted_activations()
            .len(),
        2,
        "raw acceptance is immutable"
    );
    assert_eq!(finalized.promotion().selected.len(), 1);
    assert_eq!(finalized.promotion().selected[0].node_id, selected.node_id);
    let usage = fixture
        .controller
        .activation_provider_usage(&selected)
        .unwrap();
    assert_eq!(usage.tokens.usage.total(), 12);
    let InputFixture {
        _root,
        controller,
        parent,
        child,
    } = fixture;
    let owner = controller.snapshot().unwrap().owner().clone();
    let turn = selected.turn_id.clone();
    drop(controller);
    let canonical = SessionExecutionStore::open_existing(
        Arc::new(
            axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(_root.path())
                .unwrap(),
        ),
        owner,
    )
    .unwrap();
    let controller = SessionDispatchController::open(canonical, turn).unwrap();
    assert_eq!(
        controller
            .submit_human_action(request, now_ms().unwrap())
            .unwrap(),
        receipt
    );
    let mut graph = controller
        .snapshot()
        .unwrap()
        .contract()
        .graph()
        .unwrap()
        .clone();
    graph.snapshot_id = GraphSnapshotId::new("partial-successor-graph").unwrap();
    graph.conditions.clear();
    let grant = {
        let state = controller.lock().unwrap();
        for node in &mut graph.nodes {
            node.starting_savepoint = state
                .memory
                .committed_reference(&node.conversation_id)
                .unwrap()
                .map_or(ConversationSavepoint::Empty, |checkpoint| {
                    ConversationSavepoint::Checkpoint {
                        checkpoint: Box::new(checkpoint),
                    }
                });
        }
        assert_eq!(
            graph.nodes[0].starting_savepoint,
            ConversationSavepoint::Empty
        );
        assert!(matches!(
            graph.nodes[1].starting_savepoint,
            ConversationSavepoint::Checkpoint { .. }
        ));
        state.authority.grant_policy("input-grant").unwrap()
    };
    let next = LogicalTurnId::new("partial-next-turn").unwrap();
    let controller = controller
        .begin_successor(SuccessorTurn {
            command_id: CommandId::new("partial-next-begin").unwrap(),
            turn_id: next.clone(),
            epoch_id: ExecutionEpochId::new("partial-next-epoch").unwrap(),
            graph,
            request: ExecutionRequestContent {
                turn_id: next,
                recorded_at_unix_ms: now_ms().unwrap(),
                display_input: "Continue selected result".into(),
                effective_input: "Continue selected result".into(),
                context: vec![],
                target_definition: None,
                model: None,
            },
        })
        .unwrap();
    controller.install_grant(grant).unwrap();
    let next_parent = InputProvider::new(PARENT_V2, false, false);
    let next_child = InputProvider::new(CHILD_V2, false, false);
    let mut seeds = vec![driver_seed(&parent), driver_seed(&child)];
    for seed in &mut seeds {
        seed.guidance[0] = controller
            .snapshot()
            .unwrap()
            .request_ref()
            .unwrap()
            .clone();
    }
    let outcome = driven(
        controller
            .autonomous_turn_driver(
                seeds,
                DriverFactory::new(vec![
                    driver_plan(&parent, next_parent.clone()),
                    driver_plan(&child, next_child.clone()),
                ]),
            )
            .unwrap(),
    )
    .await;
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    let text = |provider: &InputProvider| {
        serde_json::to_string(&*provider.requests.lock().unwrap()).unwrap()
    };
    assert!(!text(&next_parent).contains(PARENT_V1));
    assert!(text(&next_child).contains(CHILD_V1));
    // The selected child checkpoint legitimately retains its original parent
    // evidence. The unselected parent slot itself has no promoted head.
}

#[test]
fn historical_stop_intent_serialization_preserves_existing_promotion_digest_input() {
    let intent = TurnStopIntent {
        command_id: CommandId::new("stop").unwrap(),
        requested_revision: 3,
        evidence: EvidenceRef::new("request").unwrap(),
        unrun_nodes: vec![TurnNodeId::new("unrun").unwrap()],
        closure: TurnClosure::Cancelled,
        partial_finish: None,
    };
    assert_eq!(
        serde_json::to_string(&intent).unwrap(),
        r#"{"command_id":"stop","requested_revision":3,"evidence":"request","unrun_nodes":["unrun"]}"#
    );
}
