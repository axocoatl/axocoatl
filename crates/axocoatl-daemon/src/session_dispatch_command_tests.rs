#[path = "session_dispatch_steering_tests.rs"]
mod steering_tests;

use super::*;
use axocoatl_session::control_command::{
    CommandSourceRecord, ControlCommandRequest, ControlCommandState, ControlParameters,
    ControlReceiptUpdate, ControlTransition, FinishMode, SteerMode, TrustedCommandSource,
    CONTROL_COMMAND_SCHEMA_VERSION,
};

fn command_request(
    controller: &SessionDispatchController,
    id: &str,
    parameters: ControlParameters,
) -> ControlCommandRequest {
    let snapshot = controller.snapshot().unwrap();
    ControlCommandRequest {
        schema_version: CONTROL_COMMAND_SCHEMA_VERSION,
        command_id: CommandId::new(id).unwrap(),
        session_id: snapshot.owner().session_id.clone(),
        turn_id: snapshot.turn_id().clone(),
        execution_epoch_id: snapshot.contract().epochs().last().unwrap().id.clone(),
        expected_turn_revision: snapshot.contract().revision(),
        expected_graph_revision: snapshot.contract().graph().unwrap().revision,
        issued_at_ms: now_ms().unwrap(),
        parameters,
    }
}

fn human_source(controller: &SessionDispatchController) -> TrustedCommandSource {
    let snapshot = controller.snapshot().unwrap();
    TrustedCommandSource::human(
        snapshot.owner().session_id.clone(),
        snapshot.turn_id().clone(),
        snapshot.request_ref().unwrap().clone(),
    )
}

fn stop_request(fixture: &Fixture, id: &str) -> ControlCommandRequest {
    command_request(
        &fixture.controller,
        id,
        ControlParameters::StopActivation {
            activation: fixture.activation.clone(),
        },
    )
}

fn retry_input(fixture: &Fixture) -> ActivationInputManifest {
    let snapshot = fixture.controller.snapshot().unwrap();
    let mut input = snapshot
        .contract()
        .activations()
        .iter()
        .find(|item| item.activation == fixture.activation)
        .unwrap()
        .input
        .clone();
    input.manifest_id = InputManifestId::new("command-retry-input").unwrap();
    input.activation.activation_id = ActivationId::new("command-retry-activation").unwrap();
    input.activation.generation += 1;
    input
}

#[tokio::test]
async fn durable_stop_returns_before_running_provider_settles_and_exact_repeat_never_reexecutes() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Pending, true));
    let prepared = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(
                &fixture,
                provider.clone(),
                Arc::new(CountingTool::default()),
            ),
        )
        .unwrap();
    let task = tokio::spawn(prepared.run());
    tokio::time::timeout(Duration::from_secs(3), provider.started.notified())
        .await
        .unwrap();
    let request = stop_request(&fixture, "stop-running");
    let accepted = fixture
        .controller
        .submit_control_command(request.clone(), human_source(&fixture.controller))
        .unwrap();
    assert_eq!(accepted.view().state, ControlCommandState::Accepted);
    let duplicate = fixture
        .controller
        .submit_control_command(request.clone(), human_source(&fixture.controller))
        .unwrap();
    assert_eq!(accepted.view(), duplicate.view());
    let settled = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!settled.accepted);
    fixture.controller.reconcile_control_commands().unwrap();
    let receipt = fixture
        .controller
        .submit_control_command(request.clone(), human_source(&fixture.controller))
        .unwrap();
    assert_eq!(receipt.view().state, ControlCommandState::Settled);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let ControlTransition::Settled { result } = receipt.view().last_transition.as_ref().unwrap()
    else {
        panic!("missing settled result")
    };
    let evidence = fixture
        .controller
        .control_command_evidence(result)
        .unwrap()
        .unwrap();
    assert_eq!(evidence["event"]["kind"], "fail_activation");
    assert_eq!(
        evidence["event"]["activation"]["activation_id"],
        fixture.activation.activation_id.as_str()
    );
    let mut conflicting = request;
    conflicting.parameters = ControlParameters::FinishTurn {
        mode: FinishMode::Normal,
    };
    assert!(fixture
        .controller
        .submit_control_command(conflicting, human_source(&fixture.controller))
        .is_err());

    let input = retry_input(&fixture);
    let request = command_request(
        &fixture.controller,
        "retry-after-stop",
        ControlParameters::RetryActivation {
            activation: fixture.activation.clone(),
            input: Box::new(input.clone()),
            replay_decisions: vec![],
        },
    );
    let receipt = fixture
        .controller
        .submit_control_command(request, human_source(&fixture.controller))
        .unwrap();
    assert_eq!(receipt.view().state, ControlCommandState::Settled);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let next_provider = Arc::new(RunProvider::new(
        &Fixture {
            _root: tempfile::tempdir().unwrap(),
            ownership: fixture.ownership.clone(),
            owner: fixture.owner.clone(),
            controller: fixture.controller.clone(),
            activation: input.activation.clone(),
            profile: fixture.profile.clone(),
            config: fixture.config.clone(),
        },
        RunProviderMode::Answer,
        true,
    ));
    let next = fixture
        .controller
        .prepare_autonomous_activation(
            input.activation,
            resources(
                &fixture,
                next_provider.clone(),
                Arc::new(CountingTool::default()),
            ),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(next.accepted, "{:?}", next.failure);
    assert_eq!(next_provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn human_stop_before_executor_binding_is_durable_and_stale_new_commands_are_rejected() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let request = stop_request(&fixture, "stop-before-bind");
    let mut stale = request.clone();
    stale.command_id = CommandId::new("different-stale-stop").unwrap();
    let result = fixture
        .controller
        .submit_control_command(request.clone(), human_source(&fixture.controller))
        .unwrap();
    assert_eq!(result.view().state, ControlCommandState::Settled);
    assert!(fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(
                &fixture,
                provider.clone(),
                Arc::new(CountingTool::default())
            )
        )
        .is_err());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    let rejected = fixture
        .controller
        .submit_control_command(stale, human_source(&fixture.controller))
        .unwrap();
    assert_eq!(rejected.view().state, ControlCommandState::Rejected);
    assert_eq!(
        fixture
            .controller
            .submit_control_command(request, human_source(&fixture.controller))
            .unwrap()
            .view(),
        result.view()
    );
    assert_eq!(
        fixture
            .controller
            .snapshot()
            .unwrap()
            .contract()
            .activations()[0]
            .state,
        ActivationState::Failed
    );
}

#[tokio::test]
async fn normal_finish_waits_for_actual_actor_then_closes_and_promotes_once() {
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
    let request = command_request(
        &fixture.controller,
        "finish-while-running",
        ControlParameters::FinishTurn {
            mode: FinishMode::Normal,
        },
    );
    assert_eq!(
        fixture
            .controller
            .submit_control_command(request.clone(), human_source(&fixture.controller))
            .unwrap()
            .view()
            .state,
        ControlCommandState::Accepted
    );
    assert_eq!(
        fixture.controller.snapshot().unwrap().contract().state(),
        Some(LogicalTurnState::Running)
    );
    release.notify_one();
    let settled = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    fixture.controller.reconcile_control_commands().unwrap();
    let result = fixture
        .controller
        .submit_control_command(request.clone(), human_source(&fixture.controller))
        .unwrap();
    assert_eq!(result.view().state, ControlCommandState::Settled);
    assert_eq!(
        fixture.controller.snapshot().unwrap().contract().state(),
        Some(LogicalTurnState::Completed)
    );
    let state = fixture.controller.lock().unwrap();
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    let promotion = state.memory.promotion(&snapshot).unwrap().unwrap();
    assert_eq!(promotion.selected.len(), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(tool.count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn revision_command_allocates_one_unstarted_generation_with_exact_input() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let result = fixture
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
    assert!(result.accepted);
    let instruction = fixture
        .controller
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: "Revise the previous answer with an explicit check".into(),
        })
        .unwrap();
    let mut input = retry_input(&fixture);
    input.guidance.push(instruction.clone());
    input.revision_context = Some(RevisionContext {
        activation: fixture.activation.clone(),
        output: result.output.reference().clone(),
    });
    let request = command_request(
        &fixture.controller,
        "revise-answer",
        ControlParameters::ReviseActivation {
            activation: fixture.activation.clone(),
            input: Box::new(input.clone()),
            instruction,
            invalidate: vec![],
        },
    );
    let receipt = fixture
        .controller
        .submit_control_command(request.clone(), human_source(&fixture.controller))
        .unwrap();
    assert_eq!(receipt.view().state, ControlCommandState::Settled);
    assert_eq!(
        fixture
            .controller
            .submit_control_command(request, human_source(&fixture.controller))
            .unwrap()
            .view(),
        receipt.view()
    );
    let snapshot = fixture.controller.snapshot().unwrap();
    assert_eq!(snapshot.contract().activations().len(), 2);
    assert_eq!(
        snapshot.contract().activations()[0].state,
        ActivationState::Superseded
    );
    assert_eq!(
        snapshot.contract().activations()[1].state,
        ActivationState::Unstarted
    );
    assert_eq!(snapshot.contract().activations()[1].input, input);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn continue_receipts_keep_source_epoch_and_do_not_duplicate_prepared_generations() {
    let fixture = run_fixture();
    let snapshot = fixture.controller.snapshot().unwrap();
    fixture
        .controller
        .append_host_event(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("interrupt-for-control").unwrap(),
            expected_revision: snapshot.contract().revision(),
            session_id: fixture.owner.session_id.clone(),
            turn_id: fixture.activation.turn_id.clone(),
            event: TurnContractEvent::InterruptEpoch {
                epoch_id: fixture.activation.execution_epoch_id.clone(),
            },
        })
        .unwrap();
    let mut input = retry_input(&fixture);
    input.activation.execution_epoch_id = ExecutionEpochId::new("continued-control-epoch").unwrap();
    let request = command_request(
        &fixture.controller,
        "continue-once",
        ControlParameters::ContinueTurn {
            plan: ContinuationPlan {
                source_epoch_id: fixture.activation.execution_epoch_id.clone(),
                epoch_id: input.activation.execution_epoch_id.clone(),
                selections: vec![ContinuationSelection::Retry {
                    previous: fixture.activation.clone(),
                    input: Box::new(input),
                }],
                condition_runs: vec![],
            },
            replay_decisions: vec![],
        },
    );
    let result = fixture
        .controller
        .submit_control_command(request.clone(), human_source(&fixture.controller))
        .unwrap();
    assert_eq!(result.view().state, ControlCommandState::Settled);
    assert_eq!(
        result.view().request.execution_epoch_id,
        fixture.activation.execution_epoch_id
    );
    assert_eq!(
        fixture
            .controller
            .submit_control_command(request, human_source(&fixture.controller))
            .unwrap()
            .view(),
        result.view()
    );
    let snapshot = fixture.controller.snapshot().unwrap();
    assert_eq!(snapshot.contract().epochs().len(), 2);
    assert_eq!(snapshot.contract().activations().len(), 2);
    assert_eq!(
        snapshot.contract().activations()[1].state,
        ActivationState::Unstarted
    );
}

#[test]
fn unsupported_controls_and_revoked_input_grants_are_rejected_without_application() {
    let fixture = run_fixture();
    let request = command_request(
        &fixture.controller,
        "unsupported-steer",
        ControlParameters::SteerActivation {
            activation: fixture.activation.clone(),
            instruction: fixture
                .controller
                .snapshot()
                .unwrap()
                .request_ref()
                .unwrap()
                .clone(),
            mode: SteerMode::NextSafeBoundary,
        },
    );
    let before = fixture.controller.snapshot().unwrap().contract().revision();
    assert_eq!(
        fixture
            .controller
            .submit_control_command(request, human_source(&fixture.controller))
            .unwrap()
            .view()
            .state,
        ControlCommandState::Rejected
    );
    assert_eq!(
        fixture.controller.snapshot().unwrap().contract().revision(),
        before
    );
    fixture
        .controller
        .submit_control_command(
            stop_request(&fixture, "stop-to-revoke"),
            human_source(&fixture.controller),
        )
        .unwrap();
    {
        let state = fixture.controller.lock().unwrap();
        state
            .authority
            .revoke_grant("grant", state.authority.revision().unwrap())
            .unwrap();
    }
    let request = command_request(
        &fixture.controller,
        "retry-revoked",
        ControlParameters::RetryActivation {
            activation: fixture.activation.clone(),
            input: Box::new(retry_input(&fixture)),
            replay_decisions: vec![],
        },
    );
    assert_eq!(
        fixture
            .controller
            .submit_control_command(request, human_source(&fixture.controller))
            .unwrap()
            .view()
            .state,
        ControlCommandState::Rejected
    );
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
}

#[test]
fn agent_attribution_cannot_mint_ungranted_self_stop_or_survive_revocation() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let prepared = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(
                &fixture,
                provider.clone(),
                Arc::new(CountingTool::default()),
            ),
        )
        .unwrap();
    let source = {
        let state = fixture.controller.lock().unwrap();
        let lease = &state
            .bound
            .get(&fixture.activation.activation_id)
            .unwrap()
            .lease;
        state
            .authority
            .attest_control_source(lease, now_ms().unwrap())
            .unwrap()
    };
    assert!(matches!(source.record(), CommandSourceRecord::Agent { .. }));
    assert_eq!(
        fixture
            .controller
            .submit_control_command(stop_request(&fixture, "agent-self-stop"), source)
            .unwrap()
            .view()
            .state,
        ControlCommandState::Rejected
    );
    let source = {
        let state = fixture.controller.lock().unwrap();
        let lease = &state
            .bound
            .get(&fixture.activation.activation_id)
            .unwrap()
            .lease;
        let source = state
            .authority
            .attest_control_source(lease, now_ms().unwrap())
            .unwrap();
        state
            .authority
            .revoke_grant("grant", state.authority.revision().unwrap())
            .unwrap();
        source
    };
    assert_eq!(
        fixture
            .controller
            .submit_control_command(stop_request(&fixture, "agent-revoked-stop"), source)
            .unwrap()
            .view()
            .state,
        ControlCommandState::Rejected
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    drop(prepared);
}

#[test]
fn recovered_canonical_allocation_repairs_accepted_receipt_without_replay() {
    let fixture = run_fixture();
    fixture
        .controller
        .submit_control_command(
            stop_request(&fixture, "stop-for-crash-cut"),
            human_source(&fixture.controller),
        )
        .unwrap();
    let input = retry_input(&fixture);
    let request = command_request(
        &fixture.controller,
        "retry-crash-cut",
        ControlParameters::RetryActivation {
            activation: fixture.activation.clone(),
            input: Box::new(input.clone()),
            replay_decisions: vec![],
        },
    );
    let original = fixture
        .controller
        .submit_control_command(request.clone(), human_source(&fixture.controller))
        .unwrap();
    assert_eq!(original.view().state, ControlCommandState::Settled);
    let path = fixture.controller.lock().unwrap().commands.path();
    let mut journal: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let records = journal["records"].as_array_mut().unwrap();
    assert_eq!(
        records.last().unwrap()["event"]["update"]["transition"]["state"],
        "settled"
    );
    // Serialized crash fixture: canonical allocation is durable, receipt suffix
    // Applied/Settled is absent. The exact accepted request remains untouched.
    records.truncate(records.len() - 2);
    let Fixture {
        _root,
        ownership,
        owner,
        controller,
        activation,
        ..
    } = fixture;
    drop(controller);
    std::fs::write(path, serde_json::to_vec(&journal).unwrap()).unwrap();
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    let reopened = SessionDispatchController::open(canonical, activation.turn_id).unwrap();
    let recovered = reopened
        .submit_control_command(request, human_source(&reopened))
        .unwrap();
    assert_eq!(recovered.view(), original.view());
    let snapshot = reopened.snapshot().unwrap();
    assert_eq!(snapshot.contract().activations().len(), 2);
    assert_eq!(
        snapshot.contract().activations()[1].activation,
        input.activation
    );
    assert_eq!(
        snapshot.contract().activations()[1].state,
        ActivationState::Interrupted
    );
    assert_eq!(
        snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert!(reopened.lock().unwrap().bound.is_empty());
}

#[test]
fn requested_or_accepted_without_canonical_application_never_replays_on_reopen() {
    for accepted in [false, true] {
        let fixture = run_fixture();
        let request = stop_request(&fixture, "pending-stop-cut");
        let source = human_source(&fixture.controller);
        {
            let mut state = fixture.controller.lock().unwrap();
            let receipt = state
                .commands
                .record_requested(request.clone(), source)
                .unwrap();
            if accepted {
                let evidence = state
                    .canonical
                    .snapshot(&state.turn_id)
                    .unwrap()
                    .request_ref()
                    .unwrap()
                    .clone();
                state
                    .commands
                    .advance(ControlReceiptUpdate {
                        update_id: CommandId::new("accepted-before-crash").unwrap(),
                        command_id: request.command_id.clone(),
                        session_id: request.session_id.clone(),
                        turn_id: request.turn_id.clone(),
                        execution_epoch_id: request.execution_epoch_id.clone(),
                        expected_receipt_revision: receipt.view().revision,
                        transition: ControlTransition::Accepted {
                            validation: evidence.clone(),
                            pending: evidence,
                        },
                    })
                    .unwrap();
            }
        }
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
        let receipt = reopened
            .submit_control_command(request, human_source(&reopened))
            .unwrap();
        assert_eq!(
            receipt.view().state,
            if accepted {
                ControlCommandState::Failed
            } else {
                ControlCommandState::Rejected
            }
        );
        assert_eq!(
            reopened.snapshot().unwrap().contract().activations().len(),
            1
        );
        assert_eq!(
            reopened.snapshot().unwrap().contract().state(),
            Some(LogicalTurnState::NeedsAttention)
        );
        assert!(reopened.lock().unwrap().bound.is_empty());
    }
}

#[tokio::test]
async fn retry_rejects_forged_previous_identity_before_accepting_the_exact_failed_target() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Pending, true));
    let prepared = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(
                &fixture,
                provider.clone(),
                Arc::new(CountingTool::default()),
            ),
        )
        .unwrap();
    let run = tokio::spawn(prepared.run());
    tokio::time::timeout(Duration::from_secs(3), provider.started.notified())
        .await
        .unwrap();
    fixture
        .controller
        .stop_activation(&fixture.activation)
        .unwrap();
    let failed = tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!failed.accepted);
    let before = fixture.controller.snapshot().unwrap();
    assert_eq!(
        before.contract().activations()[0].state,
        ActivationState::Failed
    );
    let input = retry_input(&fixture);
    let mut fabricated = fixture.activation.clone();
    fabricated.activation_id = ActivationId::new("not-the-failed-activation").unwrap();
    let rejected = fixture
        .controller
        .submit_control_command(
            command_request(
                &fixture.controller,
                "reject-forged-retry-target",
                ControlParameters::RetryActivation {
                    activation: fabricated,
                    input: Box::new(input.clone()),
                    replay_decisions: vec![],
                },
            ),
            human_source(&fixture.controller),
        )
        .unwrap();
    assert_eq!(rejected.view().state, ControlCommandState::Rejected);
    let after = fixture.controller.snapshot().unwrap();
    assert_eq!(after.contract().revision(), before.contract().revision());
    assert_eq!(
        after.contract().activations(),
        before.contract().activations()
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let accepted = fixture
        .controller
        .submit_control_command(
            command_request(
                &fixture.controller,
                "accept-exact-retry-target",
                ControlParameters::RetryActivation {
                    activation: fixture.activation.clone(),
                    input: Box::new(input.clone()),
                    replay_decisions: vec![],
                },
            ),
            human_source(&fixture.controller),
        )
        .unwrap();
    assert_eq!(accepted.view().state, ControlCommandState::Settled);
    let snapshot = fixture.controller.snapshot().unwrap();
    assert_eq!(snapshot.contract().activations().len(), 2);
    assert_eq!(snapshot.contract().activations()[1].input, input);
    assert_eq!(
        snapshot.contract().activations()[1].state,
        ActivationState::Running
    );
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "retry admission is not backend replay"
    );
}
