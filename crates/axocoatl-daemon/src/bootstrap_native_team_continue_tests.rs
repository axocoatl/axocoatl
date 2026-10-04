//! A Team whose lead depends on two scouts, through the actual owned native
//! Begin, driver and Continue: when the scouts' first generations fail and a
//! person continues the turn, the lead runs once the restarted scouts finish.
//! The finite local test providers answer with scripted text and report
//! deterministic synthetic usage; this fixture is not a claim about an
//! external model.
use super::*;
use axocoatl_core::{ChatMessage, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    ProviderExecutionBounds, StreamEvent,
};
use std::pin::Pin;
use tokio_stream::Stream;

const LEAD: &str = "node-2";

/// Scouts node-0 and node-1, and the lead node-2 that depends on both.
async fn scouts_and_lead() -> NativeFixture {
    native_team_fixture(
        8,
        "Exact test host approval",
        &[&[], &[], &[]],
        &[(0, 2), (1, 2)],
    )
    .await
}

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

/// The nodes whose first generation the provider refuses, and every request
/// each activation sent, in order.
struct Scenario {
    fail_first: Vec<&'static str>,
    requests: std::sync::Mutex<Vec<(ActivationRef, String)>>,
}
impl Scenario {
    fn new(fail_first: &[&'static str]) -> Arc<Self> {
        Arc::new(Self {
            fail_first: fail_first.to_vec(),
            requests: std::sync::Mutex::new(vec![]),
        })
    }
    /// The generations of `node` that sent a request, in order.
    fn generations(&self, node: &str) -> Vec<u32> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(activation, _)| activation.node_id.as_str() == node)
            .map(|(activation, _)| activation.generation)
            .collect()
    }
    /// The text of `node`'s last request.
    fn last_request(&self, node: &str) -> String {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(activation, _)| activation.node_id.as_str() == node)
            .map(|(_, text)| text.clone())
            .unwrap()
    }
}

type EventStream =
    Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>;

/// Each activation answers once with its node and generation, unless its
/// node's first generation is scripted to fail as a hosted provider's
/// HTTP 400 did.
struct ScriptedProvider {
    scenario: Arc<Scenario>,
    activation: ActivationRef,
}
#[async_trait::async_trait]
impl LlmProvider for ScriptedProvider {
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
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<EventStream, ProviderError> {
        let text = request
            .messages
            .iter()
            .filter_map(ChatMessage::text_content)
            .collect::<Vec<_>>()
            .join("\n");
        self.scenario
            .requests
            .lock()
            .unwrap()
            .push((self.activation.clone(), text));
        let node = self.activation.node_id.as_str();
        let generation = self.activation.generation;
        if generation == 1 && self.scenario.fail_first.contains(&node) {
            return Err(ProviderError::ApiError {
                provider: "ollama".into(),
                status: 400,
                message: "invalid request error".into(),
            });
        }
        let events = vec![
            StreamEvent::TextDelta {
                delta: format!("{node} answer, generation {generation}"),
            },
            StreamEvent::Usage(TokenUsageStats::new(5, 5)),
            StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ];
        Ok(Box::pin(tokio_stream::iter(events.into_iter().map(Ok))))
    }
}

struct ScriptedFactory {
    controller: crate::session_dispatch::SessionDispatchController,
    scenario: Arc<Scenario>,
}
#[async_trait::async_trait]
impl AutonomousActivationFactory for ScriptedFactory {
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
            provider: Arc::new(ScriptedProvider {
                scenario: self.scenario.clone(),
                activation: input.activation.clone(),
            }),
            config,
            profile,
            counter: Arc::new(Counter),
            tools: Arc::new(axocoatl_tools::ToolExecutor::new()),
        })
    }
}

struct Run {
    controller: crate::session_dispatch::SessionDispatchController,
    factory: Arc<ScriptedFactory>,
    repository: EvidenceRef,
    bus: crate::stream::StreamBus,
    outcome: crate::session_dispatch::TurnDriveOutcome,
}

/// Begin the turn and drive it to its first outcome.
async fn run_turn(fixture: &NativeFixture, scenario: Arc<Scenario>) -> Run {
    let (controller, repository) = begin(fixture, &fixture.request);
    let bus = crate::stream::StreamBus::new(64);
    let factory = Arc::new(ScriptedFactory {
        controller: controller.clone(),
        scenario,
    });
    let NativeFirstTurnStart::Prepared(prepared) = finish_owned_setup(
        &fixture.registry,
        controller.clone(),
        repository.clone(),
        &fixture.request.source().unwrap(),
        bus.clone(),
        factory.clone(),
    )
    .unwrap() else {
        panic!("owned native driver")
    };
    let outcome = tokio::time::timeout(Duration::from_secs(10), prepared.run())
        .await
        .expect("the turn settles")
        .unwrap();
    Run {
        controller,
        factory,
        repository,
        bus,
        outcome,
    }
}

/// The latest activation of `node`.
fn latest(contract: &TurnContract, node: &str) -> ActivationRef {
    contract
        .activations()
        .iter()
        .rev()
        .find(|item| item.activation.node_id.as_str() == node)
        .unwrap()
        .activation
        .clone()
}

/// Continue the paused turn of `run` as a person would, restarting the
/// latest activations of `nodes`, and drive it to its next outcome.
async fn continue_turn(fixture: &NativeFixture, run: &mut Run, id: &str, nodes: &[&str]) {
    let snapshot = run.controller.snapshot().unwrap();
    let contract = snapshot.contract();
    let request = crate::session_dispatch::HumanControlActionRequest {
        schema_version: 1,
        command_id: CommandId::new(id).unwrap(),
        session_id: snapshot.owner().session_id.clone(),
        turn_id: snapshot.turn_id().clone(),
        execution_epoch_id: contract.epochs().last().unwrap().id.clone(),
        expected_turn_revision: contract.revision(),
        expected_graph_revision: contract.graph().unwrap().revision,
        activation: None,
        action: crate::session_dispatch::HumanControlAction::Continue,
        instruction: None,
        include_previous_output: false,
        context: None,
        continuation: Some(crate::session_dispatch::HumanContinuationSelection {
            restart: nodes.iter().map(|node| latest(contract, node)).collect(),
            checks: vec![],
        }),
        blocker_id: None,
        human_response: None,
        partial_finish: None,
    };
    let receipt = fixture
        .registry
        .submit_human_action(
            fixture.request.session_id.as_str(),
            fixture.request.turn_id.as_str(),
            request.clone(),
            3,
        )
        .unwrap();
    assert_eq!(
        receipt.state,
        axocoatl_session::control_command::ControlCommandState::Settled,
        "{receipt:?}"
    );
    let driver = run
        .controller
        .prepare_native_control_driver(
            &request.command_id,
            run.repository.clone(),
            run.bus.clone(),
            run.factory.clone(),
        )
        .unwrap()
        .unwrap();
    run.outcome = tokio::time::timeout(Duration::from_secs(10), driver.run())
        .await
        .expect("the continued turn settles")
        .unwrap();
}

/// What the last epoch's continuation selected for `node`.
fn selection<'a>(contract: &'a TurnContract, node: &str) -> &'a ContinuationSelection {
    contract
        .epochs()
        .last()
        .unwrap()
        .continuation
        .as_ref()
        .unwrap()
        .selections
        .iter()
        .find(|selection| {
            let selected = match selection {
                ContinuationSelection::RetainAccepted { activation }
                | ContinuationSelection::LeaveBlocked { activation, .. } => &activation.node_id,
                ContinuationSelection::Retry { previous, .. }
                | ContinuationSelection::Rebase { previous, .. }
                | ContinuationSelection::Revise { previous, .. } => &previous.node_id,
                ContinuationSelection::PrepareUnmaterialized { input } => &input.activation.node_id,
                ContinuationSelection::AwaitDependencies { node_id }
                | ContinuationSelection::LeaveUnmaterializedBlocked { node_id, .. } => node_id,
            };
            selected.as_str() == node
        })
        .unwrap()
}

/// Each failed activation's class and recommended step, by node.
fn failures(run: &Run) -> Vec<(String, &'static str, &'static str)> {
    let snapshot = run.controller.snapshot().unwrap();
    let view = run
        .controller
        .with_team_stores(|_, content, _| Ok(content.project(&snapshot).unwrap()))
        .unwrap();
    view.activations
        .iter()
        .filter_map(|activation| {
            activation.failure.as_ref().map(|failure| {
                (
                    activation.activation.activation.node_id.as_str().to_owned(),
                    failure.class,
                    failure.next_step,
                )
            })
        })
        .collect()
}

/// Without a failure, the lead runs once after both scouts, with both of
/// their answers, and the turn completes.
#[tokio::test]
async fn a_lead_runs_after_both_scouts_and_completes_the_turn() {
    let fixture = scouts_and_lead().await;
    let scenario = Scenario::new(&[]);
    let run = run_turn(&fixture, scenario.clone()).await;
    let contract = run.outcome.snapshot.contract();
    assert_eq!(
        contract.state(),
        Some(LogicalTurnState::Completed),
        "{contract:?}"
    );
    assert_eq!(contract.epochs().len(), 1);
    assert_eq!(scenario.generations("node-0"), vec![1]);
    assert_eq!(scenario.generations("node-1"), vec![1]);
    assert_eq!(scenario.generations(LEAD), vec![1]);
    let requests = scenario.requests.lock().unwrap().clone();
    assert_eq!(requests.last().unwrap().0.node_id.as_str(), LEAD);
    let lead = scenario.last_request(LEAD);
    assert!(lead.contains("node-0 answer, generation 1"), "{lead}");
    assert!(lead.contains("node-1 answer, generation 1"), "{lead}");
    assert!(failures(&run).is_empty());
}

/// Both scouts fail on a provider error, so the lead never starts and the
/// turn needs attention with Continue recommended. Continuing restarts the
/// scouts; the lead, which never ran, waits for them in that epoch instead
/// of staying blocked, runs with their new answers, and the turn completes.
#[tokio::test]
async fn continuing_failed_scouts_runs_the_lead_that_depends_on_them() {
    let fixture = scouts_and_lead().await;
    let scenario = Scenario::new(&["node-0", "node-1"]);
    let mut run = run_turn(&fixture, scenario.clone()).await;
    let contract = run.outcome.snapshot.contract();
    assert_eq!(contract.state(), Some(LogicalTurnState::NeedsAttention));
    assert!(scenario.generations(LEAD).is_empty());
    let mut failed = failures(&run);
    failed.sort();
    assert_eq!(
        failed,
        [
            ("node-0".to_owned(), "provider_error", "continue"),
            ("node-1".to_owned(), "provider_error", "continue"),
        ]
    );
    let controls = run
        .controller
        .control_plane()
        .unwrap()
        .turn_controls
        .unwrap();
    assert!(
        controls.continue_turn.enabled,
        "{:?}",
        controls.continue_turn
    );
    continue_turn(&fixture, &mut run, "restart-scouts", &["node-0", "node-1"]).await;
    let contract = run.outcome.snapshot.contract();
    assert_eq!(
        contract.state(),
        Some(LogicalTurnState::Completed),
        "{contract:?}"
    );
    assert!(matches!(
        selection(contract, LEAD),
        ContinuationSelection::AwaitDependencies { .. }
    ));
    assert_eq!(scenario.generations("node-0"), vec![1, 2]);
    assert_eq!(scenario.generations("node-1"), vec![1, 2]);
    assert_eq!(scenario.generations(LEAD), vec![1]);
    let lead = scenario.last_request(LEAD);
    assert!(lead.contains("node-0 answer, generation 2"), "{lead}");
    assert!(lead.contains("node-1 answer, generation 2"), "{lead}");
    let lead = contract
        .activations()
        .iter()
        .find(|item| item.activation.node_id.as_str() == LEAD)
        .unwrap();
    assert_eq!(lead.state, ActivationState::Accepted);
    assert_eq!(
        lead.activation.execution_epoch_id,
        contract.epochs().last().unwrap().id
    );
    let parents = lead
        .input
        .parents
        .iter()
        .map(|parent| {
            (
                parent.activation.node_id.as_str(),
                parent.activation.generation,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(parents, [("node-0", 2), ("node-1", 2)]);
}

/// Restarting only one failed scout leaves the lead blocked behind the
/// other: the turn needs attention again with the other scout offered.
/// Continuing that one then runs the lead, and the turn completes.
#[tokio::test]
async fn a_lead_waits_until_every_failed_scout_is_restarted() {
    let fixture = scouts_and_lead().await;
    let scenario = Scenario::new(&["node-0", "node-1"]);
    let mut run = run_turn(&fixture, scenario.clone()).await;
    continue_turn(&fixture, &mut run, "restart-first-scout", &["node-0"]).await;
    let contract = run.outcome.snapshot.contract();
    assert_eq!(contract.state(), Some(LogicalTurnState::NeedsAttention));
    assert!(matches!(
        selection(contract, LEAD),
        ContinuationSelection::LeaveUnmaterializedBlocked { .. }
    ));
    assert!(matches!(
        selection(contract, "node-1"),
        ContinuationSelection::LeaveBlocked { .. }
    ));
    assert!(scenario.generations(LEAD).is_empty());
    let controls = run
        .controller
        .control_plane()
        .unwrap()
        .turn_controls
        .unwrap();
    assert!(
        controls.continue_turn.enabled,
        "{:?}",
        controls.continue_turn
    );
    let offered = controls
        .continuation_choices
        .iter()
        .map(|choice| choice.activation.node_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(offered, ["node-1"]);
    continue_turn(&fixture, &mut run, "restart-second-scout", &["node-1"]).await;
    let contract = run.outcome.snapshot.contract();
    assert_eq!(
        contract.state(),
        Some(LogicalTurnState::Completed),
        "{contract:?}"
    );
    assert!(matches!(
        selection(contract, "node-0"),
        ContinuationSelection::RetainAccepted { .. }
    ));
    assert!(matches!(
        selection(contract, LEAD),
        ContinuationSelection::AwaitDependencies { .. }
    ));
    assert_eq!(scenario.generations("node-0"), vec![1, 2]);
    assert_eq!(scenario.generations("node-1"), vec![1, 2]);
    assert_eq!(scenario.generations(LEAD), vec![1]);
    let lead = scenario.last_request(LEAD);
    assert!(lead.contains("node-0 answer, generation 2"), "{lead}");
    assert!(lead.contains("node-1 answer, generation 2"), "{lead}");
}
