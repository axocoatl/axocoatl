use super::*;
#[path = "session_dispatch_resume_tests.rs"]
mod resume_tests;
use axocoatl_core::MessageRole;
use axocoatl_llm::ProviderExecutionBounds;
use axocoatl_session::execution_content::{ExecutionUsage, OutputKind};
use std::time::Duration;

#[path = "session_dispatch_turn_stop_tests.rs"]
mod turn_stop_tests;

#[path = "session_dispatch_context_tests.rs"]
mod context_tests;

#[path = "session_dispatch_stream_tests.rs"]
mod stream_tests;

#[path = "session_dispatch_input_tests.rs"]
mod input_tests;

#[path = "session_dispatch_command_tests.rs"]
mod command_tests;

#[path = "session_dispatch_host_control_tests.rs"]
mod host_control_tests;

#[derive(Clone, Copy)]
enum RunProviderMode {
    Tools,
    ToolRounds(usize),
    Answer,
    NoUsage,
    Pending,
    FragmentedReasoning,
}

/// A finite local backend with a fixed response count, synthetic token usage,
/// and no monetary effect. It does not forward to a network provider.
struct RunProvider {
    mode: RunProviderMode,
    bounded: bool,
    calls: AtomicUsize,
    started: tokio::sync::Notify,
    controller: SessionDispatchController,
    activation: ActivationRef,
    durable_claims_at_dispatch: std::sync::Mutex<Vec<u32>>,
    requests: std::sync::Mutex<Vec<Vec<ChatMessage>>>,
}

impl RunProvider {
    fn new(fixture: &Fixture, mode: RunProviderMode, bounded: bool) -> Self {
        Self {
            mode,
            bounded,
            calls: AtomicUsize::new(0),
            started: tokio::sync::Notify::new(),
            controller: fixture.controller.clone(),
            activation: fixture.activation.clone(),
            durable_claims_at_dispatch: std::sync::Mutex::new(Vec::new()),
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl LlmProvider for RunProvider {
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
    fn execution_bounds(&self, _: &ChatRequest) -> Option<ProviderExecutionBounds> {
        self.bounded.then_some(ProviderExecutionBounds {
            token_limit: if matches!(self.mode, RunProviderMode::FragmentedReasoning) { 6000 } else { 100 },
            cost_microunits: 100,
            response_bytes: if matches!(self.mode, RunProviderMode::FragmentedReasoning) { 512 * 1024 } else { 8192 },
        })
    }
    async fn chat(&self, _: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        unreachable!("native actor uses the streaming interface")
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<
        Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let call_bound = match self.mode {
            RunProviderMode::ToolRounds(rounds) => rounds + 1,
            _ => 2,
        };
        assert!(call < call_bound, "controlled backend's fixed call bound exceeded");
        self.requests.lock().unwrap().push(request.messages.clone());
        let usage = self
            .controller
            .lock()
            .unwrap()
            .authority
            .provider_usage(&self.activation)
            .unwrap();
        self.durable_claims_at_dispatch
            .lock()
            .unwrap()
            .push(usage.calls);
        self.started.notify_one();
        if matches!(self.mode, RunProviderMode::Pending) {
            return Ok(Box::pin(tokio_stream::pending()));
        }
        // Like a real model, it cannot call a tool it was not offered.
        let tool_call = !request.tools.is_empty()
            && match self.mode {
                RunProviderMode::Tools => call == 0,
                RunProviderMode::ToolRounds(rounds) => call < rounds,
                _ => false,
            };
        let mut events = if tool_call {
            vec![Ok(StreamEvent::ToolCallDelta {
                index: Some(0),
                id: if call == 0 { "native-call".into() } else { format!("native-call-{call}") },
                name: Some("effect".into()),
                // Each later round is a new call: identical rounds with the
                // same result would end the loop as making no progress.
                args_delta: if call == 0 {
                    r#"{"value":"actual"}"#.into()
                } else {
                    format!(r#"{{"value":"actual","round":{call}}}"#)
                },
            })]
        } else {
            vec![Ok(StreamEvent::TextDelta {
                delta: "done".into(),
            })]
        };
        if matches!(self.mode, RunProviderMode::FragmentedReasoning) {
            events = (0..5000).map(|_| Ok(StreamEvent::ReasoningDelta { delta: "x".into() })).collect();
            events.push(Ok(StreamEvent::TextDelta { delta: "done".into() }));
        }
        if !matches!(self.mode, RunProviderMode::NoUsage) {
            events.push(Ok(StreamEvent::Usage(TokenUsageStats::new(10, if matches!(self.mode, RunProviderMode::FragmentedReasoning) { 5002 } else { 2 }))));
        }
        events.push(Ok(StreamEvent::Done {
            finish_reason: if tool_call {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
        }));
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

fn run_fixture() -> Fixture {
    fixture_with_limits(
        GrantLimits {
            activations: 8,
            invocations: 32,
            tokens: 1000,
            cost_microunits: 1000,
        },
        "in-process",
    )
}

fn resources(
    fixture: &Fixture,
    provider: Arc<RunProvider>,
    tool: Arc<CountingTool>,
) -> AutonomousActivationResources {
    let mut executor = axocoatl_tools::ToolExecutor::new();
    executor.register_builtin("effect", tool);
    AutonomousActivationResources {
        config: fixture.config.clone(),
        profile: fixture.profile.clone(),
        provider,
        counter: Arc::new(Counter),
        tools: Arc::new(executor),
    }
}

/// The 1.1.0 eval's grant ran out mid-loop and the failure read "LLM
/// provider error: Invalid request for ollama: provider admission failed:
/// Session dispatch: authority budget or storage capacity exhausted".
#[tokio::test]
async fn a_session_budget_short_of_a_tool_round_asks_for_the_answer_and_names_its_limit() {
    let limits = |tokens| GrantLimits {
        activations: 1,
        invocations: 32,
        tokens,
        cost_microunits: 10_000,
    };
    // Room for one call (100 tokens each) but not a tool round and an answer.
    let fixture = fixture_with_limits(limits(150), "in-process");
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    let settled = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert_eq!(settled.output.content().output.text, "done");
    assert_eq!(tool.count.load(Ordering::SeqCst), 0);
    let note = provider.requests.lock().unwrap()[0]
        .last()
        .and_then(ChatMessage::text_content)
        .unwrap()
        .to_string();
    assert!(
        note.contains("the Session budget has 150 tokens left and each model call reserves 100"),
        "{note}"
    );

    // No room for even that call: the failure names the limit plainly.
    let fixture = fixture_with_limits(limits(50), "in-process");
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let settled = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(!settled.accepted);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    let expected = "The Session budget for this Agent is used up: 50 of its 50 tokens remain \
                    and the next model call needs 100.";
    assert_eq!(settled.failure.as_deref(), Some(expected));
    assert_eq!(
        settled.output.content().output.text,
        format!("Activation failed: {expected}")
    );
}

#[tokio::test]
async fn native_tool_rounds_use_reviewed_capacity_and_still_stop_at_durable_invocation_budget() {
    // Six invocations pay for two tool rounds; the third call cannot pay for
    // another round and an answer, so it goes without tools and answers.
    for (invocations, rounds) in [(32, 12), (6, 2)] {
        let fixture = fixture_with_limits(
            GrantLimits {
                activations: 1,
                invocations,
                tokens: 10_000,
                cost_microunits: 10_000,
            },
            "in-process",
        );
        let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::ToolRounds(12), true));
        let tool = Arc::new(CountingTool::default());
        let settled = fixture.controller.prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        ).unwrap().run().await.unwrap();
        assert!(settled.accepted, "{:?}", settled.failure);
        assert_eq!(provider.calls.load(Ordering::SeqCst), rounds + 1);
        assert_eq!(tool.count.load(Ordering::SeqCst), rounds);
        assert_eq!(settled.output.content().output.text, "done");
        let wrapped_up = provider.requests.lock().unwrap()[rounds]
            .last()
            .and_then(ChatMessage::text_content)
            .is_some_and(|text| text.contains("tools are no longer available"));
        assert_eq!(wrapped_up, invocations == 6);
        let state = fixture.controller.lock().unwrap();
        let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
        assert_eq!(snapshot.contract().invocations().len(), tool.count.load(Ordering::SeqCst));
        assert_eq!(state.authority.provider_usage(&fixture.activation).unwrap().calls as usize,
            provider.calls.load(Ordering::SeqCst));
    }
}

/// Two benchmark leads stopped at exactly 128 tool rounds with budget left.
/// A granted activation now runs as many rounds as its grant pays for; the
/// next bound it meets is the Session's invocation audit, which keeps every
/// record. With no room left, the Agent is asked for its final answer, as
/// when its budget runs out, and the Session's runtime stays usable.
#[tokio::test]
async fn a_granted_activation_runs_past_128_rounds_and_answers_when_the_session_record_is_full() {
    let fixture = fixture_with_limits(
        GrantLimits {
            activations: 2,
            invocations: 1_000,
            tokens: 10_000_000,
            cost_microunits: 10_000_000,
        },
        "in-process",
    );
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::ToolRounds(300), true));
    let tool = Arc::new(CountingTool::default());
    let settled = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert_eq!(settled.output.content().output.text, "done");
    let rounds = tool.count.load(Ordering::SeqCst);
    assert!(rounds > 128, "{rounds}");
    assert_eq!(provider.calls.load(Ordering::SeqCst), rounds + 1);
    let note = provider.requests.lock().unwrap()[rounds]
        .last()
        .and_then(ChatMessage::text_content)
        .unwrap()
        .to_string();
    assert!(
        note.contains("the Session can record no more tool calls, so tools are no longer available"),
        "{note}"
    );
    let state = fixture.controller.lock().unwrap();
    assert!(state.poisoned.is_none(), "{:?}", state.poisoned);
    assert_eq!(state.audit.remaining_invocations(), 0);
    assert_eq!(state.tool_call_room(), 0);
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    assert_eq!(snapshot.contract().invocations().len(), rounds);
}

/// An Agent template's `max_tool_rounds` stops its activation at that many
/// rounds although its grant has room for more. The activation's evidence
/// names the limit, it is classed `round_limit`, and Continue is offered.
#[tokio::test]
async fn a_template_round_limit_stops_the_activation_and_offers_continue() {
    let fixture = fixture_with_config(
        GrantLimits {
            activations: 2,
            invocations: 32,
            tokens: 10_000,
            cost_microunits: 10_000,
        },
        "in-process",
        AgentConfig {
            id: AgentId::new("conversation"),
            name: "Counter".into(),
            provider: "controlled".into(),
            model: "controlled-model".into(),
            tools: vec!["effect".into()],
            max_tool_rounds: Some(3),
            ..Default::default()
        },
        "count once",
    );
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::ToolRounds(12), true));
    let tool = Arc::new(CountingTool::default());
    let settled = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(!settled.accepted);
    assert_eq!(tool.count.load(Ordering::SeqCst), 3);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 4);
    let expected = "This Agent reached its tool-round limit for this activation (3 rounds) and \
                    still asked for effect; those calls did not run. Run it again to go on, or \
                    narrow the task.";
    assert_eq!(settled.failure.as_deref(), Some(expected));
    assert_eq!(
        settled.output.content().output.text,
        format!("Activation failed: {expected}")
    );
    let snapshot = fixture.controller.snapshot().unwrap();
    let view = {
        let state = fixture.controller.lock().unwrap();
        state.content.project(&snapshot).unwrap()
    };
    let failure = view.activations[0].failure.as_ref().unwrap();
    assert_eq!((failure.class, failure.next_step), ("round_limit", "continue"));
    assert!(
        failure.explanation.starts_with("The Agent used all 3 tool rounds"),
        "{}",
        failure.explanation
    );
    // Once the epoch stops, the control plane offers to continue it.
    fixture
        .controller
        .append_host_event(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("interrupt-after-round-limit").unwrap(),
            expected_revision: snapshot.contract().revision(),
            session_id: fixture.activation.session_id.clone(),
            turn_id: fixture.activation.turn_id.clone(),
            event: TurnContractEvent::InterruptEpoch {
                epoch_id: fixture.activation.execution_epoch_id.clone(),
            },
        })
        .unwrap();
    let controls = fixture.controller.control_plane().unwrap().turn_controls.unwrap();
    assert!(controls.continue_turn.enabled, "{}", controls.continue_turn.reason);
    assert_eq!(controls.continuation_choices.len(), 1);
    assert_eq!(controls.continuation_choices[0].activation, fixture.activation);
    assert_eq!(controls.continuation_choices[0].state, "failed");
}

#[tokio::test]
async fn autonomous_run_accepts_actual_native_checkpoint_only_after_durable_provider_and_tool_evidence(
) {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    let prepared = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(tool.count.load(Ordering::SeqCst), 0);
    let settled = prepared.run().await.unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert!(settled.failure.is_none());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(tool.count.load(Ordering::SeqCst), 1);
    assert_eq!(*provider.durable_claims_at_dispatch.lock().unwrap(), [1, 2]);
    let state = fixture.controller.lock().unwrap();
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    let activation = &snapshot.contract().activations()[0];
    assert_eq!(activation.state, ActivationState::Accepted);
    assert_eq!(activation.checkpoint.as_ref(), settled.checkpoint.as_ref());
    assert_eq!(activation.output.as_ref(), Some(settled.output.reference()));
    let checkpoint = state
        .memory
        .checkpoint(settled.checkpoint.as_ref().unwrap())
        .unwrap();
    assert_eq!(checkpoint.agent_id, "conversation");
    assert_eq!(
        checkpoint.cumulative_token_usage,
        TokenUsageStats::new(20, 4)
    );
    assert!(checkpoint.cumulative_token_usage_known);
    let messages = &checkpoint.session_messages;
    assert_eq!(messages.first().unwrap().content, "count once");
    assert_eq!(messages.last().unwrap().content, "done");
    let native_call = messages
        .iter()
        .find(|message| !message.tool_calls.is_empty())
        .unwrap();
    assert_eq!(native_call.tool_calls[0].id, "native-call");
    assert_eq!(native_call.tool_calls[0].name, "effect");
    assert_eq!(
        native_call.tool_calls[0].arguments_json,
        r#"{"value":"actual"}"#
    );
    assert!(messages
        .iter()
        .any(|message| message.role == MessageRole::Tool
            && message.tool_call_id.as_deref() == Some("native-call")));
    let invocation = &snapshot.contract().invocations()[0];
    assert_eq!(
        invocation.evidence.disposition(),
        EffectDisposition::OutcomeRecorded
    );
    assert!(state
        .audit
        .invocation(&invocation.invocation_id)
        .unwrap()
        .unwrap()
        .final_evidence
        .is_some());
    let projection = state.content.project(&snapshot).unwrap();
    assert!(matches!(
        projection.activations[0].output,
        ContentResolution::Available { .. }
    ));
    drop(state);
    assert!(fixture
        .controller
        .admit_provider(
            &fixture.activation,
            "a".repeat(64),
            1,
            ProviderExecutionBounds {
                token_limit: 1,
                cost_microunits: 1,
                response_bytes: 64
            }
        )
        .is_err());
    drop(provider);
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
    let reopened = SessionDispatchController::open(canonical, activation.turn_id.clone()).unwrap();
    let state = reopened.lock().unwrap();
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    assert_eq!(
        snapshot.contract().activations()[0].state,
        ActivationState::Accepted
    );
    assert_eq!(
        state
            .memory
            .checkpoint(settled.checkpoint.as_ref().unwrap())
            .unwrap()
            .session_messages
            .last()
            .unwrap()
            .content,
        "done"
    );
    assert!(matches!(
        state.content.project(&snapshot).unwrap().activations[0].output,
        ContentResolution::Available { .. }
    ));
    assert_eq!(
        state.authority.provider_usage(&activation).unwrap().calls,
        2
    );
    assert_eq!(tool.count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn autonomous_run_refuses_unbounded_provider_before_any_paid_or_tool_dispatch() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, false));
    let tool = Arc::new(CountingTool::default());
    let result = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(!result.accepted);
    assert!(result
        .failure
        .as_deref()
        .unwrap()
        .contains("enforced provider"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(tool.count.load(Ordering::SeqCst), 0);
    assert!(provider
        .durable_claims_at_dispatch
        .lock()
        .unwrap()
        .is_empty());
    let state = fixture.controller.lock().unwrap();
    assert_eq!(
        state
            .authority
            .provider_usage(&fixture.activation)
            .unwrap()
            .calls,
        0
    );
    assert!(state
        .canonical
        .snapshot(&state.turn_id)
        .unwrap()
        .contract()
        .invocations()
        .is_empty());
    assert_eq!(result.output.content().output.kind, OutputKind::Partial);
    let checkpoint = state
        .memory
        .checkpoint(result.checkpoint.as_ref().unwrap())
        .unwrap();
    assert!(checkpoint.cumulative_token_usage_known);
    assert_eq!(checkpoint.cumulative_token_usage.total(), 0);
}

#[tokio::test]
async fn autonomous_run_keeps_missing_provider_usage_unknown_in_output_and_native_checkpoint() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::NoUsage, true));
    let tool = Arc::new(CountingTool::default());
    let result = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(result.accepted, "{:?}", result.failure);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool.count.load(Ordering::SeqCst), 0);
    assert!(matches!(&result.output.content().output.usage,
        ExecutionUsage::Unknown { known_subtotal } if known_subtotal.total() == 0));
    let state = fixture.controller.lock().unwrap();
    let checkpoint = state
        .memory
        .checkpoint(result.checkpoint.as_ref().unwrap())
        .unwrap();
    assert!(!checkpoint.cumulative_token_usage_known);
    assert_eq!(checkpoint.cumulative_token_usage.total(), 0);
    assert_eq!(checkpoint.session_messages.last().unwrap().content, "done");
    let usage = state.authority.provider_usage(&fixture.activation).unwrap();
    assert_eq!(usage.calls, 1);
    assert_eq!(usage.unsettled_calls, 0);
    assert!(!usage.tokens.complete);
}

#[tokio::test]
async fn autonomous_run_refuses_changed_config_and_profile_before_backend_calls() {
    for changed in ["configuration", "profile", "conversation", "model"] {
        let fixture = run_fixture();
        let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
        let tool = Arc::new(CountingTool::default());
        let mut resolved = resources(&fixture, provider.clone(), tool.clone());
        match changed {
            "configuration" => resolved.config.system_prompt = Some("unapproved prompt".into()),
            "profile" => resolved.profile.definition = "unapproved-definition".into(),
            "conversation" => resolved.config.id = AgentId::new("another-conversation"),
            "model" => resolved.config.model = "unapproved-model".into(),
            _ => unreachable!(),
        }
        assert!(
            fixture
                .controller
                .prepare_autonomous_activation(fixture.activation.clone(), resolved)
                .is_err(),
            "{changed}"
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0, "{changed}");
        assert_eq!(tool.count.load(Ordering::SeqCst), 0, "{changed}");
    }
}

#[tokio::test]
async fn autonomous_stop_during_claimed_tool_retains_outcome_and_diagnostic_checkpoint_without_accepting(
) {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let release = Arc::new(tokio::sync::Notify::new());
    let tool = Arc::new(CountingTool {
        release: Some(release.clone()),
        ..Default::default()
    });
    let prepared = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .unwrap();
    let run = tokio::spawn(prepared.run());
    tokio::time::timeout(Duration::from_secs(5), tool.started.notified())
        .await
        .unwrap();
    fixture
        .controller
        .stop_activation(&fixture.activation)
        .unwrap();
    release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!result.accepted);
    assert!(result.failure.is_some());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool.count.load(Ordering::SeqCst), 1);
    assert_eq!(result.output.content().output.kind, OutputKind::Partial);
    let state = fixture.controller.lock().unwrap();
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    assert_eq!(
        snapshot.contract().activations()[0].state,
        ActivationState::Failed
    );
    assert!(snapshot.contract().activations()[0].checkpoint.is_none());
    assert_eq!(
        snapshot.contract().invocations()[0].evidence.disposition(),
        EffectDisposition::OutcomeRecorded
    );
    let checkpoint = state
        .memory
        .checkpoint(result.checkpoint.as_ref().unwrap())
        .unwrap();
    assert_eq!(
        checkpoint.cumulative_token_usage,
        TokenUsageStats::new(10, 2)
    );
    assert!(checkpoint.cumulative_token_usage_known);
}

#[tokio::test]
async fn autonomous_stop_during_provider_call_retains_unknown_accounting_and_single_diagnostic_checkpoint(
) {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Pending, true));
    let tool = Arc::new(CountingTool::default());
    let prepared = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .unwrap();
    let run = tokio::spawn(prepared.run());
    tokio::time::timeout(Duration::from_secs(5), provider.started.notified())
        .await
        .unwrap();
    fixture
        .controller
        .stop_activation(&fixture.activation)
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!result.accepted);
    assert_eq!(result.output.content().output.kind, OutputKind::Partial);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool.count.load(Ordering::SeqCst), 0);
    let state = fixture.controller.lock().unwrap();
    let usage = state.authority.provider_usage(&fixture.activation).unwrap();
    assert_eq!(usage.calls, 1);
    assert_eq!(usage.unsettled_calls, 0);
    assert!(!usage.tokens.complete);
    let checkpoint = state
        .memory
        .checkpoint(result.checkpoint.as_ref().unwrap())
        .unwrap();
    assert!(!checkpoint.cumulative_token_usage_known);
    assert_eq!(checkpoint.cumulative_token_usage.total(), 0);
    assert!(state
        .canonical
        .snapshot(&state.turn_id)
        .unwrap()
        .contract()
        .activations()[0]
        .checkpoint
        .is_none());
}

#[test]
fn dropping_prepared_activation_closes_exact_dispatch_without_provider_work_or_rebinding() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    let prepared = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .unwrap();
    let control = fixture
        .controller
        .lock()
        .unwrap()
        .bound
        .get(&fixture.activation.activation_id)
        .unwrap()
        .control
        .clone();
    drop(prepared);
    assert!(control.is_cancelled());
    assert!(fixture
        .controller
        .admit_provider(
            &fixture.activation,
            "a".repeat(64),
            1,
            ProviderExecutionBounds {
                token_limit: 100,
                cost_microunits: 100,
                response_bytes: 8192
            }
        )
        .is_err());
    assert!(fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .is_err());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(tool.count.load(Ordering::SeqCst), 0);
    let state = fixture.controller.lock().unwrap();
    assert_eq!(
        state
            .authority
            .provider_usage(&fixture.activation)
            .unwrap()
            .calls,
        0
    );
    assert!(state
        .canonical
        .snapshot(&state.turn_id)
        .unwrap()
        .contract()
        .invocations()
        .is_empty());
}

#[test]
fn autonomous_input_persistence_failure_prevents_provider_and_tool_dispatch_and_fails_closed() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    let memory_root = fixture
        .controller
        .lock()
        .unwrap()
        .canonical
        .path()
        .parent()
        .unwrap()
        .join("activation-state");
    let state_file = memory_root.join("activation-state.json");
    let before = std::fs::read(&state_file).unwrap();
    let saved = memory_root.join("saved-state.json");
    std::fs::rename(&state_file, &saved).unwrap();
    std::fs::create_dir(&state_file).unwrap();
    assert!(fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .is_err());
    assert_eq!(std::fs::read(saved).unwrap(), before);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(tool.count.load(Ordering::SeqCst), 0);
    assert!(fixture.controller.lock().unwrap().poisoned.is_some());
    assert!(fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .is_err());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(tool.count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn linked_successor_restores_promoted_native_history_and_counts_prior_provider_usage_once() {
    let first = run_fixture();
    let provider = Arc::new(RunProvider::new(&first, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    let settled = first
        .controller
        .prepare_autonomous_activation(
            first.activation.clone(),
            resources(&first, provider.clone(), tool.clone()),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(settled.accepted);
    let snapshot = first.controller.snapshot().unwrap();
    first
        .controller
        .append_host_event(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("close-first").unwrap(),
            expected_revision: snapshot.contract().revision(),
            session_id: first.owner.session_id.clone(),
            turn_id: first.activation.turn_id.clone(),
            event: TurnContractEvent::Close {
                closure: TurnClosure::Completed,
            },
        })
        .unwrap();
    let (predecessor, committed, mut next_input, policy, original_messages) = {
        let mut state = first.controller.lock().unwrap();
        let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
        let promotion = state.memory.promote(&snapshot).unwrap();
        assert_eq!(promotion.selected.len(), 1);
        let committed = promotion.selected[0].committed.clone();
        let checkpoint = state.memory.checkpoint(&committed).unwrap();
        assert_eq!(
            checkpoint.cumulative_token_usage,
            TokenUsageStats::new(20, 4)
        );
        (
            snapshot.contract().closed_reference().unwrap(),
            committed,
            snapshot.contract().activations()[0].input.clone(),
            state.authority.grant_policy("grant").unwrap(),
            serde_json::to_value(checkpoint.session_messages).unwrap(),
        )
    };
    drop(provider);
    let Fixture {
        _root,
        ownership,
        owner,
        controller,
        profile,
        config,
        ..
    } = first;
    drop(controller);
    let mut canonical = SessionExecutionStore::open(ownership.clone(), owner.clone()).unwrap();
    let mut content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let turn_id = LogicalTurnId::new("successor").unwrap();
    let activation = ActivationRef {
        session_id: owner.session_id.clone(),
        turn_id: turn_id.clone(),
        execution_epoch_id: ExecutionEpochId::new("successor-epoch").unwrap(),
        node_id: TurnNodeId::new("counter").unwrap(),
        generation: 1,
        activation_id: ActivationId::new("successor-activation").unwrap(),
    };
    let request = content
        .retain_request(ExecutionRequestContent {
            turn_id: turn_id.clone(),
            recorded_at_unix_ms: now_ms().unwrap(),
            display_input: "explain the previous result".into(),
            effective_input: "explain the previous result".into(),
            context: vec![],
            target_definition: Some(next_input.definition.definition_id.clone()),
            model: None,
        })
        .unwrap();
    let savepoint = ConversationSavepoint::Checkpoint {
        checkpoint: Box::new(committed.clone()),
    };
    canonical
        .begin_with_request(
            TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("begin-successor").unwrap(),
                expected_revision: 0,
                session_id: owner.session_id.clone(),
                turn_id: turn_id.clone(),
                event: TurnContractEvent::Begin {
                    epoch_id: activation.execution_epoch_id.clone(),
                    predecessor: Some(predecessor.clone()),
                    graph: TurnGraphSnapshot {
                        snapshot_id: GraphSnapshotId::new("successor-graph").unwrap(),
                        revision: 1,
                        nodes: vec![GraphNode {
                            node_id: activation.node_id.clone(),
                            slot_id: SessionTeamSlotId::new("slot").unwrap(),
                            definition: next_input.definition.clone(),
                            conversation_id: next_input.conversation_id.clone(),
                            starting_savepoint: savepoint.clone(),
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
    next_input.manifest_id = InputManifestId::new("successor-input").unwrap();
    next_input.activation = activation.clone();
    next_input.starting_savepoint = savepoint;
    next_input.guidance = vec![request.reference().clone()];
    canonical
        .append(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("start-successor").unwrap(),
            expected_revision: 1,
            session_id: owner.session_id.clone(),
            turn_id: turn_id.clone(),
            event: TurnContractEvent::StartActivation {
                input: Box::new(next_input),
            },
        })
        .unwrap();
    drop(content);
    let controller = SessionDispatchController::open(canonical, turn_id).unwrap();
    controller.install_grant(policy).unwrap();
    let second = Fixture {
        _root,
        ownership,
        owner,
        controller,
        activation,
        profile,
        config,
    };
    let provider = Arc::new(RunProvider::new(&second, RunProviderMode::Answer, true));
    let settled = second
        .controller
        .prepare_autonomous_activation(
            second.activation.clone(),
            resources(&second, provider.clone(), tool.clone()),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        tool.count.load(Ordering::SeqCst),
        1,
        "old native tool must not replay"
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]
        .iter()
        .any(|message| message.role == MessageRole::User
            && message.text_content() == Some("count once")));
    assert!(requests[0].iter().any(|message| message
        .tool_calls
        .iter()
        .any(|call| call.id == "native-call" && call.name == "effect")));
    assert!(requests[0]
        .iter()
        .any(|message| message.role == MessageRole::Tool
            && message.tool_call_id.as_deref() == Some("native-call")));
    assert_eq!(
        requests[0].last().unwrap().text_content(),
        Some("explain the previous result")
    );
    drop(requests);
    let state = second.controller.lock().unwrap();
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    assert_eq!(snapshot.contract().predecessor(), Some(&predecessor));
    let checkpoint = state
        .memory
        .checkpoint(settled.checkpoint.as_ref().unwrap())
        .unwrap();
    let prefix_len = original_messages.as_array().unwrap().len();
    assert_eq!(
        serde_json::to_value(&checkpoint.session_messages[..prefix_len]).unwrap(),
        original_messages
    );
    assert_eq!(
        checkpoint.session_messages[prefix_len].content,
        "explain the previous result"
    );
    assert_eq!(checkpoint.session_messages.len(), prefix_len + 2);
    assert_eq!(
        checkpoint.cumulative_token_usage,
        TokenUsageStats::new(30, 6)
    );
    assert!(checkpoint.cumulative_token_usage_known);
    assert!(matches!(&settled.output.content().output.usage,
        ExecutionUsage::Measured { usage } if usage == &TokenUsageStats::new(10, 2)));
    assert_eq!(
        state
            .memory
            .committed_reference(&NodeConversationId::new("conversation").unwrap())
            .unwrap(),
        Some(committed)
    );
}
