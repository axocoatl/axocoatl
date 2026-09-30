use super::*;
use crate::session_dispatch::{SessionGrantChange, SessionGrantDecision};
#[derive(Clone, Copy)]
enum Mode {
    Stop,
    StaleStop,
    StaleGraphStop,
    InspectStop,
    InspectFinish,
    InspectStaleStop,
    InspectChangedGraphStop,
    Finish,
    Expand,
}
struct Factory {
    inner: NestedFactory,
    mode: Mode,
    observed_calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl AutonomousActivationFactory for Factory {
    async fn resources(
        &self,
        input: &ActivationInputManifest,
    ) -> std::result::Result<AutonomousActivationResources, String> {
        let mut resources = self.inner.resources(input).await?;
        if resources.config.role == AgentRole::Coordinator {
            resources.provider = Arc::new(Provider {
                controller: self.inner.controller.clone(),
                activation: input.activation.clone(),
                mode: self.mode,
                calls: AtomicUsize::new(0),
                observed_calls: self.observed_calls.clone(),
            });
        }
        Ok(resources)
    }
}
struct Provider {
    controller: crate::session_dispatch::SessionDispatchController,
    activation: ActivationRef,
    mode: Mode,
    calls: AtomicUsize,
    observed_calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl LlmProvider for Provider {
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
        assert!(request
            .tools
            .iter()
            .any(|tool| tool.name == "coordination_control"));
        self.observed_calls.fetch_add(1, Ordering::SeqCst);
        let round = self.calls.fetch_add(1, Ordering::SeqCst);
        let from_inspect = matches!(
            self.mode,
            Mode::InspectStop
                | Mode::InspectFinish
                | Mode::InspectStaleStop
                | Mode::InspectChangedGraphStop
        );
        let calls = if from_inspect {
            let submit_round = if matches!(self.mode, Mode::InspectStaleStop) {
                2
            } else {
                1
            };
            let arguments = if round < submit_round {
                Some((
                    format!("inspect-call-{round}"),
                    serde_json::json!({"operation":"inspect"}),
                ))
            } else if round == submit_round {
                // Consume exactly the actual tool response a real model sees;
                // do not obtain a fresh revision from the controller.
                let response = request
                    .messages
                    .iter()
                    .find(|message| message.tool_call_id.as_deref() == Some("inspect-call-0"))
                    .expect("actual retained inspect response in provider conversation");
                let response: serde_json::Value =
                    serde_json::from_str(response.text_content().unwrap()).unwrap();
                let actions: Vec<crate::session_dispatch::HumanControlActionRequest> =
                    serde_json::from_value(response["legal_actions"].clone()).unwrap();
                let action = if matches!(self.mode, Mode::InspectFinish) {
                    crate::session_dispatch::HumanControlAction::Finish
                } else {
                    crate::session_dispatch::HumanControlAction::Stop
                };
                let mut offered = actions
                    .into_iter()
                    .find(|offered| {
                        offered.action == action
                            && (action == crate::session_dispatch::HumanControlAction::Finish
                                || offered.activation.as_ref() == Some(&self.activation))
                    })
                    .expect("requested operation must be offered under the actual delegated grant");
                offered.command_id = CommandId::new("self-command").unwrap();
                if matches!(self.mode, Mode::InspectChangedGraphStop) {
                    offered.expected_graph_revision += 1;
                }
                Some((
                    "control-call".into(),
                    serde_json::json!({"operation":"submit","request":offered}),
                ))
            } else {
                None
            };
            arguments
                .into_iter()
                .map(|(id, arguments)| axocoatl_core::ToolCall {
                    id,
                    name: "coordination_control".into(),
                    arguments,
                    provider_metadata: Default::default(),
                })
                .collect()
        } else if round == 0 {
            let snapshot = self.controller.snapshot().unwrap();
            let arguments = match self.mode {
                Mode::Stop | Mode::StaleStop | Mode::StaleGraphStop | Mode::Finish => {
                    let mut request = nested_action(
                        &self.controller,
                        "self-command",
                        if matches!(
                            self.mode,
                            Mode::Stop | Mode::StaleStop | Mode::StaleGraphStop
                        ) {
                            crate::session_dispatch::HumanControlAction::Stop
                        } else {
                            crate::session_dispatch::HumanControlAction::Finish
                        },
                        matches!(
                            self.mode,
                            Mode::Stop | Mode::StaleStop | Mode::StaleGraphStop
                        )
                        .then(|| self.activation.clone()),
                        vec![],
                    );
                    if matches!(self.mode, Mode::StaleStop) {
                        request.expected_turn_revision -= 1;
                    }
                    if matches!(self.mode, Mode::StaleGraphStop) {
                        request.expected_graph_revision -= 1;
                    }
                    serde_json::json!({"operation":"submit","request":request})
                }
                Mode::Expand => {
                    let policy = self
                        .controller
                        .session_grants()
                        .unwrap()
                        .grants
                        .into_iter()
                        .find(|status| status.policy.holder == self.activation.node_id)
                        .unwrap()
                        .policy;
                    let mut limits = policy.limits.clone();
                    limits.tokens += 10000;
                    let delegation = policy.delegation.unwrap();
                    let request = SessionGrantChange {
                        request_id: "expanded-budget".into(),
                        activation: self.activation.clone(),
                        grant_id: policy.id,
                        expected_grant_revision: policy.revision,
                        limits,
                        expires_at_ms: policy.expires_at_ms,
                        operations: delegation
                            .operations
                            .iter()
                            .map(|permission| permission.operation)
                            .collect(),
                        max_nodes: delegation.graph_limits.max_nodes,
                        max_edges: delegation.graph_limits.max_edges,
                        reason: "Complete one additional approved review".into(),
                    };
                    serde_json::json!({"operation":"expand","request":request})
                }
                Mode::InspectStop
                | Mode::InspectFinish
                | Mode::InspectStaleStop
                | Mode::InspectChangedGraphStop => unreachable!("inspect path handled above"),
            };
            assert_eq!(snapshot.contract().state(), Some(LogicalTurnState::Running));
            vec![axocoatl_core::ToolCall {
                id: "control-call".into(),
                name: "coordination_control".into(),
                arguments,
                provider_metadata: Default::default(),
            }]
        } else {
            vec![]
        };
        Ok(ChatResponse {
            content: if calls.is_empty() {
                "Reviewed results".into()
            } else {
                String::new()
            },
            tool_calls: calls,
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
        unreachable!("Coordinator uses bounded direct chat")
    }
}
async fn run_control(mode: Mode, approve: Option<bool>) {
    let fixture = coordinator_fixture(100000).await;
    let (controller, repository) = begin(&fixture, &fixture.request);
    let factory = Arc::new(Factory {
        inner: NestedFactory {
            controller: controller.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
            child_calls: Arc::new(AtomicUsize::new(0)),
            resolved: std::sync::Mutex::new(vec![]),
            gate: None,
        },
        mode,
        observed_calls: Arc::new(AtomicUsize::new(0)),
    });
    let NativeFirstTurnStart::Prepared(prepared) = finish_owned_setup(
        &fixture.registry,
        controller.clone(),
        repository,
        &fixture.request.source().unwrap(),
        crate::stream::StreamBus::new(64),
        factory,
    )
    .unwrap() else {
        panic!("new driver")
    };
    let human = async {
        if let Some(approve) = approve {
            let token = fixture
                .registry
                .session_team_token(fixture.request.session_id.as_str())
                .unwrap();
            let read = || {
                fixture
                    .registry
                    .with_session_team_grant_stores(&token, |canonical, content, held| {
                        crate::session_dispatch::retained_grant_view(
                            canonical,
                            content,
                            &fixture.request.turn_id,
                            held,
                        )
                        .map_err(|error| {
                            crate::error::DaemonError::SessionConflict(error.to_string())
                        })
                    })
                    .unwrap()
            };
            let proposal = loop {
                if let Some(proposal) = read().proposals.into_iter().next() {
                    break proposal;
                }
                tokio::task::yield_now().await;
            };
            let preview = controller.preview_grant_change(&proposal.request).unwrap();
            let decision = SessionGrantDecision {
                request: proposal.request,
                review_digest: preview.review_digest,
                approve,
                reason: if approve {
                    "Approved"
                } else {
                    "No extra authority"
                }
                .into(),
            };
            let retained = || {
                fixture
                    .registry
                    .with_session_team_grant_stores(&token, |canonical, content, held| {
                        crate::session_dispatch::retained_grant_decision(
                            canonical,
                            content,
                            &fixture.request.turn_id,
                            &decision,
                            held,
                        )
                        .map_err(|error| {
                            crate::error::DaemonError::SessionConflict(error.to_string())
                        })
                    })
                    .unwrap()
            };
            assert!(
                retained().is_none(),
                "new human decision must still use actual approval authority"
            );
            let first = controller.decide_grant_change(&decision).unwrap();
            let repeated = retained().expect("same host retry resolves through held authority");
            assert_eq!(
                serde_json::to_value(first).unwrap(),
                serde_json::to_value(repeated).unwrap()
            );
        }
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(prepared.run(), human)
    })
    .await
    .unwrap();
    let outcome = result.unwrap();
    match mode {
        Mode::Stop | Mode::InspectStop => {
            assert_eq!(
                outcome.snapshot.contract().state(),
                Some(LogicalTurnState::NeedsAttention)
            );
            let receipt = controller
                .control_command_receipt(&CommandId::new("self-command").unwrap())
                .unwrap()
                .unwrap();
            assert!(matches!(
                receipt.view().source,
                axocoatl_session::control_command::CommandSourceRecord::Agent { .. }
            ));
            assert_eq!(
                receipt.view().state,
                axocoatl_session::control_command::ControlCommandState::Settled
            );
            controller
                .with_team_stores(|canonical, _, _| {
                    let original = receipt.view().request.expected_turn_revision;
                    // The receipt keeps the model's original revision, while the
                    // exact own tool admission remains a separately recorded event.
                    let own_admission_revision =
                        original + u64::from(matches!(mode, Mode::InspectStop));
                    assert!(canonical.records().unwrap().iter().any(|record|
                    record.turn_id == receipt.view().request.turn_id
                    && record.expected_revision == own_admission_revision
                    && matches!(&record.event, TurnContractEvent::RecordIntent { activation, .. }
                        if activation.node_id == fixture.request.node_evidence[0].node_id)));
                    if matches!(mode, Mode::InspectStop) {
                        assert!(canonical
                            .records()
                            .unwrap()
                            .iter()
                            .any(|record| record.turn_id == receipt.view().request.turn_id
                                && record.expected_revision == original
                                && matches!(
                                    &record.event,
                                    TurnContractEvent::RecordOutcome {
                                        outcome: InvocationOutcome::Succeeded,
                                        ..
                                    }
                                )));
                    }
                    Ok(())
                })
                .unwrap();
        }
        Mode::StaleStop
        | Mode::StaleGraphStop
        | Mode::InspectStaleStop
        | Mode::InspectChangedGraphStop => {
            assert_eq!(
                outcome.snapshot.contract().state(),
                Some(LogicalTurnState::Completed)
            );
            let receipt = controller
                .control_command_receipt(&CommandId::new("self-command").unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(
                receipt.view().state,
                axocoatl_session::control_command::ControlCommandState::Rejected
            );
        }
        Mode::Finish | Mode::InspectFinish => {
            assert_eq!(
                outcome.snapshot.contract().state(),
                Some(LogicalTurnState::Completed)
            );
            let receipt = controller
                .control_command_receipt(&CommandId::new("self-command").unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(
                receipt.view().state,
                axocoatl_session::control_command::ControlCommandState::Settled
            );
        }
        Mode::Expand => {
            assert_eq!(
                outcome.snapshot.contract().state(),
                Some(LogicalTurnState::Completed)
            );
            let grant = controller
                .session_grants()
                .unwrap()
                .grants
                .into_iter()
                .find(|status| status.policy.id == "coordinator-grant")
                .unwrap();
            assert_eq!(
                grant.policy.revision,
                if approve == Some(true) { 2 } else { 1 }
            );
        }
    }
}
#[tokio::test]
async fn scoped_self_stop_returns_receipt_without_waiting_for_its_own_actor() {
    run_control(Mode::Stop, None).await;
}
#[tokio::test]
async fn scoped_normal_finish_waits_for_own_safe_settlement() {
    run_control(Mode::Finish, None).await;
}
#[tokio::test]
async fn exact_human_expansion_resumes_same_owner_and_retry_is_identical() {
    run_control(Mode::Expand, Some(true)).await;
}
#[tokio::test]
async fn denied_expansion_resumes_without_gaining_authority() {
    run_control(Mode::Expand, Some(false)).await;
}

#[tokio::test]
async fn scoped_control_tool_does_not_rebase_over_an_unrelated_canonical_revision() {
    run_control(Mode::StaleStop, None).await;
}
#[tokio::test]
async fn scoped_control_tool_does_not_rebase_a_stale_graph() {
    run_control(Mode::StaleGraphStop, None).await;
}

#[tokio::test]
async fn scoped_self_stop_uses_the_exact_offer_returned_in_the_previous_provider_round() {
    run_control(Mode::InspectStop, None).await;
}
#[tokio::test]
async fn scoped_finish_uses_the_exact_offer_returned_in_the_previous_provider_round() {
    run_control(Mode::InspectFinish, None).await;
}
#[tokio::test]
async fn scoped_control_offer_rejects_an_intervening_inspect_invocation() {
    run_control(Mode::InspectStaleStop, None).await;
}
#[tokio::test]
async fn scoped_control_offer_rejects_an_altered_graph_revision() {
    run_control(Mode::InspectChangedGraphStop, None).await;
}

async fn recover_lost_control_return(mode: Mode, expected_reconciliation: bool) {
    use axocoatl_session::execution_ownership::UpgradedFormatOwnership;
    use axocoatl_session::invocation_audit::{
        InvocationFinalEvidence, InvocationOutcomeSource, InvocationReplayPolicy,
    };
    let fixture = coordinator_fixture(100000).await;
    let (controller, repository) = begin(&fixture, &fixture.request);
    controller.lose_control_tool_outcome_for_test();
    let observed_calls = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(Factory {
        inner: NestedFactory {
            controller: controller.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
            child_calls: Arc::new(AtomicUsize::new(0)),
            resolved: std::sync::Mutex::new(vec![]),
            gate: None,
        },
        mode,
        observed_calls: observed_calls.clone(),
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
        panic!("new native driver")
    };
    assert!(
        tokio::time::timeout(Duration::from_secs(10), prepared.run())
            .await
            .unwrap()
            .is_err()
    );
    let (before, result) = controller.control_tool_recovery_evidence_for_test();
    assert!(result.is_none());
    assert!(before.final_evidence.is_none());
    assert!(matches!(
        before.intent.replay_policy,
        InvocationReplayPolicy::ReconcileBeforeReplay { .. }
    ));
    controller.reject_unbound_control_reconciliation_for_test();
    // The real driver's error path retains child settlement tasks. Join that
    // ownership before simulating process reconstruction in this same process.
    controller.close_registered_repository_admission().unwrap();
    controller
        .wait_for_registered_executions(Duration::from_secs(5))
        .await
        .unwrap();
    let snapshot = controller.snapshot().unwrap();
    let before_calls = observed_calls.load(Ordering::SeqCst);
    assert_eq!(before_calls, 1);
    drop(factory);
    drop(controller);
    let NativeFixture {
        repository,
        registry,
        request: _,
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
        let format = Arc::new(UpgradedFormatOwnership::open(repository._data.path()).unwrap());
        let stores =
            crate::bootstrap::session_recovery::recover_session_stores(format, &session).unwrap();
        crate::session_dispatch::SessionDispatchController::open_existing_retained(
            stores,
            snapshot.turn_id().clone(),
        )
        .map_err(|failure| failure.error)
        .unwrap()
    };
    let recovered = reopen();
    let (after, result) = recovered.control_tool_recovery_evidence_for_test();
    assert_eq!(after.intent, before.intent);
    assert_eq!(
        observed_calls.load(Ordering::SeqCst),
        before_calls,
        "reconstruction has no provider/executor"
    );
    if expected_reconciliation {
        assert!(matches!(
            after.final_evidence,
            Some(InvocationFinalEvidence::Outcome {
                source: InvocationOutcomeSource::Reconciliation,
                ..
            })
        ));
        let result = result.unwrap();
        assert_eq!(
            result["Ok"]["reconciliation"]["adapter"],
            "coordination-control-journal-v1"
        );
        let observed: axocoatl_session::control_command::CommandReceiptView =
            serde_json::from_value(result["Ok"]["receipt"].clone()).unwrap();
        assert_eq!(observed.request.command_id.as_str(), "self-command");
        assert!(matches!(
            observed.source,
            axocoatl_session::control_command::CommandSourceRecord::Agent { .. }
        ));
        // The recorded state is the actual receipt observed during lookup. It
        // does not claim that an Accepted command had already settled.
        assert_ne!(
            observed.state,
            axocoatl_session::control_command::ControlCommandState::Rejected
        );
    } else {
        assert!(
            after.final_evidence.is_none(),
            "a local command ID without accepted invocation binding is insufficient"
        );
        assert!(result.is_none());
    }
    let retained = recovered.snapshot().unwrap();
    let retained_value = serde_json::to_value(retained.contract()).unwrap();
    let evidence = after.final_evidence;
    let receipt = recovered
        .control_command_receipt(&CommandId::new("self-command").unwrap())
        .unwrap()
        .unwrap()
        .view()
        .clone();
    drop(recovered);
    let repeated = reopen();
    assert_eq!(
        repeated
            .control_tool_recovery_evidence_for_test()
            .0
            .final_evidence,
        evidence
    );
    assert_eq!(
        serde_json::to_value(repeated.snapshot().unwrap().contract()).unwrap(),
        retained_value
    );
    assert_eq!(
        repeated
            .control_command_receipt(&CommandId::new("self-command").unwrap())
            .unwrap()
            .unwrap()
            .view(),
        &receipt
    );
    assert_eq!(observed_calls.load(Ordering::SeqCst), before_calls);
}

#[tokio::test]
async fn native_control_receipt_reconciles_lost_tool_return_without_reexecuting() {
    recover_lost_control_return(Mode::Stop, true).await;
}
#[tokio::test]
async fn native_control_rejected_without_exact_admission_remains_unknown_after_restart() {
    recover_lost_control_return(Mode::StaleStop, false).await;
}
