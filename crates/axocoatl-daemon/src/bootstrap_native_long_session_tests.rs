//! One Agent through several owned native turns of one Session, each running
//! hundreds of tool calls, past the 128 rounds an activation once had and the
//! 256 invocations and 4096 content records a Session once held. Every record
//! stays readable: History and its export hold every tool call.
//! The finite local test provider scripts its tool calls and answers and
//! reports deterministic synthetic usage. The tool is a host invocation tool
//! registered under the `web_search` name that only counts its calls; it
//! searches nothing. This fixture is not a claim about an external model.
use super::*;
use axocoatl_core::{ChatMessage, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    ProviderExecutionBounds, StreamEvent,
};
use axocoatl_session::execution_content::{ActivationStreamPayload, ContentResolution};
use axocoatl_session::session_history::{HistoryVisibility, SessionHistoryEntry};
use std::pin::Pin;
use tokio_stream::Stream;

/// The host tool name the counting tool is registered under.
const TOOL: &str = "web_search";

struct Counter;
impl axocoatl_token::TokenCounter for Counter {
    fn count_text(&self, text: &str) -> usize {
        text.len().div_ceil(4)
    }
    fn count_messages(&self, messages: &[ChatMessage]) -> usize {
        messages
            .iter()
            .map(|message| self.count_text(&serde_json::to_string(&message.content).unwrap()))
            .sum::<usize>()
            + 4
    }
    fn count_tool_definition(&self, tool: &serde_json::Value) -> usize {
        self.count_text(&tool.to_string())
    }
}

type EventStream =
    Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>;

/// Asks for the counting tool on each of its first `tool_calls` calls, each
/// time with new arguments, then answers with its turn's name.
struct ToolCallingProvider {
    turn: String,
    tool_calls: usize,
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl LlmProvider for ToolCallingProvider {
    fn provider_id(&self) -> &str {
        "ollama"
    }
    fn model_id(&self) -> &str {
        "test-model"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
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
    /// A byte estimate: the shared tokenizer over a conversation of
    /// thousands of tool messages on every call would only slow the test.
    fn count_tokens(&self, request: &ChatRequest) -> usize {
        serde_json::to_vec(&request.messages).unwrap().len() / 4
    }
    async fn chat(&self, _: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        unreachable!("Agents stream through DefaultAgentBehavior")
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<EventStream, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(call <= self.tool_calls, "the fixed call count is exceeded");
        let tool_call = call < self.tool_calls && !request.tools.is_empty();
        let mut events = if tool_call {
            vec![StreamEvent::ToolCallDelta {
                index: Some(0),
                id: format!("{}-call-{call}", self.turn),
                name: Some(TOOL.into()),
                args_delta: format!(r#"{{"turn":"{}","call":{call}}}"#, self.turn),
            }]
        } else {
            vec![StreamEvent::TextDelta {
                delta: format!("{} done", self.turn),
            }]
        };
        events.push(StreamEvent::Usage(TokenUsageStats::new(5, 5)));
        events.push(StreamEvent::Done {
            finish_reason: if tool_call {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
        });
        Ok(Box::pin(tokio_stream::iter(events.into_iter().map(Ok))))
    }
}

/// Counts its calls and returns its arguments.
struct CountingTool(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl axocoatl_tools::BuiltinTool for CountingTool {
    fn description(&self) -> &str {
        "Count one call"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    async fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, axocoatl_tools::ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::json!({ "counted": args }))
    }
}

/// The counting tool as a host invocation tool: every admitted call gets it.
struct CountingHostTool(Arc<AtomicUsize>);
impl crate::session_dispatch::HostInvocationTool for CountingHostTool {
    fn name(&self) -> &'static str {
        TOOL
    }
    fn definition(&self) -> Arc<dyn axocoatl_tools::BuiltinTool> {
        Arc::new(crate::session_dispatch::HostToolDefinition::new(
            TOOL,
            "Count one call",
            serde_json::json!({"type":"object"}),
            axocoatl_llm::ConcurrencyPolicy::default(),
        ))
    }
    fn refusal(&self, _: &ExecutionProfile) -> Option<String> {
        None
    }
    fn bind(
        &self,
        _: crate::session_dispatch::HostInvocationContext,
    ) -> Arc<dyn axocoatl_tools::BuiltinTool> {
        Arc::new(CountingTool(self.0.clone()))
    }
}

struct ToolCallingFactory {
    controller: crate::session_dispatch::SessionDispatchController,
    calls: usize,
}
#[async_trait::async_trait]
impl AutonomousActivationFactory for ToolCallingFactory {
    async fn resources(
        &self,
        input: &ActivationInputManifest,
    ) -> std::result::Result<AutonomousActivationResources, String> {
        let (mut config, profile) = self
            .controller
            .with_team_stores(|_, content, _| {
                let ActivationEvidenceContent::Definition {
                    configuration,
                    profile,
                    ..
                } = content
                    .resolve_activation_evidence(&input.definition.snapshot)
                    .unwrap()
                else {
                    panic!("exact definition")
                };
                Ok((
                    serde_json::from_str::<AgentConfig>(&configuration).unwrap(),
                    profile,
                ))
            })
            .map_err(|error| error.to_string())?;
        config.id = AgentId::new(input.conversation_id.as_str());
        Ok(AutonomousActivationResources {
            provider: Arc::new(ToolCallingProvider {
                turn: input.activation.turn_id.as_str().to_owned(),
                tool_calls: self.calls,
                calls: AtomicUsize::new(0),
            }),
            config,
            profile,
            counter: Arc::new(Counter),
            tools: Arc::new(axocoatl_tools::ToolExecutor::new()),
        })
    }
}

/// The request of turn `index`: the fixture's first turn, then successors.
fn turn_request(fixture: &NativeFixture, index: usize) -> NativeFirstTurnRequest {
    let mut request = fixture.request.clone();
    if index == 0 {
        return request;
    }
    request.turn_id = LogicalTurnId::new(format!("long-turn-{index}")).unwrap();
    request.request.turn_id = request.turn_id.clone();
    request.request.display_input = format!("Do part {index}");
    request.request.effective_input = request.request.display_input.clone();
    request.command_id = CommandId::new(format!("long-turn-{index}-begin")).unwrap();
    request.epoch_id = ExecutionEpochId::new(format!("long-turn-{index}-epoch")).unwrap();
    request.graph_snapshot_id = GraphSnapshotId::new(format!("long-turn-{index}-graph")).unwrap();
    request
}

/// Begin a successor turn on the Session's released repository, as a new
/// request after a finished turn does.
async fn begin_successor(
    fixture: &NativeFixture,
    owner: &mut SessionRepositoryOwner,
    request: &NativeFirstTurnRequest,
) -> (
    crate::session_dispatch::SessionDispatchController,
    EvidenceRef,
) {
    let session = request.session_id.as_str();
    assert!(fixture
        .registry
        .native_reacquisition_needed(session)
        .unwrap());
    let token = fixture.registry.prepare_reacquisition(session).unwrap();
    let fresh = owner.reacquire_between_turns().await.unwrap();
    fixture
        .registry
        .complete_reacquisition(token, fresh.clone())
        .unwrap();
    *owner = fresh;
    let team = fixture.registry.session_team_token(session).unwrap();
    let setup = prepare_admission(
        &fixture.registry,
        &team,
        &SecureDir::open(fixture.repository._data.path()).unwrap(),
        request,
        &request.source().unwrap(),
    )
    .unwrap();
    fixture
        .registry
        .begin_native_successor_checked(
            session,
            SuccessorTurn {
                command_id: request.command_id.clone(),
                turn_id: request.turn_id.clone(),
                epoch_id: request.epoch_id.clone(),
                graph: setup.content.graph.clone(),
                request: request.request.clone(),
            },
            |canonical, content, memory| {
                verify_selected_team(
                    canonical,
                    content,
                    memory,
                    request.expected_team_revision,
                    &request.turn_id,
                    &setup.content.graph,
                    None,
                )
            },
        )
        .unwrap()
}

/// Run `turns` turns of `calls` tool calls each through one Session, then
/// read every turn and tool call back from History and its export. Returns
/// the Session's content records and their sealed segments.
async fn run_long_session(turns: usize, calls: usize) -> (u64, usize) {
    let fixture = native_team_fixture_with_limits(
        GrantLimits {
            activations: 2,
            // Each round is a model call and a tool call.
            invocations: 2 * calls as u32 + 10,
            tokens: 1_000_000,
            cost_microunits: 0,
        },
        "Exact test host approval",
        &[&[TOOL]],
        &[],
    )
    .await;
    let session = fixture.request.session_id.as_str().to_owned();
    let effects = Arc::new(AtomicUsize::new(0));
    let mut owner = fixture.repository.owner.clone();
    // One daemon stream bus serves every turn of the Session.
    let bus = crate::stream::StreamBus::new(64);
    for index in 0..turns {
        let request = turn_request(&fixture, index);
        let (controller, repository) = if index == 0 {
            begin(&fixture, &request)
        } else {
            begin_successor(&fixture, &mut owner, &request).await
        };
        controller
            .register_host_invocation_tool(Arc::new(CountingHostTool(effects.clone())))
            .unwrap();
        let factory = Arc::new(ToolCallingFactory {
            controller: controller.clone(),
            calls,
        });
        let NativeFirstTurnStart::Prepared(prepared) = finish_owned_setup(
            &fixture.registry,
            controller.clone(),
            repository,
            &request.source().unwrap(),
            bus.clone(),
            factory,
        )
        .unwrap() else {
            panic!("owned native driver")
        };
        let outcome = tokio::time::timeout(Duration::from_secs(900), prepared.run())
            .await
            .expect("the turn settles")
            .unwrap();
        let contract = outcome.snapshot.contract();
        assert_eq!(
            contract.state(),
            Some(LogicalTurnState::Completed),
            "turn {index}"
        );
        assert_eq!(contract.invocations().len(), calls, "turn {index}");
        assert_eq!(effects.load(Ordering::SeqCst), calls * (index + 1));
    }
    let total = turns * calls;

    // History holds every turn and every tool call, in order.
    let history = fixture
        .registry
        .history_snapshot(&session)
        .unwrap()
        .unwrap();
    let executed: Vec<_> = history
        .entries(HistoryVisibility::Visible)
        .into_iter()
        .filter_map(|entry| match entry {
            SessionHistoryEntry::ExecutionV2(turn) => Some(turn),
            SessionHistoryEntry::LegacyV1(_) => None,
        })
        .collect();
    assert_eq!(executed.len(), turns);
    for (index, turn) in executed.iter().enumerate() {
        let expected = turn_request(&fixture, index).turn_id;
        assert_eq!(turn.turn_id, expected);
        assert_eq!(turn.state, LogicalTurnState::Completed);
        let activation = &turn.activations[0];
        let results: Vec<u64> = activation
            .stream
            .iter()
            .filter_map(|event| match &event.content.payload {
                ActivationStreamPayload::ToolResult { .. } => Some(event.content.sequence),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), calls, "turn {index}");
        assert!(results.windows(2).all(|pair| pair[0] < pair[1]));
        let ContentResolution::Available {
            content: output, ..
        } = &activation.output
        else {
            panic!("turn {index} output: {:?}", activation.output)
        };
        assert_eq!(output.text, format!("{} done", expected.as_str()));
    }

    // The export holds the same: every tool call of every turn.
    let export = history.export_json(HistoryVisibility::Visible).unwrap();
    let export = serde_json::from_str::<serde_json::Value>(&export)
        .unwrap()
        .to_string();
    for index in 0..turns {
        let turn = turn_request(&fixture, index).turn_id;
        for call in [0, calls / 2, calls - 1] {
            let id = format!("{}-call-{call}", turn.as_str());
            assert!(export.contains(&id), "{id} missing from the export");
        }
    }
    assert_eq!(
        export.matches(r#""kind":"tool_result""#).count(),
        total,
        "tool results in the export"
    );
    assert_eq!(effects.load(Ordering::SeqCst), total);

    let team = fixture.registry.session_team_token(&session).unwrap();
    fixture
        .registry
        .with_session_team_stores(&team, |_, content, _| {
            Ok((content.record_count(), content.sealed_segments().0))
        })
        .unwrap()
}

/// Past the 128 rounds an activation had and, across two turns, the 256
/// invocations a Session's audit held.
#[tokio::test]
async fn a_session_keeps_every_tool_call_past_the_old_round_and_audit_bounds() {
    let (records, _) = run_long_session(2, 140).await;
    assert!(records > 2 * 140, "{records} content records");
}

/// 1,500 tool calls across three turns: past every old lifetime bound,
/// including the 4096 records the content journal held. A debug build takes
/// minutes, mostly in synced writes, so it runs on request.
#[tokio::test]
#[ignore = "takes minutes in a debug build; run with --ignored"]
async fn a_session_keeps_every_record_of_fifteen_hundred_tool_calls_across_turns() {
    let (records, sealed) = run_long_session(3, 500).await;
    assert!(records > 4096, "{records} content records");
    assert!(sealed > 0);
}
