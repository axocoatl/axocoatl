use super::*;
use crate::session_dispatch::{HumanControlAction, HumanControlActionRequest};
use axocoatl_session::control_command::{ControlCommandState, ControlParameters};

fn action(fixture: &Fixture, id: &str, action: HumanControlAction) -> HumanControlActionRequest {
    let snapshot = fixture.controller.snapshot().unwrap();
    HumanControlActionRequest {
        schema_version: 1,
        command_id: CommandId::new(id).unwrap(),
        session_id: fixture.activation.session_id.clone(),
        turn_id: fixture.activation.turn_id.clone(),
        execution_epoch_id: fixture.activation.execution_epoch_id.clone(),
        expected_turn_revision: snapshot.contract().revision(),
        expected_graph_revision: snapshot.contract().graph().unwrap().revision,
        activation: Some(fixture.activation.clone()),
        action,
        instruction: None,
        include_previous_output: false, context: None,
        blocker_id: None,
        human_response: None,
        partial_finish: None,
        continuation: None,
    }
}

#[test]
fn human_action_decode_rejects_inconsistent_identity_unknown_fields_and_schema() {
    let fixture = run_fixture();
    let request = action(&fixture, "decode", HumanControlAction::Stop);
    let bytes = serde_json::to_vec(&request).unwrap();
    assert_eq!(HumanControlActionRequest::decode(&bytes).unwrap(), request);
    for (field, value) in [
        ("schema_version", serde_json::json!(2)),
        ("session_id", serde_json::json!("foreign")),
        ("turn_id", serde_json::json!("foreign")),
        ("execution_epoch_id", serde_json::json!("foreign")),
        ("expected_graph_revision", serde_json::json!(0)),
        ("source", serde_json::json!("human")),
    ] {
        let mut wire = serde_json::to_value(&request).unwrap();
        wire[field] = value;
        assert!(
            HumanControlActionRequest::decode(&serde_json::to_vec(&wire).unwrap()).is_err(),
            "{field}"
        );
    }
    assert!(HumanControlActionRequest::decode(&vec![b' '; 128 * 1024 + 1]).is_err());
}

#[tokio::test]
async fn host_stop_retry_preserves_captured_input_and_repeated_receipt_then_runs_one_generation() {
    let fixture = run_fixture();
    let before = fixture
        .controller
        .snapshot()
        .unwrap()
        .contract()
        .activations()[0]
        .input
        .clone();
    let stop = action(&fixture, "host-stop", HumanControlAction::Stop);
    let stopped = fixture
        .controller
        .submit_human_action(stop.clone(), 100)
        .unwrap();
    assert_eq!(stopped.state, ControlCommandState::Settled);
    assert_eq!(
        fixture
            .controller
            .submit_human_action(stop.clone(), 999)
            .unwrap(),
        stopped
    );
    let mut conflict = stop;
    conflict.action = HumanControlAction::Retry;
    assert!(fixture
        .controller
        .submit_human_action(conflict, 999)
        .is_err());
    let retry = action(&fixture, "host-retry", HumanControlAction::Retry);
    let receipt = fixture
        .controller
        .submit_human_action(retry.clone(), 200)
        .unwrap();
    assert_eq!(receipt.state, ControlCommandState::Settled);
    assert_eq!(
        fixture.controller.submit_human_action(retry, 1000).unwrap(),
        receipt
    );
    let ControlParameters::RetryActivation {
        input,
        replay_decisions,
        ..
    } = &receipt.request.parameters
    else {
        panic!("host did not build retry")
    };
    assert!(replay_decisions.is_empty());
    assert_ne!(
        input.activation.activation_id,
        before.activation.activation_id
    );
    assert_eq!(
        input.activation.generation,
        before.activation.generation + 1
    );
    assert_eq!(
        input.activation.execution_epoch_id,
        before.activation.execution_epoch_id
    );
    assert_eq!(input.starting_savepoint, before.starting_savepoint);
    assert_eq!(input.parents, before.parents);
    assert_eq!(input.guidance, before.guidance);
    assert_eq!(input.budget, before.budget);
    assert_eq!(
        fixture
            .controller
            .snapshot()
            .unwrap()
            .contract()
            .activations()
            .len(),
        2
    );
    let mut provider = RunProvider::new(&fixture, RunProviderMode::Answer, true);
    provider.activation = input.activation.clone();
    let provider = Arc::new(provider);
    let settled = fixture
        .controller
        .prepare_autonomous_activation(
            input.activation.clone(),
            resources(
                &fixture,
                provider.clone(),
                Arc::new(CountingTool::default()),
            ),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn concurrent_identical_actions_share_one_receipt_despite_different_host_times() {
    let fixture = run_fixture();
    let request = action(&fixture, "concurrent-stop", HumanControlAction::Stop);
    let threads = (0..8)
        .map(|index| {
            let controller = fixture.controller.clone();
            let request = request.clone();
            std::thread::spawn(move || {
                controller
                    .submit_human_action(request, 100 + index)
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    let receipts = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert!(receipts.iter().all(|receipt| *receipt == receipts[0]));
    assert_eq!(
        fixture
            .controller
            .snapshot()
            .unwrap()
            .contract()
            .activations()
            .len(),
        1
    );
    let state = fixture.controller.lock().unwrap();
    assert_eq!(
        state
            .commands
            .records()
            .unwrap()
            .iter()
            .filter(|record| matches!(
                record.event,
                axocoatl_session::control_command::ControlCommandEvent::Requested { .. }
            ))
            .count(),
        1
    );
}

#[test]
fn durable_host_receipts_survive_reconstruction_with_their_original_identity() {
    let fixture = run_fixture();
    let request = action(&fixture, "retained-stop", HumanControlAction::Stop);
    let original = fixture
        .controller
        .submit_human_action(request.clone(), 100)
        .unwrap();
    let records = fixture
        .controller
        .lock()
        .unwrap()
        .control_plane_commands()
        .unwrap();
    assert_eq!(records, vec![original.clone()]);
    let Fixture {
        _root,
        ownership,
        owner,
        controller,
        activation,
        ..
    } = fixture;
    drop(controller);
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    let reopened = SessionDispatchController::open(canonical, activation.turn_id).unwrap();
    assert_eq!(
        reopened.lock().unwrap().control_plane_commands().unwrap(),
        records
    );
    assert_eq!(
        reopened.submit_human_action(request, 9999).unwrap(),
        original
    );
}

#[test]
fn capabilities_are_nonmutating_and_match_actual_stale_and_revoked_rejections() {
    let fixture = run_fixture();
    let initial = fixture.controller.snapshot().unwrap();
    let state = fixture.controller.lock().unwrap();
    let records = state.commands.records().unwrap().to_vec();
    let capabilities = state
        .human_control_capabilities(&fixture.activation, 100)
        .unwrap();
    assert!(capabilities.stop.enabled);
    assert!(!capabilities.retry.enabled);
    assert_eq!(records, state.commands.records().unwrap());
    assert_eq!(
        state
            .canonical
            .snapshot(&fixture.activation.turn_id)
            .unwrap()
            .contract()
            .revision(),
        initial.contract().revision()
    );
    drop(state);
    let stop = action(&fixture, "capability-stop", HumanControlAction::Stop);
    let mut stale = stop.clone();
    stale.command_id = CommandId::new("stale-stop").unwrap();
    fixture.controller.submit_human_action(stop, 100).unwrap();
    let rejected = fixture.controller.submit_human_action(stale, 100).unwrap();
    assert_eq!(rejected.state, ControlCommandState::Rejected);
    let state = fixture.controller.lock().unwrap();
    let capabilities = state
        .human_control_capabilities(&fixture.activation, 100)
        .unwrap();
    assert!(!capabilities.stop.enabled);
    assert!(capabilities.retry.enabled, "{}", capabilities.retry.reason);
    state
        .authority
        .revoke_grant("grant", state.authority.revision().unwrap())
        .unwrap();
    let capabilities = state
        .human_control_capabilities(&fixture.activation, 100)
        .unwrap();
    assert!(!capabilities.retry.enabled);
    drop(state);
    let retry = action(&fixture, "revoked-retry", HumanControlAction::Retry);
    assert_eq!(
        fixture
            .controller
            .submit_human_action(retry, 100)
            .unwrap()
            .state,
        ControlCommandState::Rejected
    );
}

#[tokio::test]
async fn restart_and_unresolved_effects_never_gain_automatic_retry_permission() {
    let fixture = run_fixture();
    let control = bind(&fixture);
    let permit = control
        .execution_boundary()
        .unwrap()
        .admit(&ToolInvocationRequest {
            actor_id: fixture.config.id.to_string(),
            provider_id: fixture.profile.provider.clone(),
            model_id: fixture.profile.model.clone(),
            provider_response_group: 1,
            provider_call_index: 0,
            provider_call_count: 1,
            tool_call: axocoatl_llm::ToolCall {
                id: "unresolved-invocation".into(),
                name: "effect".into(),
                arguments: serde_json::json!({}),
                provider_metadata: Default::default(),
            },
        })
        .await
        .unwrap();
    // Losing the admitted invocation's waiter supplies neither an outcome nor
    // proof of non-dispatch. Its real durable intent remains unresolved.
    drop(permit);
    let snapshot = fixture.controller.snapshot().unwrap();
    assert_eq!(snapshot.contract().invocations().len(), 1);
    assert_eq!(
        snapshot.contract().invocations()[0].activation,
        fixture.activation
    );
    assert_eq!(
        snapshot.contract().invocations()[0].evidence.disposition(),
        EffectDisposition::OutcomeUnknown
    );
    // Close the actual execution lease before recording its failed generation,
    // as the owned runtime does. A still-live prior lease would reject Retry
    // before the unresolved effect itself is examined.
    fixture
        .controller
        .stop_activation(&fixture.activation)
        .unwrap();
    fixture
        .controller
        .append_host_event(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("failed-generation").unwrap(),
            expected_revision: snapshot.contract().revision(),
            session_id: fixture.activation.session_id.clone(),
            turn_id: fixture.activation.turn_id.clone(),
            event: TurnContractEvent::FailActivation {
                activation: fixture.activation.clone(),
                evidence: EvidenceRef::new("failed-output").unwrap(),
            },
        })
        .unwrap();
    let state = fixture.controller.lock().unwrap();
    let capability = state
        .human_control_capabilities(&fixture.activation, now_ms().unwrap())
        .unwrap();
    assert!(!capability.retry.enabled);
    assert!(!capability.retry.reason.is_empty());
    drop(state);
    let retry = action(&fixture, "unsafe-retry", HumanControlAction::Retry);
    assert_eq!(
        fixture
            .controller
            .submit_human_action(retry, now_ms().unwrap())
            .unwrap()
            .state,
        ControlCommandState::Rejected
    );
    let snapshot = fixture.controller.snapshot().unwrap();
    fixture
        .controller
        .append_host_event(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("lost-epoch").unwrap(),
            expected_revision: snapshot.contract().revision(),
            session_id: fixture.activation.session_id.clone(),
            turn_id: fixture.activation.turn_id.clone(),
            event: TurnContractEvent::InterruptEpoch {
                epoch_id: fixture.activation.execution_epoch_id.clone(),
            },
        })
        .unwrap();
    let retry = action(&fixture, "cross-epoch-retry", HumanControlAction::Retry);
    assert_eq!(
        fixture
            .controller
            .submit_human_action(retry, now_ms().unwrap())
            .unwrap()
            .state,
        ControlCommandState::Rejected
    );
}

fn turn_action(fixture: &Fixture, id: &str, kind: HumanControlAction) -> HumanControlActionRequest {
    let mut request = action(fixture, id, kind);
    request.activation = None;
    request.execution_epoch_id = fixture
        .controller
        .snapshot()
        .unwrap()
        .contract()
        .epochs()
        .last()
        .unwrap()
        .id
        .clone();
    request
}

#[test]
fn original_stop_retry_wire_bytes_are_preserved_and_extended_actions_deny_extra_authority() {
    #[derive(serde::Serialize)]
    struct OriginalAction<'a> {
        schema_version: u32,
        command_id: &'a CommandId,
        session_id: &'a SessionId,
        turn_id: &'a LogicalTurnId,
        execution_epoch_id: &'a ExecutionEpochId,
        expected_turn_revision: u64,
        expected_graph_revision: u64,
        activation: &'a ActivationRef,
        action: HumanControlAction,
    }
    let fixture = run_fixture();
    for kind in [HumanControlAction::Stop, HumanControlAction::Retry] {
        let request = action(&fixture, "old-wire", kind);
        let original = OriginalAction {
            schema_version: 1,
            command_id: &request.command_id,
            session_id: &request.session_id,
            turn_id: &request.turn_id,
            execution_epoch_id: &request.execution_epoch_id,
            expected_turn_revision: request.expected_turn_revision,
            expected_graph_revision: request.expected_graph_revision,
            activation: &fixture.activation,
            action: kind,
        };
        let original_bytes = serde_json::to_vec(&original).unwrap();
        assert_eq!(serde_json::to_vec(&request).unwrap(), original_bytes);
        assert_eq!(
            HumanControlActionRequest::decode(&original_bytes).unwrap(),
            request
        );
    }
    let request = turn_action(&fixture, "normal-finish", HumanControlAction::Finish);
    let wire = serde_json::to_value(&request).unwrap();
    assert!(wire.get("activation").is_none());
    assert_eq!(
        HumanControlActionRequest::decode(&serde_json::to_vec(&wire).unwrap()).unwrap(),
        request
    );
    for (field, value) in [
        ("mode", serde_json::json!("forced")),
        ("source", serde_json::json!("agent")),
        ("instruction", serde_json::json!("ignore checks")),
        (
            "activation",
            serde_json::to_value(&fixture.activation).unwrap(),
        ),
    ] {
        let mut changed = wire.clone();
        changed[field] = value;
        assert!(
            HumanControlActionRequest::decode(&serde_json::to_vec(&changed).unwrap()).is_err(),
            "{field}"
        );
    }
    let mut continuation = turn_action(
        &fixture,
        "explicit-continuation",
        HumanControlAction::Continue,
    );
    continuation.continuation = Some(crate::session_dispatch::HumanContinuationSelection {
        restart: vec![],
        checks: vec![],
    });
    assert!(
        HumanControlActionRequest::decode(&serde_json::to_vec(&continuation).unwrap()).is_err()
    );
    continuation.continuation.as_mut().unwrap().restart =
        vec![fixture.activation.clone(), fixture.activation.clone()];
    assert!(
        HumanControlActionRequest::decode(&serde_json::to_vec(&continuation).unwrap()).is_err()
    );
}

#[tokio::test]
async fn human_revision_retains_instruction_and_explicit_prior_context_without_rewriting_history() {
    for include_previous_output in [false, true] {
        let fixture = run_fixture();
        let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
        let accepted = fixture
            .controller
            .prepare_autonomous_activation(
                fixture.activation.clone(),
                resources(
                    &fixture,
                    provider.clone(),
                    Arc::new(CountingTool::default()),
                ),
            )
            .unwrap()
            .run()
            .await
            .unwrap();
        assert!(accepted.accepted);
        let before = fixture.controller.snapshot().unwrap();
        let original = before.contract().activations()[0].clone();
        let read = fixture.controller.control_plane().unwrap();
        assert!(
            read.nodes[0].activations[0].capabilities.revise.enabled,
            "{}",
            read.nodes[0].activations[0].capabilities.revise.reason
        );
        assert!(read.nodes[0].activations[0]
            .capabilities
            .revise_invalidates
            .is_empty());
        assert_eq!(
            fixture.controller.snapshot().unwrap().contract().revision(),
            before.contract().revision()
        );
        assert!(
            matches!(read.commands, crate::session_control_plane::EvidenceValue::Available { value } if value.is_empty())
        );
        let mut request = action(&fixture, "human-revision", HumanControlAction::Revise);
        request.instruction = Some("Explain the exact check result.".into());
        request.include_previous_output = include_previous_output;
        let receipt = fixture
            .controller
            .submit_human_action(request.clone(), 200)
            .unwrap();
        assert_eq!(receipt.state, ControlCommandState::Settled);
        let ControlParameters::ReviseActivation {
            input,
            instruction,
            invalidate,
            ..
        } = &receipt.request.parameters
        else {
            panic!("expected revision")
        };
        assert!(invalidate.is_empty());
        assert!(input.guidance.contains(instruction));
        assert_eq!(input.revision_context.is_some(), include_previous_output);
        if let Some(context) = &input.revision_context {
            assert_eq!(context.activation, fixture.activation);
            assert_eq!(context.output, accepted.output.reference().clone());
        }
        let state = fixture.controller.lock().unwrap();
        assert!(
            matches!(state.content.resolve_activation_evidence(instruction).unwrap(),
            ActivationEvidenceContent::Guidance { text } if text == "Explain the exact check result.")
        );
        let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
        let historical = &snapshot.contract().activations()[0];
        assert_eq!(historical.input, original.input);
        assert_eq!(historical.output, original.output);
        assert_eq!(historical.checkpoint, original.checkpoint);
        assert_eq!(historical.state, ActivationState::Superseded);
        assert_eq!(snapshot.contract().activations()[1].input, **input);
        drop(state);
        assert_eq!(
            fixture
                .controller
                .submit_human_action(request.clone(), 999)
                .unwrap(),
            receipt
        );
        let mut changed = request;
        changed.instruction = Some("Different instruction".into());
        assert!(fixture
            .controller
            .submit_human_action(changed, 999)
            .is_err());
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fixture
                .controller
                .snapshot()
                .unwrap()
                .contract()
                .activations()
                .len(),
            2
        );
    }
}

#[test]
fn human_continue_creates_one_explicit_epoch_and_retains_exact_receipt_after_reopen() {
    let fixture = run_fixture();
    let before = fixture.controller.snapshot().unwrap();
    fixture
        .controller
        .append_host_event(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("interrupt-human-continuation").unwrap(),
            expected_revision: before.contract().revision(),
            session_id: fixture.activation.session_id.clone(),
            turn_id: fixture.activation.turn_id.clone(),
            event: TurnContractEvent::InterruptEpoch {
                epoch_id: fixture.activation.execution_epoch_id.clone(),
            },
        })
        .unwrap();
    let read = fixture.controller.control_plane().unwrap();
    let controls = read.turn_controls.unwrap();
    assert!(
        controls.continue_turn.enabled,
        "{}",
        controls.continue_turn.reason
    );
    assert_eq!(controls.continuation_choices.len(), 1);
    assert_eq!(
        controls.continuation_choices[0].activation,
        fixture.activation
    );
    let mut request = turn_action(&fixture, "human-continue", HumanControlAction::Continue);
    request.continuation = Some(crate::session_dispatch::HumanContinuationSelection {
        restart: vec![fixture.activation.clone()],
        checks: vec![],
    });
    let receipt = fixture
        .controller
        .submit_human_action(request.clone(), 200)
        .unwrap();
    assert_eq!(receipt.state, ControlCommandState::Settled);
    let ControlParameters::ContinueTurn {
        plan,
        replay_decisions,
    } = &receipt.request.parameters
    else {
        panic!("expected Continue")
    };
    assert_eq!(plan.source_epoch_id, fixture.activation.execution_epoch_id);
    assert_ne!(plan.epoch_id, plan.source_epoch_id);
    assert!(replay_decisions.is_empty());
    assert_eq!(plan.selections.len(), 1);
    let ContinuationSelection::Retry { previous, input } = &plan.selections[0] else {
        panic!("selected restart")
    };
    assert_eq!(previous, &fixture.activation);
    assert_eq!(input.activation.execution_epoch_id, plan.epoch_id);
    assert_eq!(
        input.guidance,
        before.contract().activations()[0].input.guidance
    );
    assert_eq!(
        input.starting_savepoint,
        before.contract().activations()[0].input.starting_savepoint
    );
    assert_eq!(
        fixture
            .controller
            .submit_human_action(request.clone(), 900)
            .unwrap(),
        receipt
    );
    let snapshot = fixture.controller.snapshot().unwrap();
    assert_eq!(snapshot.contract().epochs().len(), 2);
    assert_eq!(snapshot.contract().activations().len(), 2);
    let Fixture {
        _root,
        ownership,
        owner,
        controller,
        activation,
        ..
    } = fixture;
    drop(controller);
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    let reopened = SessionDispatchController::open(canonical, activation.turn_id).unwrap();
    let repeated = reopened.submit_human_action(request, 1000).unwrap();
    assert_eq!(repeated, receipt);
    assert!(matches!(reopened.control_plane().unwrap().commands,
        crate::session_control_plane::EvidenceValue::Available { value } if value == vec![receipt]));
}

#[tokio::test]
async fn human_normal_finish_receipt_waits_for_real_tool_settlement_and_survives_refresh() {
    let fixture = run_fixture();
    let release = Arc::new(tokio::sync::Notify::new());
    let tool = Arc::new(CountingTool {
        release: Some(release.clone()),
        ..Default::default()
    });
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let prepared = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .unwrap();
    let task = tokio::spawn(prepared.run());
    tokio::time::timeout(Duration::from_secs(3), tool.started.notified())
        .await
        .unwrap();
    let controls = fixture
        .controller
        .control_plane()
        .unwrap()
        .turn_controls
        .unwrap();
    assert!(controls.finish.enabled, "{}", controls.finish.reason);
    let request = turn_action(&fixture, "human-finish", HumanControlAction::Finish);
    let receipt = fixture
        .controller
        .submit_human_action(request.clone(), 100)
        .unwrap();
    assert_eq!(receipt.state, ControlCommandState::Accepted);
    assert_eq!(
        fixture.controller.snapshot().unwrap().contract().state(),
        Some(LogicalTurnState::Running)
    );
    assert!(
        matches!(fixture.controller.control_plane().unwrap().commands,
        crate::session_control_plane::EvidenceValue::Available { value } if value == vec![receipt])
    );
    release.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .accepted
    );
    fixture.controller.reconcile_control_commands().unwrap();
    let settled = fixture
        .controller
        .submit_human_action(request, 999)
        .unwrap();
    assert_eq!(settled.state, ControlCommandState::Settled);
    assert!(matches!(
        settled.request.parameters,
        ControlParameters::FinishTurn {
            mode: axocoatl_session::control_command::FinishMode::Normal
        }
    ));
    assert_eq!(
        fixture.controller.snapshot().unwrap().contract().state(),
        Some(LogicalTurnState::Completed)
    );
    assert_eq!(tool.count.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
}
