use super::*;
use axocoatl_actor::{AgentActor, DefaultAgentBehavior};
use axocoatl_core::{AgentConfig, AgentId, AgentInput, ChatMessage, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};
use axocoatl_session::control_authority::GrantLimits;
use axocoatl_session::execution_content::ExecutionRequestContent;
use axocoatl_session::execution_ownership::{LegacyFormatOwnership, UpgradedFormatOwnership};
use axocoatl_session::execution_store::ExecutionStoreOwner;
use axocoatl_token::TokenCounter;
use ractor::Actor;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_stream::Stream;

struct Counter;
impl TokenCounter for Counter {
    fn count_text(&self, text: &str) -> usize {
        text.len() / 4 + 1
    }
    fn count_messages(&self, messages: &[ChatMessage]) -> usize {
        messages
            .iter()
            .map(|message| {
                message
                    .text_content()
                    .map_or(1, |text| self.count_text(text))
            })
            .sum()
    }
    fn count_tool_definition(&self, definition: &serde_json::Value) -> usize {
        self.count_text(&definition.to_string())
    }
}

#[derive(Default)]
struct Provider {
    calls: AtomicUsize,
}
#[async_trait]
impl LlmProvider for Provider {
    fn provider_id(&self) -> &str {
        "controlled"
    }
    fn model_id(&self) -> &str {
        "controlled-model"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            ..Default::default()
        }
    }
    async fn chat(&self, _: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        unreachable!()
    }
    async fn chat_stream(
        &self,
        _: ChatRequest,
    ) -> std::result::Result<
        Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        let mut events = if first {
            vec![Ok(StreamEvent::ToolCallDelta {
                index: Some(0),
                id: "native-call".into(),
                name: Some("effect".into()),
                args_delta: r#"{"value":"actual"}"#.into(),
            })]
        } else {
            vec![Ok(StreamEvent::TextDelta {
                delta: "done".into(),
            })]
        };
        events.push(Ok(StreamEvent::Usage(TokenUsageStats::new(10, 2))));
        events.push(Ok(StreamEvent::Done {
            finish_reason: if first {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
        }));
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

#[derive(Default)]
struct CountingTool {
    count: AtomicUsize,
    started: tokio::sync::Notify,
    release: Option<Arc<tokio::sync::Notify>>,
}
#[async_trait]
impl axocoatl_tools::BuiltinTool for CountingTool {
    fn description(&self) -> &str {
        "Count actual effects"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    async fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, axocoatl_tools::ToolError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        if let Some(release) = &self.release {
            release.notified().await;
        }
        Ok(serde_json::json!({"actual":args}))
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    ownership: Arc<UpgradedFormatOwnership>,
    owner: ExecutionStoreOwner,
    controller: SessionDispatchController,
    activation: ActivationRef,
    profile: ExecutionProfile,
    config: AgentConfig,
}

fn fixture() -> Fixture {
    fixture_with_limits(
        GrantLimits {
            activations: 8,
            invocations: 32,
            tokens: 0,
            cost_microunits: 0,
        },
        "in-process-test",
    )
}

fn fixture_with_limits(limits: GrantLimits, isolation: &str) -> Fixture {
    fixture_with_config(
        limits,
        isolation,
        AgentConfig {
            id: AgentId::new("conversation"),
            name: "Counter".into(),
            provider: "controlled".into(),
            model: "controlled-model".into(),
            tools: vec!["effect".into()],
            ..Default::default()
        },
        "count once",
    )
}

fn fixture_with_config(
    limits: GrantLimits,
    isolation: &str,
    config: AgentConfig,
    input: &str,
) -> Fixture {
    fixture_with_captured_input(limits, isolation, config, input, |_, _| vec![])
}

fn fixture_with_captured_input(
    limits: GrantLimits,
    isolation: &str,
    config: AgentConfig,
    input: &str,
    capture: impl FnOnce(&mut ExecutionContentStore, &mut ExecutionRequestContent) -> Vec<EvidenceRef>,
) -> Fixture {
    fixture_with_captured_input_and_history(limits, isolation, config, input, capture, false)
}

fn fixture_with_captured_input_and_history(
    limits: GrantLimits,
    isolation: &str,
    config: AgentConfig,
    input: &str,
    capture: impl FnOnce(&mut ExecutionContentStore, &mut ExecutionRequestContent) -> Vec<EvidenceRef>,
    history: bool,
) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    if history {
        axocoatl_session::SessionTurnStore::open(root.path().join("session-history")).unwrap();
    }
    let ownership = Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let owner = ExecutionStoreOwner {
        workspace_id: "workspace".into(),
        session_id: SessionId::new("session").unwrap(),
    };
    let mut canonical = SessionExecutionStore::open(ownership.clone(), owner.clone()).unwrap();
    let mut content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    if history {
        let frontier = canonical.legacy_history_snapshot().unwrap();
        let retained = content.retain_legacy_history(&frontier).unwrap();
        canonical.seal_legacy_history(&retained).unwrap();
    }
    let turn = LogicalTurnId::new("turn").unwrap();
    let activation = ActivationRef {
        session_id: owner.session_id.clone(),
        turn_id: turn.clone(),
        execution_epoch_id: ExecutionEpochId::new("epoch").unwrap(),
        node_id: TurnNodeId::new("counter").unwrap(),
        generation: 1,
        activation_id: ActivationId::new("activation").unwrap(),
    };
    let profile = ExecutionProfile {
        definition: "counter".into(),
        provider: config.provider.clone(),
        model: config.model.clone(),
        isolation: isolation.into(),
        tools: config.tools.clone(),
    };
    let definition_id = AgentDefinitionId::new("counter").unwrap();
    let definition = content
        .retain_activation_evidence(ActivationEvidenceContent::Definition {
            definition_id: definition_id.clone(),
            revision: 1,
            profile: profile.clone(),
            configuration: serde_json::to_string(&config).unwrap(),
        })
        .unwrap();
    let approval = content
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: "Host approved the bounded counting tool".into(),
        })
        .unwrap();
    let policy = AuthorityGrant {
        id: "grant".into(),
        revision: 1,
        issuer_evidence: approval.reference().clone(),
        holder: activation.node_id.clone(),
        descendants: vec![],
        allow_stop_descendants: false,
        delegation: None,
        conditions: vec![],
        profiles: vec![profile.clone()],
        limits: limits.clone(),
        expires_at_ms: now_ms().unwrap() + 3_600_000,
    };
    let grant = content
        .retain_activation_evidence(ActivationEvidenceContent::Grant {
            policy: policy.clone(),
        })
        .unwrap();
    let budget = content
        .retain_activation_evidence(ActivationEvidenceContent::Budget { limits })
        .unwrap();
    let mut request_content = ExecutionRequestContent {
        turn_id: turn.clone(),
        recorded_at_unix_ms: now_ms().unwrap(),
        display_input: input.into(),
        effective_input: input.into(),
        context: vec![],
        target_definition: Some(definition_id.clone()),
        model: None,
    };
    let attachments = capture(&mut content, &mut request_content);
    let request = content.retain_request(request_content).unwrap();
    let definition = DefinitionSnapshotRef {
        definition_id,
        snapshot: definition.reference().clone(),
    };
    canonical
        .begin_with_request(
            TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("begin").unwrap(),
                expected_revision: 0,
                session_id: owner.session_id.clone(),
                turn_id: turn.clone(),
                event: TurnContractEvent::Begin {
                    epoch_id: activation.execution_epoch_id.clone(),
                    predecessor: None,
                    graph: TurnGraphSnapshot {
                        snapshot_id: GraphSnapshotId::new("graph").unwrap(),
                        revision: 1,
                        nodes: vec![GraphNode {
                            node_id: activation.node_id.clone(),
                            slot_id: SessionTeamSlotId::new("slot").unwrap(),
                            definition: definition.clone(),
                            conversation_id: NodeConversationId::new("conversation").unwrap(),
                            starting_savepoint: ConversationSavepoint::Empty,
                            required: true,
                        }],
                        dependencies: vec![],
                        conditions: vec![],
                    },
                },
            },
            &request,
        )
        .unwrap();
    canonical
        .append(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("start").unwrap(),
            expected_revision: 1,
            session_id: owner.session_id.clone(),
            turn_id: turn.clone(),
            event: TurnContractEvent::StartActivation {
                input: Box::new(ActivationInputManifest {
                    manifest_id: InputManifestId::new("input").unwrap(),
                    activation: activation.clone(),
                    definition,
                    conversation_id: NodeConversationId::new("conversation").unwrap(),
                    starting_savepoint: ConversationSavepoint::Empty,
                    parents: vec![],
                    guidance: vec![request.reference().clone()],
                    attachments,
                    repository: RepositoryInput::Unavailable,
                    budget: budget.reference().clone(),
                    grant: Some(GrantSnapshotRef {
                        grant_id: GrantId::new("grant").unwrap(),
                        revision: 1,
                        evidence: grant.reference().clone(),
                    }),
                    revision_context: None,
                }),
            },
        })
        .unwrap();
    drop(content);
    let controller = SessionDispatchController::open(canonical, turn).unwrap();
    controller.install_grant(policy).unwrap();
    Fixture {
        _root: root,
        ownership,
        owner,
        controller,
        activation,
        profile,
        config,
    }
}

#[path = "session_dispatch_run_tests.rs"]
mod run_tests;

#[path = "session_dispatch_provider_tests.rs"]
mod provider_tests;

#[path = "session_dispatch_ollama_tests.rs"]
mod ollama_tests;

fn bind(fixture: &Fixture) -> AgentRunControl {
    fixture
        .controller
        .bind_activation(
            fixture.activation.clone(),
            fixture.profile.clone(),
            serde_json::to_string(&fixture.config).unwrap(),
            AgentRunControl::new(axocoatl_actor::AgentRunId::new("activation")),
            DispatchReservation {
                tokens: 0,
                cost_microunits: 0,
            },
        )
        .unwrap()
}

async fn run_actor(
    config: AgentConfig,
    control: AgentRunControl,
    provider: Arc<Provider>,
    tool: Arc<CountingTool>,
) -> std::result::Result<
    axocoatl_actor::MeasuredAgentRunOutcome,
    axocoatl_actor::AgentExecutionFailure,
> {
    let mut executor = axocoatl_tools::ToolExecutor::new();
    executor.register_builtin("effect", tool);
    let behavior = DefaultAgentBehavior::new(provider, Arc::new(Counter))
        .with_tool_executor(Arc::new(executor));
    let (actor, handle) = AgentActor::spawn(None, AgentActor, (config, Box::new(behavior)))
        .await
        .unwrap();
    let result = axocoatl_actor::execute_agent_controlled_measured(
        &actor,
        AgentInput::text("count once"),
        control,
    )
    .await;
    actor.stop(None);
    tokio::time::timeout(std::time::Duration::from_secs(3), handle)
        .await
        .unwrap()
        .unwrap();
    result
}

#[tokio::test]
async fn durable_actor_dispatch_records_exact_bytes_and_settlement() {
    let fixture = fixture();
    let provider = Arc::new(Provider::default());
    let tool = Arc::new(CountingTool::default());
    run_actor(
        fixture.config.clone(),
        bind(&fixture),
        provider.clone(),
        tool.clone(),
    )
    .await
    .unwrap();
    assert_eq!(tool.count.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    let state = fixture.controller.lock().unwrap();
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    let invocation = &snapshot.contract().invocations()[0];
    assert_eq!(
        invocation.evidence.disposition(),
        EffectDisposition::OutcomeRecorded
    );
    let audit = state
        .audit
        .invocation(&invocation.invocation_id)
        .unwrap()
        .unwrap();
    let arguments = state
        .content
        .tool_arguments(&snapshot, &fixture.activation, &invocation.invocation_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &state.content.read_tool_arguments(&arguments).unwrap()
        )
        .unwrap(),
        serde_json::json!({"value":"actual"})
    );
    assert_eq!(audit.intent.arguments, *arguments.protected_arguments());
    assert!(audit.final_evidence.is_some());
    drop(state);
    let view = fixture.controller.control_plane().unwrap();
    let wire = serde_json::to_value(&view).unwrap();
    assert_eq!(
        wire["invocations"]["value"][0]["scope"],
        "durable_invocation_audit"
    );
    assert_eq!(
        wire["invocations"]["value"][0]["disposition"],
        "outcome_recorded"
    );
    let evidence = &view.nodes[0].activations[0].evidence;
    assert!(evidence.iter().any(|item| item.kind == "tool_started"));
    assert!(evidence.iter().any(|item| item.kind == "tool_result"));
}

#[tokio::test]
async fn lost_admission_acknowledgements_cause_zero_backend_dispatch_and_no_replay() {
    for cut in [
        TestFailure::CanonicalIntent,
        TestFailure::AuditIntent,
        TestFailure::AuthorityClaim,
    ] {
        let fixture = fixture();
        let control = bind(&fixture);
        fixture.controller.lock().unwrap().fail_at = Some(cut);
        let provider = Arc::new(Provider::default());
        let tool = Arc::new(CountingTool::default());
        assert!(run_actor(
            fixture.config.clone(),
            control,
            provider.clone(),
            tool.clone()
        )
        .await
        .is_err());
        assert_eq!(tool.count.load(Ordering::SeqCst), 0);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        let Fixture {
            _root,
            ownership,
            owner,
            controller,
            activation,
            profile,
            config,
        } = fixture;
        drop(controller);
        let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
        let reopened =
            SessionDispatchController::open(canonical, activation.turn_id.clone()).unwrap();
        let snapshot = reopened.snapshot().unwrap();
        assert_eq!(
            snapshot.contract().state(),
            Some(LogicalTurnState::NeedsAttention)
        );
        assert_eq!(
            snapshot.contract().invocations()[0].evidence.disposition(),
            EffectDisposition::OutcomeUnknown
        );
        assert!(reopened
            .bind_activation(
                activation,
                profile,
                serde_json::to_string(&config).unwrap(),
                AgentRunControl::new(axocoatl_actor::AgentRunId::new("retry")),
                DispatchReservation {
                    tokens: 0,
                    cost_microunits: 0
                }
            )
            .is_err());
    }
}

#[tokio::test]
async fn outcome_ack_failure_reconciles_after_reopen_without_redispatch() {
    for cut in [TestFailure::ContentResult, TestFailure::AuditOutcome] {
        let fixture = fixture();
        let control = bind(&fixture);
        fixture.controller.lock().unwrap().fail_at = Some(cut);
        let provider = Arc::new(Provider::default());
        let tool = Arc::new(CountingTool::default());
        assert!(run_actor(
            fixture.config.clone(),
            control,
            provider.clone(),
            tool.clone()
        )
        .await
        .is_err());
        assert_eq!(tool.count.load(Ordering::SeqCst), 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
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
            reopened.snapshot().unwrap().contract().invocations()[0]
                .evidence
                .disposition(),
            EffectDisposition::OutcomeRecorded
        );
        assert_eq!(tool.count.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn exact_stop_and_closed_turn_still_retain_claimed_late_outcome() {
    for lost_outcome_ack in [false, true] {
        let fixture = fixture();
        let control = bind(&fixture);
        if lost_outcome_ack {
            fixture.controller.lock().unwrap().fail_at = Some(TestFailure::ContentResult);
        }
        let release = Arc::new(tokio::sync::Notify::new());
        let tool = Arc::new(CountingTool {
            release: Some(release.clone()),
            ..Default::default()
        });
        let provider = Arc::new(Provider::default());
        let running = tokio::spawn(run_actor(
            fixture.config.clone(),
            control,
            provider.clone(),
            tool.clone(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(3), tool.started.notified())
            .await
            .unwrap();
        fixture
            .controller
            .stop_activation(&fixture.activation)
            .unwrap();
        // Stop closes dispatch authority; the host still has to record the
        // interrupted canonical epoch before it can close the logical turn.
        let snapshot = fixture.controller.snapshot().unwrap();
        fixture
            .controller
            .append_host_event(TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("interrupt").unwrap(),
                expected_revision: snapshot.contract().revision(),
                session_id: fixture.owner.session_id.clone(),
                turn_id: fixture.activation.turn_id.clone(),
                event: TurnContractEvent::InterruptEpoch {
                    epoch_id: fixture.activation.execution_epoch_id.clone(),
                },
            })
            .unwrap();
        let snapshot = fixture.controller.snapshot().unwrap();
        fixture
            .controller
            .append_host_event(TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("close").unwrap(),
                expected_revision: snapshot.contract().revision(),
                session_id: fixture.owner.session_id.clone(),
                turn_id: fixture.activation.turn_id.clone(),
                event: TurnContractEvent::Close {
                    closure: TurnClosure::Cancelled,
                },
            })
            .unwrap();
        assert!(!running.is_finished());
        release.notify_one();
        let result = running.await.unwrap();
        if lost_outcome_ack {
            assert!(result.is_err());
        } else {
            assert!(result.unwrap().outcome.is_cancelled());
        }
        let state = fixture.controller.lock().unwrap();
        let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
        assert_eq!(
            snapshot.contract().state(),
            Some(LogicalTurnState::Cancelled)
        );
        let invocation = &snapshot.contract().invocations()[0];
        assert_eq!(
            invocation.evidence.disposition(),
            EffectDisposition::OutcomeUnknown
        );
        assert_eq!(
            state
                .audit
                .invocation(&invocation.invocation_id)
                .unwrap()
                .unwrap()
                .disposition(),
            if lost_outcome_ack {
                EffectDisposition::OutcomeUnknown
            } else {
                EffectDisposition::OutcomeRecorded
            }
        );
        let closure_revision = snapshot.contract().revision();
        drop(state);
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
        let state = reopened.lock().unwrap();
        let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
        assert_eq!(snapshot.contract().revision(), closure_revision);
        assert_eq!(
            snapshot.contract().state(),
            Some(LogicalTurnState::Cancelled)
        );
        let invocation = &snapshot.contract().invocations()[0];
        assert_eq!(
            invocation.evidence.disposition(),
            EffectDisposition::OutcomeUnknown
        );
        assert_eq!(
            state
                .audit
                .invocation(&invocation.invocation_id)
                .unwrap()
                .unwrap()
                .disposition(),
            EffectDisposition::OutcomeRecorded
        );
        assert_eq!(tool.count.load(Ordering::SeqCst), 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn binding_rejects_configuration_profile_and_nonzero_unenforced_budget() {
    for case in 0..3 {
        let fixture = fixture();
        let mut profile = fixture.profile.clone();
        let mut configuration = serde_json::to_string(&fixture.config).unwrap();
        let mut reservation = DispatchReservation {
            tokens: 0,
            cost_microunits: 0,
        };
        match case {
            0 => configuration.push(' '),
            1 => profile.model = "foreign-model".into(),
            _ => reservation.tokens = 1,
        };
        assert!(fixture
            .controller
            .bind_activation(
                fixture.activation,
                profile,
                configuration,
                AgentRunControl::new(axocoatl_actor::AgentRunId::new("activation")),
                reservation
            )
            .is_err());
    }
}

#[test]
fn generic_begin_without_canonical_request_cannot_open_dispatch_adapter() {
    let prepared = fixture();
    let graph = prepared
        .controller
        .snapshot()
        .unwrap()
        .contract()
        .graph()
        .unwrap()
        .clone();
    let root = tempfile::tempdir().unwrap();
    let ownership = Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let mut canonical = SessionExecutionStore::open(ownership, prepared.owner.clone()).unwrap();
    canonical
        .append(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("unbound-begin").unwrap(),
            expected_revision: 0,
            session_id: prepared.owner.session_id,
            turn_id: prepared.activation.turn_id.clone(),
            event: TurnContractEvent::Begin {
                epoch_id: prepared.activation.execution_epoch_id,
                graph,
                predecessor: None,
            },
        })
        .unwrap();
    let error = SessionDispatchController::open(canonical, prepared.activation.turn_id)
        .err()
        .expect("generic contract fixtures must not admit live tool dispatch");
    assert!(error
        .to_string()
        .contains("canonical retained request binding"));
}

#[tokio::test]
async fn actual_audit_storage_identity_failure_prevents_backend_dispatch() {
    let fixture = fixture();
    let control = bind(&fixture);
    let audit_path = fixture
        .controller
        .lock()
        .unwrap()
        .canonical
        .path()
        .parent()
        .unwrap()
        .join("invocation-audit");
    std::fs::rename(&audit_path, audit_path.with_file_name("audit-moved")).unwrap();
    std::fs::create_dir(&audit_path).unwrap();
    let provider = Arc::new(Provider::default());
    let tool = Arc::new(CountingTool::default());
    assert!(run_actor(
        fixture.config.clone(),
        control,
        provider.clone(),
        tool.clone()
    )
    .await
    .is_err());
    assert_eq!(tool.count.load(Ordering::SeqCst), 0);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture
            .controller
            .snapshot()
            .unwrap()
            .contract()
            .invocations()[0]
            .evidence
            .disposition(),
        EffectDisposition::OutcomeUnknown
    );
    assert!(std::fs::read_dir(audit_path).unwrap().next().is_none());
}

#[tokio::test]
async fn actor_outside_the_bound_conversation_cannot_dispatch() {
    let fixture = fixture();
    let control = bind(&fixture);
    let mut foreign = fixture.config.clone();
    foreign.id = AgentId::new("another-conversation");
    let provider = Arc::new(Provider::default());
    let tool = Arc::new(CountingTool::default());
    assert!(run_actor(foreign, control, provider.clone(), tool.clone())
        .await
        .is_err());
    assert_eq!(tool.count.load(Ordering::SeqCst), 0);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(fixture
        .controller
        .snapshot()
        .unwrap()
        .contract()
        .invocations()
        .is_empty());
}

#[tokio::test]
async fn truncated_result_keeps_known_status_and_full_digest_across_reopen() {
    for succeeded in [true, false] {
        let fixture = fixture();
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
                    id: "large".into(),
                    name: "effect".into(),
                    arguments: serde_json::json!({}),
                    provider_metadata: Default::default(),
                },
            })
            .await
            .unwrap();
        let payload = "x".repeat(MAX_RESULT_BYTES + 100);
        let returned = if succeeded {
            Ok(serde_json::Value::String(payload))
        } else {
            Err(payload)
        };
        let bytes = serde_json::to_vec(&returned).unwrap();
        permit
            .record_outcome(&ToolInvocationOutcome::Returned(returned))
            .await
            .unwrap();
        {
            let state = fixture.controller.lock().unwrap();
            let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
            let invocation = &snapshot.contract().invocations()[0];
            let arguments = state
                .content
                .tool_arguments(&snapshot, &fixture.activation, &invocation.invocation_id)
                .unwrap()
                .unwrap();
            let result = state.content.tool_result(&arguments).unwrap().unwrap();
            assert!(result.is_truncated());
            assert_eq!(result.original_byte_len(), bytes.len() as u64);
            assert_eq!(
                result.original_sha256(),
                format!("{:x}", Sha256::digest(&bytes))
            );
            assert_eq!(
                result.outcome(),
                if succeeded {
                    InvocationOutcome::Succeeded
                } else {
                    InvocationOutcome::Failed
                }
            );
            assert_eq!(
                invocation.evidence.disposition(),
                EffectDisposition::OutcomeRecorded
            );
        }
        drop(control);
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
            reopened.snapshot().unwrap().contract().invocations()[0]
                .evidence
                .disposition(),
            EffectDisposition::OutcomeRecorded
        );
    }
}
#[tokio::test]
async fn revoked_tool_admission_preserves_claimed_settlement_and_readable_history() {
    let fixture = fixture();
    let control = bind(&fixture);
    let boundary = control.execution_boundary().unwrap();
    let request = |group| ToolInvocationRequest {
        actor_id: fixture.config.id.to_string(),
        provider_id: fixture.profile.provider.clone(),
        model_id: fixture.profile.model.clone(),
        provider_response_group: group,
        provider_call_index: 0,
        provider_call_count: 1,
        tool_call: axocoatl_llm::ToolCall {
            id: format!("effect-{group}"),
            name: "effect".into(),
            arguments: serde_json::json!({"value":"retained"}),
            provider_metadata: Default::default(),
        },
    };
    let claimed = boundary.admit(&request(1)).await.unwrap();
    fixture.controller.revoke_control_grant("grant", 1).unwrap();
    let before = fixture.controller.snapshot().unwrap();
    let refused = boundary.admit(&request(2)).await;
    assert!(
        refused.is_err(),
        "revoked authority must never admit another tool"
    );
    assert_eq!(
        fixture.controller.snapshot().unwrap().contract().revision(),
        before.contract().revision()
    );
    {
        let state = fixture.controller.lock().unwrap();
        state
            .ready()
            .expect("a definitive pre-write refusal is not journal corruption");
        assert_eq!(state.authority.usage("grant").unwrap().invocations, 1);
    }
    claimed
        .record_outcome(&ToolInvocationOutcome::Returned(Ok(
            serde_json::json!({"settled":true}),
        )))
        .await
        .unwrap();
    let after = fixture.controller.snapshot().unwrap();
    assert_eq!(after.contract().invocations().len(), 1);
    assert_eq!(
        after.contract().invocations()[0].evidence.disposition(),
        EffectDisposition::OutcomeRecorded
    );
    let state = fixture.controller.lock().unwrap();
    state.ready().unwrap();
    assert_eq!(state.authority.usage("grant").unwrap().invocations, 1);
    assert!(state
        .authority
        .grant_status("grant")
        .unwrap()
        .revoked_at_revision
        .is_some());
}
