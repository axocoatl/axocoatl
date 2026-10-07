//! Slots of one turn that depend on nothing run at the same time, through
//! the actual owned native Begin and driver: the shape of an audit's area
//! turn, one worker slot per area with no dependencies. A slot that depends
//! on another starts only after it, which shows the measurement can tell the
//! two apart. The finite local test provider answers with scripted text and
//! deterministic synthetic usage; it records when each activation's model
//! call starts and ends and holds each call until the expected number are
//! in flight together (or a bound passes), so the overlap is observed, not
//! inferred. This fixture is not a claim about an external model.
use super::*;
use axocoatl_core::{ChatMessage, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    ProviderExecutionBounds, StreamEvent,
};
use std::pin::Pin;
use std::time::Instant;
use tokio_stream::Stream;

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

/// Model calls in flight, the most at once, and each call's interval.
struct Overlap {
    /// How many calls a call waits to see in flight together.
    expect: usize,
    /// The longest a call waits for them.
    hold: Duration,
    /// (in flight now, most in flight at once)
    counts: std::sync::Mutex<(usize, usize)>,
    arrived: tokio::sync::Notify,
    intervals: std::sync::Mutex<Vec<(String, Instant, Instant)>>,
}

impl Overlap {
    fn new(expect: usize, hold: Duration) -> Arc<Self> {
        Arc::new(Self {
            expect,
            hold,
            counts: std::sync::Mutex::new((0, 0)),
            arrived: tokio::sync::Notify::new(),
            intervals: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn most_at_once(&self) -> usize {
        self.counts.lock().unwrap().1
    }

    /// The interval of `node`'s one model call.
    fn interval(&self, node: &str) -> (Instant, Instant) {
        let intervals = self.intervals.lock().unwrap();
        let matching: Vec<_> = intervals.iter().filter(|(id, _, _)| id == node).collect();
        assert_eq!(matching.len(), 1, "{node} made one model call");
        (matching[0].1, matching[0].2)
    }

    async fn call(&self, node: &str) {
        let start = Instant::now();
        {
            let mut counts = self.counts.lock().unwrap();
            counts.0 += 1;
            counts.1 = counts.1.max(counts.0);
        }
        self.arrived.notify_waiters();
        let deadline = tokio::time::Instant::now() + self.hold;
        loop {
            let arrived = self.arrived.notified();
            tokio::pin!(arrived);
            arrived.as_mut().enable();
            if self.counts.lock().unwrap().1 >= self.expect {
                break;
            }
            if tokio::time::timeout_at(deadline, arrived).await.is_err() {
                break;
            }
        }
        self.counts.lock().unwrap().0 -= 1;
        self.intervals
            .lock()
            .unwrap()
            .push((node.to_owned(), start, Instant::now()));
    }
}

type EventStream =
    Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>;

/// Each activation answers once with its node, after its model call has
/// been measured.
struct MeasuredProvider {
    overlap: Arc<Overlap>,
    activation: ActivationRef,
}
#[async_trait::async_trait]
impl LlmProvider for MeasuredProvider {
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
            response_bytes: 64 * 1024,
        })
    }
    async fn chat(&self, _: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        unreachable!("Agents stream through DefaultAgentBehavior")
    }
    async fn chat_stream(&self, _: ChatRequest) -> std::result::Result<EventStream, ProviderError> {
        let node = self.activation.node_id.as_str();
        self.overlap.call(node).await;
        let events = vec![
            StreamEvent::TextDelta {
                delta: format!("{node} answer"),
            },
            StreamEvent::Usage(TokenUsageStats::new(5, 5)),
            StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ];
        Ok(Box::pin(tokio_stream::iter(events.into_iter().map(Ok))))
    }
}

struct MeasuredFactory {
    controller: crate::session_dispatch::SessionDispatchController,
    overlap: Arc<Overlap>,
}
#[async_trait::async_trait]
impl AutonomousActivationFactory for MeasuredFactory {
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
                } = &content
                    .resolve_activation_evidence(&input.definition.snapshot)
                    .unwrap()
                else {
                    panic!("exact definition")
                };
                Ok((
                    serde_json::from_str::<AgentConfig>(configuration).unwrap(),
                    profile.clone(),
                ))
            })
            .map_err(|error| error.to_string())?;
        config.id = AgentId::new(input.conversation_id.as_str());
        Ok(AutonomousActivationResources {
            provider: Arc::new(MeasuredProvider {
                overlap: self.overlap.clone(),
                activation: input.activation.clone(),
            }),
            config,
            profile,
            counter: Arc::new(Counter),
            tools: Arc::new(axocoatl_tools::ToolExecutor::new()),
        })
    }
}

/// Begin the fixture's turn and drive it to its outcome.
async fn drive(
    fixture: &NativeFixture,
    overlap: Arc<Overlap>,
) -> crate::session_dispatch::TurnDriveOutcome {
    let (controller, repository) = begin(fixture, &fixture.request);
    let factory = Arc::new(MeasuredFactory {
        controller: controller.clone(),
        overlap,
    });
    let NativeFirstTurnStart::Prepared(prepared) = finish_owned_setup(
        &fixture.registry,
        controller,
        repository,
        &fixture.request.source().unwrap(),
        crate::stream::StreamBus::new(64),
        factory,
    )
    .unwrap() else {
        panic!("owned native driver")
    };
    tokio::time::timeout(Duration::from_secs(30), prepared.run())
        .await
        .expect("the turn settles")
        .unwrap()
}

/// Three slots without dependencies, as an audit's area workers: every
/// model call is in flight before any of them ends, and the turn completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slots_without_dependencies_run_at_the_same_time() {
    let fixture = native_team_fixture(8, "Exact test host approval", &[&[], &[], &[]], &[]).await;
    let overlap = Overlap::new(3, Duration::from_secs(10));
    let outcome = drive(&fixture, overlap.clone()).await;
    let contract = outcome.snapshot.contract();
    assert_eq!(
        contract.state(),
        Some(LogicalTurnState::Completed),
        "{contract:?}"
    );
    assert_eq!(
        overlap.most_at_once(),
        3,
        "every slot's call was in flight at once"
    );
    let intervals: Vec<_> = ["node-0", "node-1", "node-2"]
        .iter()
        .map(|node| overlap.interval(node))
        .collect();
    let last_start = intervals.iter().map(|(start, _)| *start).max().unwrap();
    let first_end = intervals.iter().map(|(_, end)| *end).min().unwrap();
    assert!(
        last_start < first_end,
        "the three activation intervals overlap: {intervals:?}"
    );
}

/// The control: a slot that depends on another starts its model call only
/// after its parent's ended, and the two are never in flight together.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dependent_slot_starts_after_its_parent() {
    let fixture = native_team_fixture(8, "Exact test host approval", &[&[], &[]], &[(0, 1)]).await;
    let overlap = Overlap::new(2, Duration::from_millis(300));
    let outcome = drive(&fixture, overlap.clone()).await;
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    assert_eq!(overlap.most_at_once(), 1);
    let (_, parent_end) = overlap.interval("node-0");
    let (child_start, _) = overlap.interval("node-1");
    assert!(child_start >= parent_end);
}
