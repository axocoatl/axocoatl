use super::*;
use crate::session_dispatch::{
    HumanControlAction, HumanControlActionRequest, SessionDispatchController,
};
use axocoatl_session::control_command::{CommandReceiptView, ControlCommandState};

struct RevisionFactory {
    inner: NestedFactory,
    lose_return: bool,
    rounds: Arc<AtomicUsize>,
    observed: Arc<std::sync::Mutex<Vec<CommandReceiptView>>>,
}
#[async_trait::async_trait]
impl AutonomousActivationFactory for RevisionFactory {
    async fn resources(
        &self,
        input: &ActivationInputManifest,
    ) -> std::result::Result<AutonomousActivationResources, String> {
        let mut resources = self.inner.resources(input).await?;
        if resources.config.role == AgentRole::Coordinator {
            resources.provider = Arc::new(RevisionProvider {
                controller: self.inner.controller.clone(),
                activation: input.activation.clone(),
                rounds: self.rounds.clone(),
                observed: self.observed.clone(),
                request: std::sync::Mutex::new(None),
                lose_return: self.lose_return,
            });
        }
        Ok(resources)
    }
}
struct RevisionProvider {
    controller: SessionDispatchController,
    activation: ActivationRef,
    rounds: Arc<AtomicUsize>,
    observed: Arc<std::sync::Mutex<Vec<CommandReceiptView>>>,
    request: std::sync::Mutex<Option<HumanControlActionRequest>>,
    lose_return: bool,
}
#[async_trait::async_trait]
impl LlmProvider for RevisionProvider {
    fn provider_id(&self) -> &str {
        "ollama"
    }
    fn model_id(&self) -> &str {
        "test-model"
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
    async fn chat(&self, request: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        let arguments = match round {
            0 => Some(serde_json::json!({"operation":"inspect"})),
            1 => {
                let result = request
                    .messages
                    .iter()
                    .find(|message| message.tool_call_id.as_deref() == Some("revision-call-0"))
                    .unwrap();
                let value: serde_json::Value =
                    serde_json::from_str(result.text_content().unwrap()).unwrap();
                let offered: Vec<HumanControlActionRequest> =
                    serde_json::from_value(value["legal_actions"].clone()).unwrap();
                let mut revision = offered.into_iter().find(|request| request.action == HumanControlAction::Revise
                    && request.activation.as_ref().is_some_and(|target| target.node_id != self.activation.node_id))
                    .expect("an exact admitted inspect may offer granted descendant Revise after its own outcome");
                revision.command_id = CommandId::new("agent-revise-child").unwrap();
                revision.instruction =
                    Some("Review the accepted child again using the approved inputs".into());
                *self.request.lock().unwrap() = Some(revision.clone());
                if self.lose_return {
                    self.controller.lose_control_tool_outcome_for_test();
                }
                Some(serde_json::json!({"operation":"submit","request":revision}))
            }
            2 => {
                let message = request
                    .messages
                    .iter()
                    .find(|message| message.tool_call_id.as_deref() == Some("revision-call-1"))
                    .unwrap();
                let actual: CommandReceiptView =
                    serde_json::from_str(message.text_content().unwrap()).unwrap();
                assert_eq!(
                    actual.state,
                    ControlCommandState::Accepted,
                    "tool must return before canonical revision"
                );
                self.observed.lock().unwrap().push(actual);
                let repeated = self.request.lock().unwrap().clone().unwrap();
                Some(serde_json::json!({"operation":"submit","request":repeated}))
            }
            _ => {
                let message = request
                    .messages
                    .iter()
                    .find(|message| message.tool_call_id.as_deref() == Some("revision-call-2"))
                    .unwrap();
                let actual: CommandReceiptView =
                    serde_json::from_str(message.text_content().unwrap()).unwrap();
                assert_eq!(
                    actual.state,
                    ControlCommandState::Settled,
                    "exact retry reads the applied receipt"
                );
                self.observed.lock().unwrap().push(actual);
                let target = self
                    .request
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .activation
                    .clone()
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if self
                            .controller
                            .snapshot()
                            .unwrap()
                            .contract()
                            .current_accepted_activations()
                            .iter()
                            .any(|item| {
                                item.activation.node_id == target.node_id
                                    && item.activation.generation == 2
                            })
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                None
            }
        };
        Ok(ChatResponse {
            content: if arguments.is_none() {
                "Reviewed current child generation".into()
            } else {
                String::new()
            },
            tool_calls: arguments
                .into_iter()
                .map(|arguments| axocoatl_core::ToolCall {
                    id: format!("revision-call-{round}"),
                    name: "coordination_control".into(),
                    arguments,
                    provider_metadata: Default::default(),
                })
                .collect(),
            finish_reason: FinishReason::Stop,
            usage: TokenUsageStats::new(5, 5),
            model: "test-model".into(),
            provider: "ollama".into(),
        })
    }
    async fn chat_stream(
        &self,
        _: ChatRequest,
    ) -> std::result::Result<
        Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        unreachable!()
    }
}

async fn exercise_revision(lose_return: bool) {
    let fixture = coordinator_fixture_with_operations(
        100000,
        vec![
            DelegatedOperation::AddAgent,
            DelegatedOperation::ReviseActivation,
            DelegatedOperation::StopActivation,
            DelegatedOperation::RetryActivation,
            DelegatedOperation::FinishNormally,
        ],
    )
    .await;
    let (controller, repository) = begin(&fixture, &fixture.request);
    let factory = Arc::new(RevisionFactory {
        inner: NestedFactory {
            controller: controller.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
            child_calls: Arc::new(AtomicUsize::new(0)),
            resolved: std::sync::Mutex::new(vec![]),
            gate: None,
        },
        lose_return,
        rounds: Arc::new(AtomicUsize::new(0)),
        observed: Arc::new(std::sync::Mutex::new(vec![])),
    });
    let NativeFirstTurnStart::Prepared(prepared) = finish_owned_setup(
        &fixture.registry,
        controller.clone(),
        repository,
        &fixture.request.source().unwrap(),
        crate::stream::StreamBus::new(64),
        factory.clone(),
    )
    .unwrap() else {
        panic!("fresh driver")
    };
    let result = tokio::time::timeout(Duration::from_secs(10), prepared.run())
        .await
        .unwrap();
    let id = CommandId::new("agent-revise-child").unwrap();
    if !lose_return {
        let outcome = result.unwrap();
        assert_eq!(
            outcome.snapshot.contract().state(),
            Some(LogicalTurnState::Completed)
        );
        let receipt = controller.control_command_receipt(&id).unwrap().unwrap();
        assert_eq!(receipt.view().state, ControlCommandState::Settled);
        assert_eq!(
            factory.inner.child_calls.load(Ordering::SeqCst),
            3,
            "two original children and one exact revised generation"
        );
        assert_eq!(
            factory
                .observed
                .lock()
                .unwrap()
                .iter()
                .map(|view| view.state)
                .collect::<Vec<_>>(),
            vec![ControlCommandState::Accepted, ControlCommandState::Settled]
        );
        controller
            .with_team_stores(|canonical, _, _| {
                let records = canonical.records().unwrap();
                let revision = records
                    .iter()
                    .position(|record| {
                        matches!(record.event, TurnContractEvent::ReviseAccepted { .. })
                    })
                    .unwrap();
                assert!(matches!(
                    records[revision - 1].event,
                    TurnContractEvent::RecordOutcome {
                        outcome: InvocationOutcome::Succeeded,
                        ..
                    }
                ));
                assert_eq!(
                    records
                        .iter()
                        .filter(|record| matches!(
                            record.event,
                            TurnContractEvent::ReviseAccepted { .. }
                        ))
                        .count(),
                    1
                );
                Ok(())
            })
            .unwrap();
        return;
    }
    assert!(result.is_err());
    assert_eq!(factory.rounds.load(Ordering::SeqCst), 2);
    controller.close_registered_repository_admission().unwrap();
    controller
        .wait_for_registered_executions(Duration::from_secs(5))
        .await
        .unwrap();
    let turn = controller.snapshot().unwrap().turn_id().clone();
    let count = factory.inner.child_calls.clone();
    assert_eq!(count.load(Ordering::SeqCst), 2);
    drop(factory);
    drop(controller);
    let NativeFixture {
        repository,
        registry,
        ..
    } = fixture;
    drop(registry);
    let session = repository
        .owner
        .inner
        .sessions
        .lock()
        .await
        .get(&repository.owner.metadata().session_id)
        .unwrap()
        .clone();
    let reopen = || {
        let format = Arc::new(
            axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(
                repository._data.path(),
            )
            .unwrap(),
        );
        let stores =
            crate::bootstrap::session_recovery::recover_session_stores(format, &session).unwrap();
        SessionDispatchController::open_existing_retained(stores, turn.clone())
            .map_err(|failure| failure.error)
            .unwrap()
    };
    let recovered = reopen();
    let receipt = recovered
        .control_command_receipt(&id)
        .unwrap()
        .unwrap()
        .view()
        .clone();
    assert_eq!(
        receipt.state,
        ControlCommandState::Failed,
        "recovery cannot apply the unexecuted revision under a dead source"
    );
    assert!(recovered
        .snapshot()
        .unwrap()
        .contract()
        .activations()
        .iter()
        .all(|item| item.activation.generation == 1));
    assert_eq!(count.load(Ordering::SeqCst), 2);
    drop(recovered);
    assert_eq!(
        reopen()
            .control_command_receipt(&id)
            .unwrap()
            .unwrap()
            .view(),
        &receipt
    );
}
#[tokio::test]
async fn delegated_revision_waits_for_actual_tool_outcome_and_exact_retry_does_not_rerun() {
    exercise_revision(false).await;
}
#[tokio::test]
async fn delegated_revision_lost_tool_return_does_not_apply_or_reexecute_after_restart() {
    exercise_revision(true).await;
}
