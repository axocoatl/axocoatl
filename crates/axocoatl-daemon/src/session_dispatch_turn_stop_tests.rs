use super::*;

fn stop(controller: &SessionDispatchController) -> TurnStopReceipt {
    let snapshot = controller.snapshot().unwrap();
    controller.request_human_turn_stop(snapshot.owner().session_id.as_str(), snapshot.turn_id().as_str()).unwrap()
}

#[tokio::test]
async fn whole_stop_acknowledges_before_provider_settlement_and_repeats_exactly() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Pending, true));
    let prepared = fixture.controller.prepare_autonomous_activation(fixture.activation.clone(),
        resources(&fixture, provider.clone(), Arc::new(CountingTool::default()))).unwrap();
    let run = tokio::spawn(prepared.run());
    tokio::time::timeout(Duration::from_secs(3), provider.started.notified()).await.unwrap();
    let first = stop(&fixture.controller);
    assert!(first.first_request());
    assert!(first.settled().is_none());
    assert!(matches!(first.accepted().envelope().event, TurnContractEvent::RequestTurnStop { .. }));
    let again = stop(&fixture.controller);
    assert!(!again.first_request());
    assert_eq!(first.accepted().envelope(), again.accepted().envelope());
    assert!(!tokio::time::timeout(Duration::from_secs(3), run).await.unwrap().unwrap().unwrap().accepted);
    let final_receipt = stop(&fixture.controller);
    assert!(final_receipt.settled().is_some());
    assert_eq!(fixture.controller.snapshot().unwrap().contract().state(), Some(LogicalTurnState::Cancelled));
    assert!(final_receipt.settled().unwrap().promotion().selected.is_empty());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let usage = fixture.controller.activation_provider_usage(&fixture.activation).unwrap();
    assert!(!usage.tokens.complete, "cancelled call is unknown, not measured zero");
    assert!(fixture.controller.prepare_autonomous_activation(fixture.activation.clone(),
        resources(&fixture, provider, Arc::new(CountingTool::default()))).is_err());
}

#[tokio::test]
async fn whole_stop_preserves_an_inflight_tool_until_actual_outcome_and_keeps_charges() {
    let fixture = run_fixture();
    let release = Arc::new(tokio::sync::Notify::new());
    let tool = Arc::new(CountingTool { release: Some(release.clone()), ..Default::default() });
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let prepared = fixture.controller.prepare_autonomous_activation(fixture.activation.clone(),
        resources(&fixture, provider.clone(), tool.clone())).unwrap();
    let run = tokio::spawn(prepared.run());
    tokio::time::timeout(Duration::from_secs(3), tool.started.notified()).await.unwrap();
    let first = stop(&fixture.controller);
    assert!(first.settled().is_none());
    assert_eq!(fixture.controller.snapshot().unwrap().contract().state(), Some(LogicalTurnState::Running));
    release.notify_one();
    let settled = tokio::time::timeout(Duration::from_secs(3), run).await.unwrap().unwrap().unwrap();
    assert!(!settled.accepted);
    assert!(stop(&fixture.controller).settled().is_some());
    let snapshot = fixture.controller.snapshot().unwrap();
    assert_eq!(snapshot.contract().invocations().len(), 1);
    assert_ne!(snapshot.contract().invocations()[0].evidence.disposition(), EffectDisposition::OutcomeUnknown);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool.count.load(Ordering::SeqCst), 1);
    let usage = fixture.controller.activation_provider_usage(&fixture.activation).unwrap();
    assert_eq!(usage.tokens.usage.input_tokens, 10);
    assert_eq!(usage.tokens.usage.output_tokens, 2);
}

#[tokio::test]
async fn whole_stop_before_prepared_run_never_strands_running_generation_or_replays() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let prepared = fixture.controller.prepare_autonomous_activation(fixture.activation.clone(),
        resources(&fixture, provider.clone(), Arc::new(CountingTool::default()))).unwrap();
    assert!(stop(&fixture.controller).settled().is_none());
    assert!(prepared.run().await.is_err());
    assert!(stop(&fixture.controller).settled().is_some());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(fixture.controller.snapshot().unwrap().contract().current_accepted_activations().is_empty());
}

#[test]
fn whole_stop_recovery_preserves_request_and_closes_without_restarting_actor() {
    let fixture = run_fixture();
    let envelope = {
        let mut state = fixture.controller.lock().unwrap();
        let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
        let envelope = TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("crash-after-durable-stop").unwrap(),
            expected_revision: snapshot.contract().revision(),
            session_id: snapshot.owner().session_id.clone(), turn_id: snapshot.turn_id().clone(),
            event: TurnContractEvent::RequestTurnStop { evidence: snapshot.request_ref().unwrap().clone() },
        };
        state.canonical.append(envelope.clone()).unwrap();
        envelope
    };
    // Stop reached canonical persistence before its process-local cancellation
    // join. Reopen the same actual store, retaining its original format owner.
    let Fixture { _root, ownership, owner, controller, activation, .. } = fixture;
    drop(controller);
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    assert_eq!(canonical.snapshot(&activation.turn_id).unwrap().contract().state(), Some(LogicalTurnState::NeedsAttention));
    let reopened = SessionDispatchController::open(canonical, activation.turn_id.clone()).unwrap();
    let receipt = stop(&reopened);
    assert!(!receipt.first_request());
    assert_eq!(receipt.accepted().envelope(), &envelope);
    assert!(receipt.settled().is_some());
    let snapshot = reopened.snapshot().unwrap();
    assert_eq!(snapshot.contract().state(), Some(LogicalTurnState::Cancelled));
    assert_eq!(snapshot.contract().activations()[0].state, ActivationState::Interrupted);
    assert!(snapshot.contract().current_accepted_activations().is_empty());
}

#[test]
fn whole_stop_between_guidance_handoff_and_actor_ack_records_delivery_without_reopening_work() {
    use axocoatl_session::control_command::{ControlParameters, SteerMode, ControlCommandState, ControlCommandRequest, TrustedCommandSource, CONTROL_COMMAND_SCHEMA_VERSION};
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let prepared = fixture.controller.prepare_autonomous_activation(fixture.activation.clone(),
        resources(&fixture, provider.clone(), Arc::new(CountingTool::default()))).unwrap();
    let instruction = fixture.controller.retain_activation_evidence(ActivationEvidenceContent::Guidance { text: "Exact already handed guidance".into() }).unwrap();
    let snapshot = fixture.controller.snapshot().unwrap();
    let request = ControlCommandRequest {
        schema_version: CONTROL_COMMAND_SCHEMA_VERSION,
        command_id: CommandId::new("guide-whole-stop-race").unwrap(),
        session_id: snapshot.owner().session_id.clone(), turn_id: snapshot.turn_id().clone(),
        execution_epoch_id: snapshot.contract().epochs().last().unwrap().id.clone(),
        expected_turn_revision: snapshot.contract().revision(),
        expected_graph_revision: snapshot.contract().graph().unwrap().revision,
        issued_at_ms: now_ms().unwrap(),
        parameters: ControlParameters::SteerActivation { activation: fixture.activation.clone(), instruction, mode: SteerMode::NextSafeBoundary },
    };
    let source = TrustedCommandSource::human(request.session_id.clone(), request.turn_id.clone(), snapshot.request_ref().unwrap().clone());
    fixture.controller.submit_control_command(request.clone(), source).unwrap();
    let delivery = fixture.controller.take_activation_guidance(&fixture.activation, false).unwrap().unwrap();
    assert!(stop(&fixture.controller).settled().is_none());
    assert_eq!(fixture.controller.control_command_receipt(&request.command_id).unwrap().unwrap().view().state, ControlCommandState::Applied);
    let mut actual_input = axocoatl_memory::session::SessionMemory::new();
    actual_input.append(MessageRole::User, &delivery.text, 4);
    delivery.acknowledgement.acknowledge().unwrap();
    assert_eq!(fixture.controller.control_command_receipt(&request.command_id).unwrap().unwrap().view().state, ControlCommandState::Settled);
    assert!(fixture.controller.take_activation_guidance(&fixture.activation, false).unwrap().is_none());
    drop(prepared);
    assert!(stop(&fixture.controller).settled().is_some());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(fixture.controller.lock().unwrap().poisoned.is_none());
}

#[test]
fn contended_final_ticket_hands_settlement_to_lock_owner_without_runtime_or_next_request() {
    let fixture = run_fixture();
    // This is the same lifetime ticket held by PreparedActivation. A separate
    // read owner holds the exact controller gate when final execution drops.
    let ticket = fixture.controller.lock().unwrap().acquire_execution_ticket(&fixture.controller).unwrap();
    let accepted = stop(&fixture.controller);
    assert!(accepted.settled().is_none());
    let guard = fixture.controller.lock().unwrap();
    assert!(guard.driver.is_none());
    assert_eq!(guard.canonical.snapshot(&guard.turn_id).unwrap().contract().state(), Some(LogicalTurnState::Running));
    std::thread::scope(|scope| { scope.spawn(move || drop(ticket)).join().unwrap(); });
    assert!(guard.execution_lifetimes.is_idle());
    assert_eq!(guard.canonical.snapshot(&guard.turn_id).unwrap().contract().state(), Some(LogicalTurnState::Running));
    drop(guard);
    // Raw observation is intentional: no controller API can secretly repair
    // this assertion. Guard release itself must have completed the handoff.
    let state = fixture.controller.state.lock().unwrap();
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    assert_eq!(snapshot.contract().state(), Some(LogicalTurnState::Cancelled));
    assert!(state.memory.promotion(&snapshot).unwrap().is_some());
    assert!(state.poisoned.is_none());
}

#[test]
fn final_ticket_can_drop_under_its_own_controller_guard_without_reentrant_deadlock() {
    let fixture = run_fixture();
    let ticket = fixture.controller.lock().unwrap().acquire_execution_ticket(&fixture.controller).unwrap();
    assert!(stop(&fixture.controller).settled().is_none());
    let guard = fixture.controller.lock().unwrap();
    drop(ticket);
    assert!(guard.execution_lifetimes.is_idle());
    drop(guard);
    let state = fixture.controller.state.lock().unwrap();
    assert_eq!(state.canonical.snapshot(&state.turn_id).unwrap().contract().state(), Some(LogicalTurnState::Cancelled));
    assert!(state.poisoned.is_none());
}
