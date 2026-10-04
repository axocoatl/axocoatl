use super::*;
use axocoatl_core::MessageRole;
use axocoatl_llm::ProviderExecutionBounds;
use axocoatl_session::execution_content::ExecutionRequestContent;

#[derive(Default)]
struct LifecycleProvider {
    requests: Mutex<Vec<Vec<ChatMessage>>>,
}
#[async_trait]
impl LlmProvider for LifecycleProvider {
    fn provider_id(&self) -> &str {
        "controlled"
    }
    fn model_id(&self) -> &str {
        "controlled-model"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            ..Default::default()
        }
    }
    fn execution_bounds(&self, _: &ChatRequest) -> Option<ProviderExecutionBounds> {
        Some(ProviderExecutionBounds {
            token_limit: 100,
            cost_microunits: 0,
            response_bytes: 8192,
        })
    }
    async fn chat(&self, _: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        unreachable!()
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<
        Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        self.requests.lock().unwrap().push(request.messages);
        Ok(Box::pin(tokio_stream::iter(vec![
            Ok(StreamEvent::TextDelta {
                delta: "retained first answer".into(),
            }),
            Ok(StreamEvent::Usage(TokenUsageStats::new(10, 2))),
            Ok(StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            }),
        ])))
    }
}
fn lifecycle_fixture() -> Fixture {
    fixture_with_limits(
        GrantLimits {
            activations: 8,
            invocations: 16,
            tokens: 1000,
            cost_microunits: 0,
        },
        "in-process",
    )
}
fn lifecycle_resources(
    fixture: &Fixture,
    provider: Arc<LifecycleProvider>,
) -> AutonomousActivationResources {
    AutonomousActivationResources {
        config: fixture.config.clone(),
        profile: fixture.profile.clone(),
        provider,
        counter: Arc::new(Counter),
        tools: Arc::new(axocoatl_tools::ToolExecutor::new()),
    }
}
async fn accept(fixture: &Fixture) -> SettledActivation {
    let result = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            lifecycle_resources(fixture, Arc::new(LifecycleProvider::default())),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(result.accepted, "{:?}", result.failure);
    result
}
fn closing(controller: &SessionDispatchController, closure: TurnClosure) -> TurnContractEnvelope {
    let snapshot = controller.snapshot().unwrap();
    TurnContractEnvelope {
        schema_version: TURN_CONTRACT_SCHEMA_VERSION,
        command_id: CommandId::new("close-lifecycle").unwrap(),
        expected_revision: snapshot.contract().revision(),
        session_id: snapshot.owner().session_id.clone(),
        turn_id: snapshot.turn_id().clone(),
        event: TurnContractEvent::Close { closure },
    }
}
fn successor(controller: &SessionDispatchController) -> SuccessorTurn {
    let snapshot = controller.snapshot().unwrap();
    let mut graph = snapshot.contract().graph().unwrap().clone();
    graph.snapshot_id = GraphSnapshotId::new("next-graph").unwrap();
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
    SuccessorTurn {
        command_id: CommandId::new("next-begin").unwrap(),
        turn_id: LogicalTurnId::new("next-turn").unwrap(),
        epoch_id: ExecutionEpochId::new("next-epoch").unwrap(),
        graph,
        request: ExecutionRequestContent {
            turn_id: LogicalTurnId::new("next-turn").unwrap(),
            recorded_at_unix_ms: 2,
            display_input: "next request".into(),
            effective_input: "next request".into(),
            context: vec![],
            target_definition: None,
            model: None,
        },
    }
}

#[tokio::test]
async fn accepted_close_promotion_successor_restores_real_conversation_and_reopens() {
    let fixture = lifecycle_fixture();
    let accepted = accept(&fixture).await;
    let closed = fixture
        .controller
        .close_and_promote(closing(&fixture.controller, TurnClosure::Completed))
        .unwrap();
    assert_eq!(
        closed.promotion().selected[0].accepted,
        accepted.checkpoint.unwrap()
    );
    let next = successor(&fixture.controller);
    let graph = next.graph.clone();
    let old_input = fixture
        .controller
        .snapshot()
        .unwrap()
        .contract()
        .activations()[0]
        .input
        .clone();
    let policy = fixture
        .controller
        .lock()
        .unwrap()
        .authority
        .grant_policy("grant")
        .unwrap();
    let Fixture {
        _root,
        ownership,
        owner,
        controller,
        config,
        profile,
        ..
    } = fixture;
    let controller = controller.begin_successor(next).unwrap();
    assert_eq!(
        controller.snapshot().unwrap().contract().state(),
        Some(LogicalTurnState::Running)
    );
    assert_eq!(
        controller.snapshot().unwrap().contract().predecessor(),
        Some(&closed.promotion().closure)
    );
    let mut input = old_input;
    input.manifest_id = InputManifestId::new("next-input").unwrap();
    input.activation.turn_id = LogicalTurnId::new("next-turn").unwrap();
    input.activation.execution_epoch_id = ExecutionEpochId::new("next-epoch").unwrap();
    input.activation.activation_id = ActivationId::new("next-activation").unwrap();
    input.starting_savepoint = graph.nodes[0].starting_savepoint.clone();
    input.guidance = vec![controller
        .snapshot()
        .unwrap()
        .request_ref()
        .unwrap()
        .clone()];
    controller.install_grant(policy).unwrap();
    controller
        .append_host_event(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("next-start").unwrap(),
            expected_revision: 1,
            session_id: owner.session_id.clone(),
            turn_id: input.activation.turn_id.clone(),
            event: TurnContractEvent::StartActivation {
                input: Box::new(input.clone()),
            },
        })
        .unwrap();
    let provider = Arc::new(LifecycleProvider::default());
    let result = controller
        .prepare_autonomous_activation(
            input.activation,
            AutonomousActivationResources {
                config,
                profile,
                provider: provider.clone(),
                counter: Arc::new(Counter),
                tools: Arc::new(axocoatl_tools::ToolExecutor::new()),
            },
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(result.accepted, "{:?}", result.failure);
    let requests = provider.requests.lock().unwrap();
    assert!(requests[0]
        .iter()
        .any(|message| message.role == MessageRole::Assistant
            && message.text_content() == Some("retained first answer")));
    assert!(requests[0]
        .iter()
        .any(|message| message.role == MessageRole::User
            && message.text_content() == Some("next request")));
    drop(requests);
    let mut close = closing(&controller, TurnClosure::Completed);
    close.command_id = CommandId::new("next-close").unwrap();
    let final_turn = controller.close_and_promote(close).unwrap();
    drop(controller);
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    let reopened =
        SessionDispatchController::open(canonical, LogicalTurnId::new("next-turn").unwrap())
            .unwrap();
    assert_eq!(
        reopened
            .lock()
            .unwrap()
            .memory
            .promotion(final_turn.snapshot())
            .unwrap()
            .unwrap(),
        *final_turn.promotion()
    );
    drop(reopened);
    drop(_root);
}

#[tokio::test]
async fn lost_close_or_promotion_ack_recovers_exact_decision_without_provider_replay() {
    for cut in [TestFailure::CanonicalClose, TestFailure::Promotion] {
        let fixture = lifecycle_fixture();
        let accepted = accept(&fixture).await;
        let envelope = closing(&fixture.controller, TurnClosure::Completed);
        fixture.controller.lock().unwrap().fail_at = Some(cut);
        assert!(fixture
            .controller
            .close_and_promote(envelope.clone())
            .is_err());
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
        let finalized = reopened.close_and_promote(envelope).unwrap();
        assert_eq!(
            finalized.promotion().selected[0].accepted,
            accepted.checkpoint.unwrap()
        );
        assert_eq!(
            reopened
                .lock()
                .unwrap()
                .authority
                .provider_usage(&accepted.activation)
                .unwrap()
                .calls,
            1
        );
        drop(reopened);
        drop(_root);
    }
}

#[tokio::test]
async fn partial_promotion_failure_blocks_owner_and_reopen_finishes_recorded_heads() {
    let fixture = lifecycle_fixture();
    accept(&fixture).await;
    let root = fixture
        .controller
        .lock()
        .unwrap()
        .canonical
        .path()
        .parent()
        .unwrap()
        .join("activation-state");
    let blocked = root
        .join("heads")
        .join(format!("{:x}.json", Sha256::digest(b"conversation")));
    std::fs::create_dir(&blocked).unwrap();
    assert!(fixture
        .controller
        .close_and_promote(closing(&fixture.controller, TurnClosure::Completed))
        .is_err());
    // The durable decision is the journal's last record; its completion is not.
    let journal = std::fs::read(root.join("activation-state.active.jsonl")).unwrap();
    let last: serde_json::Value = serde_json::from_slice(
        journal
            .split(|byte| *byte == b'\n')
            .rfind(|line| !line.is_empty())
            .unwrap(),
    )
    .unwrap();
    assert!(last["record"]["promotion_prepared"].is_object());
    let Fixture {
        _root,
        ownership,
        owner,
        controller,
        activation,
        ..
    } = fixture;
    drop(controller);
    std::fs::remove_dir(&blocked).unwrap();
    let reopened = SessionDispatchController::open(
        SessionExecutionStore::open(ownership, owner).unwrap(),
        activation.turn_id,
    )
    .unwrap();
    assert!(reopened
        .lock()
        .unwrap()
        .memory
        .committed_checkpoint(&NodeConversationId::new("conversation").unwrap())
        .unwrap()
        .is_some());
    drop(reopened);
    drop(_root);
}

#[tokio::test]
async fn successor_refuses_live_clones_and_stale_savepoints() {
    let fixture = lifecycle_fixture();
    accept(&fixture).await;
    fixture
        .controller
        .close_and_promote(closing(&fixture.controller, TurnClosure::Completed))
        .unwrap();
    let next = successor(&fixture.controller);
    assert!(fixture.controller.clone().begin_successor(next).is_err());
    assert!(fixture
        .controller
        .snapshot()
        .unwrap()
        .contract()
        .state()
        .unwrap()
        .is_closed());
    let mut stale = successor(&fixture.controller);
    stale.graph.nodes[0].starting_savepoint = ConversationSavepoint::Empty;
    let Fixture {
        _root,
        ownership,
        owner,
        controller,
        activation,
        ..
    } = fixture;
    assert!(controller.begin_successor(stale).is_err());
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    assert!(canonical
        .turn(&LogicalTurnId::new("next-turn").unwrap())
        .unwrap()
        .is_none());
    assert!(canonical
        .turn(&activation.turn_id)
        .unwrap()
        .unwrap()
        .state()
        .unwrap()
        .is_closed());
    drop(canonical);
    drop(_root);
}

#[tokio::test]
async fn lost_successor_publication_never_allocates_or_runs_a_replacement() {
    for cut in [TestFailure::SuccessorRequest, TestFailure::SuccessorBegin] {
        let fixture = lifecycle_fixture();
        accept(&fixture).await;
        fixture
            .controller
            .close_and_promote(closing(&fixture.controller, TurnClosure::Completed))
            .unwrap();
        let next = successor(&fixture.controller);
        fixture.controller.lock().unwrap().fail_at = Some(cut);
        let Fixture {
            _root,
            ownership,
            owner,
            controller,
            ..
        } = fixture;
        assert!(controller.begin_successor(next).is_err());
        let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
        let next = canonical
            .turn(&LogicalTurnId::new("next-turn").unwrap())
            .unwrap();
        if cut == TestFailure::SuccessorBegin {
            let next = next.unwrap();
            assert_eq!(next.state(), Some(LogicalTurnState::NeedsAttention));
            assert!(next.activations().is_empty());
        } else {
            assert!(next.is_none());
        }
        drop(canonical);
        drop(_root);
    }
}

#[tokio::test]
async fn missing_older_promotion_cannot_move_a_newer_admitted_baseline() {
    let fixture = lifecycle_fixture();
    accept(&fixture).await;
    let close = closing(&fixture.controller, TurnClosure::Completed);
    let next = successor(&fixture.controller);
    assert_eq!(
        next.graph.nodes[0].starting_savepoint,
        ConversationSavepoint::Empty
    );
    let memory_path;
    {
        // Model the old host gap deliberately: canonical closure and successor
        // Begin happened without the new close/promotion protocol.
        let mut state = fixture.controller.lock().unwrap();
        state.canonical.append(close).unwrap();
        let predecessor = state
            .canonical
            .snapshot(&state.turn_id)
            .unwrap()
            .contract()
            .closed_reference()
            .unwrap();
        memory_path = state
            .canonical
            .path()
            .parent()
            .unwrap()
            .join("activation-state/activation-state.active.jsonl");
        let request = state.content.retain_request(next.request.clone()).unwrap();
        let session_id = state.canonical.owner().session_id.clone();
        state
            .canonical
            .begin_with_request(
                TurnContractEnvelope {
                    schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                    command_id: next.command_id.clone(),
                    expected_revision: 0,
                    session_id,
                    turn_id: next.turn_id.clone(),
                    event: TurnContractEvent::Begin {
                        epoch_id: next.epoch_id.clone(),
                        graph: next.graph.clone(),
                        predecessor: Some(predecessor),
                    },
                },
                &request,
            )
            .unwrap();
    }
    let before = std::fs::read(&memory_path).unwrap();
    let Fixture {
        _root,
        ownership,
        owner,
        controller,
        ..
    } = fixture;
    drop(controller);
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    assert!(SessionDispatchController::open(canonical, next.turn_id.clone()).is_err());
    assert_eq!(std::fs::read(&memory_path).unwrap(), before);
    drop(_root);
}

#[tokio::test]
async fn cancelling_successor_before_first_activation_promotes_an_empty_selection() {
    let fixture = lifecycle_fixture();
    accept(&fixture).await;
    let first = fixture
        .controller
        .close_and_promote(closing(&fixture.controller, TurnClosure::Completed))
        .unwrap();
    let next = successor(&fixture.controller);
    let Fixture {
        _root, controller, ..
    } = fixture;
    let controller = controller.begin_successor(next).unwrap();
    assert!(controller
        .snapshot()
        .unwrap()
        .contract()
        .activations()
        .is_empty());
    let mut close = closing(&controller, TurnClosure::Cancelled);
    close.command_id = CommandId::new("cancel-empty-successor").unwrap();
    let cancelled = controller.close_and_promote(close).unwrap();
    assert!(cancelled.promotion().selected.is_empty());
    assert_eq!(
        controller
            .lock()
            .unwrap()
            .memory
            .committed_reference(&NodeConversationId::new("conversation").unwrap())
            .unwrap(),
        Some(first.promotion().selected[0].committed.clone())
    );
    drop(controller);
    drop(_root);
}
