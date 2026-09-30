//! Actual owned native Begin -> lead Agent -> `delegate` -> same-driver helper.
//! The finite local test providers report only deterministic synthetic usage;
//! this fixture is not a claim about an external model or repository execution.
use super::*;
use crate::bootstrap::session_team::{ApprovedCoordinatorPolicy, ApprovedCoordinatorResource};
use crate::session_dispatch::NativeCoordinatorWorker;
use axocoatl_core::{AgentRole, ChatMessage, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    ProviderExecutionBounds, StreamEvent,
};
use axocoatl_session::control_command::{
    CommandReceiptView, CommandSourceRecord, ControlCommandState, ControlParameters,
};
use sha2::Digest;
use std::pin::Pin;
use tokio_stream::Stream;

const LEAD_MARKER: &str = "lead-private-scratch-note";
const TASK: &str = "List every public function in src/lib.rs and report only their names.";
const LEAD_GRANT: &str = "lead-grant";

fn helper_limits() -> GrantLimits {
    GrantLimits {
        activations: 2,
        invocations: 4,
        tokens: 10000,
        cost_microunits: 0,
    }
}

async fn lead_fixture(aggregate_tokens: u64) -> NativeFixture {
    lead_fixture_with_helpers(aggregate_tokens, &[("scout", &[])]).await
}

/// One Autonomous lead slot whose reviewed approval lets it add the given
/// Worker helper templates. Delegation is minted from it at turn start.
async fn lead_fixture_with_helpers(
    aggregate_tokens: u64,
    helpers: &[(&str, &[&str])],
) -> NativeFixture {
    let mut fixture = native_fixture().await;
    let session_id = fixture.request.session_id.clone();
    let token = fixture
        .registry
        .session_team_token(session_id.as_str())
        .unwrap();
    let metadata = fixture.repository.owner.metadata().clone();
    let working_dir = fixture.repository._workspace.path().to_path_buf();
    let (grant, node) = fixture
        .registry
        .with_session_team_stores(&token, |canonical, content, _| {
            let slot_id = SessionTeamSlotId::new("lead-slot").unwrap();
            let identity = format!(
                "{:x}",
                sha2::Sha256::digest(
                    serde_json::to_vec(&(session_id.as_str(), slot_id.as_str())).unwrap()
                )
            );
            let node_id = TurnNodeId::new(format!("team-node-{}", &identity[..24])).unwrap();
            let conversation_id = NodeConversationId::new("lead-conversation").unwrap();
            let mut agents = vec![("lead", AgentRole::Autonomous, vec![])];
            for (name, tools) in helpers {
                agents.push((
                    name,
                    AgentRole::Worker,
                    tools.iter().map(|tool| tool.to_string()).collect(),
                ));
            }
            let mut retained = vec![];
            for (name, role, tools) in agents {
                let definition_id = AgentDefinitionId::new(format!("{name}-definition")).unwrap();
                let config = AgentConfig {
                    id: AgentId::new(if role == AgentRole::Worker {
                        format!("{name}-template")
                    } else {
                        conversation_id.as_str().to_owned()
                    }),
                    role,
                    provider: "ollama".into(),
                    model: "test-model".into(),
                    tools: tools.clone(),
                    sampling: SamplingConfig {
                        max_tokens: Some(128),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let profile = ExecutionProfile {
                    definition: definition_id.as_str().into(),
                    provider: "ollama".into(),
                    model: "test-model".into(),
                    isolation: "in-process".into(),
                    tools,
                };
                let snapshot = content
                    .retain_activation_evidence(ActivationEvidenceContent::Definition {
                        definition_id: definition_id.clone(),
                        revision: 1,
                        profile: profile.clone(),
                        configuration: serde_json::to_string(&config).unwrap(),
                    })
                    .unwrap()
                    .reference()
                    .clone();
                content
                    .retain_provider_profile(
                        canonical,
                        &snapshot,
                        "ollama",
                        serde_json::json!({"fixture":"finite local provider; no external model"})
                            .to_string(),
                    )
                    .unwrap();
                retained.push((
                    name.to_string(),
                    DefinitionSnapshotRef {
                        definition_id,
                        snapshot,
                    },
                    profile,
                ));
            }
            let lead_limits = GrantLimits {
                activations: 12,
                invocations: 20,
                tokens: aggregate_tokens,
                cost_microunits: 0,
            };
            let approved = ApprovedCoordinatorPolicy {
                workers: retained[1..]
                    .iter()
                    .map(|(name, definition, _)| NativeCoordinatorWorker {
                        template_id: name.clone(),
                        definition: definition.clone(),
                        limits: helper_limits(),
                        adhoc_allowed: false,
                    })
                    .collect(),
                operations: vec![DelegatedOperation::AddAgent],
                max_nodes: 8,
                max_edges: 0,
                htn_methods_yaml: None,
                resource: ApprovedCoordinatorResource {
                    session_id: session_id.as_str().into(),
                    workspace_id: metadata.workspace_id.clone(),
                    working_dir: working_dir.clone(),
                    environment_generation: metadata.environment_generation,
                    backend: metadata.backend.clone(),
                    network: "none".into(),
                    require_resource_limits: false,
                    image: None,
                    setup_command: None,
                    setup_approved: false,
                    setup_reviewed: true,
                },
            };
            let issuer = content
                .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                    text: serde_json::json!({
                        "kind": "authenticated_session_team_apply",
                        "edit": {"command_id": "approve-lead", "expected_configuration_revision": 1,
                            "slots": [], "dependencies": [], "layout": []},
                        "templates": [[slot_id.as_str(), "lead"]],
                        "coordinators": [[slot_id.as_str(), approved]]
                    })
                    .to_string(),
                })
                .unwrap()
                .reference()
                .clone();
            let grant = AuthorityGrant {
                id: LEAD_GRANT.into(),
                revision: 1,
                issuer_evidence: issuer,
                holder: node_id.clone(),
                descendants: vec![],
                allow_stop_descendants: false,
                delegation: None,
                profiles: retained
                    .iter()
                    .map(|(_, _, profile)| profile.clone())
                    .collect(),
                conditions: vec![],
                limits: lead_limits.clone(),
                expires_at_ms: u64::MAX,
            };
            let grant_ref = content
                .retain_activation_evidence(ActivationEvidenceContent::Grant {
                    policy: grant.clone(),
                })
                .unwrap()
                .reference()
                .clone();
            let budget = content
                .retain_activation_evidence(ActivationEvidenceContent::Budget {
                    limits: lead_limits,
                })
                .unwrap()
                .reference()
                .clone();
            let mut team = SessionTeamStore::open_owned(
                canonical
                    .component_namespace(ExecutionComponent::SessionTeam)
                    .unwrap(),
                canonical,
                content,
                None,
            )
            .unwrap();
            team.commit(
                SessionTeamCommit {
                    schema_version: 1,
                    command_id: CommandId::new("approve-lead").unwrap(),
                    expected_configuration_revision: 1,
                    graph: SessionTeamGraph {
                        slots: vec![SessionTeamSlot {
                            slot_id: slot_id.clone(),
                            node_id: node_id.clone(),
                            definition: retained[0].1.clone(),
                            conversation_id,
                            required: true,
                            budget,
                            grant: Some(grant_ref),
                        }],
                        dependencies: vec![],
                        conditions: vec![],
                    },
                    initial_source: None,
                    continuity: vec![SlotContinuityDecision {
                        slot_id,
                        decision: SessionTeamContinuity::Reset,
                    }],
                    layout: vec![],
                },
                canonical,
                content,
                None,
            )
            .unwrap();
            Ok((grant, node_id))
        })
        .unwrap();
    fixture.request.expected_team_revision = 2;
    fixture.request.grants = vec![grant];
    fixture.request.node_evidence = vec![NativeNodeEvidence {
        node_id: node,
        guidance: vec![],
        attachments: vec![],
    }];
    fixture
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

/// What the lead asks for and how the helper behaves; every provider request
/// is recorded for the assertions.
struct Scenario {
    helper: String,
    task: String,
    answer: String,
    helper_fails: bool,
    lead_fails_first_generation: bool,
    hold_helper: Option<Arc<tokio::sync::Semaphore>>,
    lead_requests: std::sync::Mutex<Vec<(ActivationRef, ChatRequest)>>,
    helper_requests: std::sync::Mutex<Vec<ChatRequest>>,
    helper_calls: AtomicUsize,
}
impl Scenario {
    fn new(answer: impl Into<String>) -> Self {
        Self {
            helper: "scout".into(),
            task: TASK.into(),
            answer: answer.into(),
            helper_fails: false,
            lead_fails_first_generation: false,
            hold_helper: None,
            lead_requests: std::sync::Mutex::new(vec![]),
            helper_requests: std::sync::Mutex::new(vec![]),
            helper_calls: AtomicUsize::new(0),
        }
    }
    fn helper_calls(&self) -> usize {
        self.helper_calls.load(Ordering::SeqCst)
    }
    /// The lead's view of the delegate result in its last provider request.
    fn last_delegate_result(&self) -> String {
        let requests = self.lead_requests.lock().unwrap();
        let (_, request) = requests.last().unwrap();
        request
            .messages
            .iter()
            .rev()
            .find(|message| message.tool_call_id.as_deref() == Some("delegate-call"))
            .expect("the lead's next request carries the delegate result")
            .text_content()
            .unwrap()
            .to_owned()
    }
}

fn bounds() -> ProviderExecutionBounds {
    ProviderExecutionBounds {
        token_limit: 100,
        cost_microunits: 0,
        response_bytes: 64 * 1024,
    }
}
fn capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        streaming: true,
        tool_calling: true,
        ..Default::default()
    }
}
type EventStream =
    Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>;
fn finished(mut events: Vec<StreamEvent>, finish_reason: FinishReason) -> EventStream {
    events.push(StreamEvent::Usage(TokenUsageStats::new(5, 5)));
    events.push(StreamEvent::Done { finish_reason });
    Box::pin(tokio_stream::iter(events.into_iter().map(Ok)))
}
fn provider_failure(reason: &str) -> EventStream {
    Box::pin(tokio_stream::iter(vec![Err(ProviderError::Stream(
        reason.into(),
    ))]))
}

/// Round 0 delegates; later rounds answer from the delegate result.
struct LeadProvider {
    scenario: Arc<Scenario>,
    activation: ActivationRef,
    round: AtomicUsize,
}
#[async_trait::async_trait]
impl LlmProvider for LeadProvider {
    fn provider_id(&self) -> &str {
        "ollama"
    }
    fn model_id(&self) -> &str {
        "test-model"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        capabilities()
    }
    fn execution_bounds(&self, _: &ChatRequest) -> Option<ProviderExecutionBounds> {
        Some(bounds())
    }
    async fn chat(&self, _: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        unreachable!("the lead streams through DefaultAgentBehavior")
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<EventStream, ProviderError> {
        self.scenario
            .lead_requests
            .lock()
            .unwrap()
            .push((self.activation.clone(), request));
        let round = self.round.fetch_add(1, Ordering::SeqCst);
        if round == 0 {
            return Ok(finished(
                vec![
                    StreamEvent::TextDelta {
                        delta: LEAD_MARKER.into(),
                    },
                    StreamEvent::ToolCallDelta {
                        index: Some(0),
                        id: "delegate-call".into(),
                        name: Some("delegate".into()),
                        args_delta: serde_json::json!({
                            "helper": self.scenario.helper,
                            "task": self.scenario.task,
                        })
                        .to_string(),
                    },
                ],
                FinishReason::ToolUse,
            ));
        }
        if self.scenario.lead_fails_first_generation && self.activation.generation == 1 {
            return Ok(provider_failure("lead provider lost its connection"));
        }
        Ok(finished(
            vec![StreamEvent::TextDelta {
                delta: "Lead summary of the helper answer".into(),
            }],
            FinishReason::Stop,
        ))
    }
}

struct HelperProvider {
    scenario: Arc<Scenario>,
}
#[async_trait::async_trait]
impl LlmProvider for HelperProvider {
    fn provider_id(&self) -> &str {
        "ollama"
    }
    fn model_id(&self) -> &str {
        "test-model"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        capabilities()
    }
    fn execution_bounds(&self, _: &ChatRequest) -> Option<ProviderExecutionBounds> {
        Some(bounds())
    }
    async fn chat(&self, _: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        unreachable!("helpers stream through DefaultAgentBehavior")
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<EventStream, ProviderError> {
        self.scenario.helper_requests.lock().unwrap().push(request);
        self.scenario.helper_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(hold) = &self.scenario.hold_helper {
            hold.acquire().await.unwrap().forget();
        }
        if self.scenario.helper_fails {
            return Ok(provider_failure("helper provider failed"));
        }
        Ok(finished(
            vec![StreamEvent::TextDelta {
                delta: self.scenario.answer.clone(),
            }],
            FinishReason::Stop,
        ))
    }
}

struct DelegateFactory {
    controller: crate::session_dispatch::SessionDispatchController,
    scenario: Arc<Scenario>,
    resolved: std::sync::Mutex<Vec<ActivationInputManifest>>,
}
#[async_trait::async_trait]
impl AutonomousActivationFactory for DelegateFactory {
    async fn resources(
        &self,
        input: &ActivationInputManifest,
    ) -> std::result::Result<AutonomousActivationResources, String> {
        self.resolved.lock().unwrap().push(input.clone());
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
                    serde_json::from_str::<AgentConfig>(configuration).unwrap(),
                    profile.clone(),
                ))
            })
            .map_err(|error| error.to_string())?;
        config.id = AgentId::new(input.conversation_id.as_str());
        let provider: Arc<dyn LlmProvider> = if config.role == AgentRole::Worker {
            Arc::new(HelperProvider {
                scenario: self.scenario.clone(),
            })
        } else {
            Arc::new(LeadProvider {
                scenario: self.scenario.clone(),
                activation: input.activation.clone(),
                round: AtomicUsize::new(0),
            })
        };
        Ok(AutonomousActivationResources {
            provider,
            config,
            profile,
            counter: Arc::new(Counter),
            tools: Arc::new(axocoatl_tools::ToolExecutor::new()),
        })
    }
}

struct Run {
    controller: crate::session_dispatch::SessionDispatchController,
    repository: EvidenceRef,
    bus: crate::stream::StreamBus,
    factory: Arc<DelegateFactory>,
    outcome: std::result::Result<crate::session_dispatch::TurnDriveOutcome, String>,
}

async fn run_lead(fixture: &NativeFixture, scenario: Arc<Scenario>, lose_return: bool) -> Run {
    let (controller, repository) = begin(fixture, &fixture.request);
    if lose_return {
        controller.lose_delegate_outcome_for_test();
    }
    let factory = Arc::new(DelegateFactory {
        controller: controller.clone(),
        scenario,
        resolved: std::sync::Mutex::new(vec![]),
    });
    let bus = crate::stream::StreamBus::new(64);
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
        .unwrap()
        .map_err(|error| error.to_string());
    Run {
        controller,
        repository,
        bus,
        factory,
        outcome,
    }
}

fn agent_commands(
    controller: &crate::session_dispatch::SessionDispatchController,
) -> Vec<CommandReceiptView> {
    let crate::session_control_plane::EvidenceValue::Available { value: commands } =
        controller.control_plane().unwrap().commands
    else {
        panic!("actual command journal")
    };
    commands
        .into_iter()
        .filter(|receipt| matches!(receipt.source, CommandSourceRecord::Agent { .. }))
        .collect()
}

fn helper_node(
    snapshot: &axocoatl_session::execution_store::DurableTurnSnapshot,
    lead: &TurnNodeId,
) -> Option<TurnNodeId> {
    snapshot
        .contract()
        .graph()
        .unwrap()
        .nodes
        .iter()
        .find(|node| node.node_id != *lead)
        .map(|node| node.node_id.clone())
}

#[tokio::test]
async fn lead_delegates_to_read_only_helper_and_receives_bounded_result() {
    let fixture = lead_fixture(100000).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    // The cut at 8192 bytes falls inside the two-byte character.
    let answer = format!("{}é{}", "a".repeat(8191), "b".repeat(800));
    let scenario = Arc::new(Scenario::new(answer.clone()));
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed),
        "{:?}",
        outcome.snapshot.contract()
    );
    assert_eq!(scenario.helper_calls(), 1);
    let node = helper_node(&outcome.snapshot, &lead).expect("one helper node");
    let accepted = outcome.snapshot.contract().current_accepted_activations();
    assert_eq!(accepted.len(), 2);
    let graph = outcome.snapshot.contract().graph().unwrap();
    assert!(
        !graph
            .nodes
            .iter()
            .find(|item| item.node_id == node)
            .unwrap()
            .required,
        "a delegated helper is optional work"
    );
    assert!(
        graph
            .nodes
            .iter()
            .find(|item| item.node_id == lead)
            .unwrap()
            .required
    );

    let first_request = scenario.lead_requests.lock().unwrap()[0].1.clone();
    let tool = first_request
        .tools
        .iter()
        .find(|tool| tool.name == "delegate")
        .expect("the lead is offered delegate");
    assert!(tool.description.contains("- scout: no tools"));
    assert_eq!(
        tool.parameters["properties"]["helper"]["enum"],
        serde_json::json!(["scout"])
    );

    let result: serde_json::Value = serde_json::from_str(&scenario.last_delegate_result()).unwrap();
    assert_eq!(result["helper"], "scout");
    assert_eq!(result["node_id"], node.as_str());
    assert_eq!(result["status"], "completed");
    assert_eq!(result["truncated"], true);
    assert_eq!(result["output_bytes"], answer.len());
    assert_eq!(result["result"], "a".repeat(8191));
    assert!(result["note"].as_str().unwrap().contains("narrower task"));
    assert_eq!(
        run.controller.delegate_helper_answer_for_test(&node),
        Some(answer),
        "the helper's accepted output keeps the full answer"
    );

    let commands = agent_commands(&run.controller);
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].state, ControlCommandState::Settled);
    let ControlParameters::AddAgent { input, .. } = &commands[0].request.parameters else {
        panic!("delegate admits the helper with AddAgent")
    };
    assert_eq!(input.activation.node_id, node);
    assert!(fixture.registry.live_native_turns().unwrap().is_empty());
}

#[tokio::test]
async fn delegated_helper_starts_in_a_fresh_conversation() {
    let fixture = lead_fixture(100000).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let scenario = Arc::new(Scenario::new("pub fn run, pub fn stop"));
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    let node = helper_node(&outcome.snapshot, &lead).unwrap();
    let resolved = run.factory.resolved.lock().unwrap().clone();
    let lead_input = resolved
        .iter()
        .find(|input| input.activation.node_id == lead)
        .unwrap();
    let helper_input = resolved
        .iter()
        .find(|input| input.activation.node_id == node)
        .unwrap();
    assert_eq!(
        helper_input.starting_savepoint,
        ConversationSavepoint::Empty
    );
    assert!(helper_input.parents.is_empty());
    assert_ne!(helper_input.conversation_id, lead_input.conversation_id);

    let helper_requests = scenario.helper_requests.lock().unwrap();
    assert_eq!(helper_requests.len(), 1);
    let text = serde_json::to_string(&helper_requests[0].messages).unwrap();
    assert!(text.contains(TASK), "the helper receives the task");
    assert!(
        !text.contains(LEAD_MARKER),
        "the helper cannot see the lead's conversation"
    );
    assert!(
        helper_requests[0]
            .tools
            .iter()
            .all(|tool| tool.name != "delegate"),
        "a helper cannot delegate further"
    );
    let lead_requests = scenario.lead_requests.lock().unwrap();
    assert!(
        serde_json::to_string(&lead_requests.last().unwrap().1.messages)
            .unwrap()
            .contains(LEAD_MARKER)
    );
}

#[tokio::test]
async fn failed_helper_returns_a_tool_error_and_the_lead_completes() {
    let fixture = lead_fixture(100000).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let mut scenario = Scenario::new("never produced");
    scenario.helper_fails = true;
    let scenario = Arc::new(scenario);
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed),
        "{:?}",
        outcome.snapshot.contract()
    );
    let node = helper_node(&outcome.snapshot, &lead).unwrap();
    let helper = outcome
        .snapshot
        .contract()
        .activations()
        .iter()
        .rev()
        .find(|item| item.activation.node_id == node)
        .unwrap();
    assert_eq!(helper.state, ActivationState::Failed);
    let error = scenario.last_delegate_result();
    assert!(
        error.contains("The helper 'scout' did not finish")
            && error.contains(node.as_str())
            && error.contains("Continue without its answer"),
        "{error}"
    );
    let accepted = outcome.snapshot.contract().current_accepted_activations();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].activation.node_id, lead);
}

#[tokio::test]
async fn delegated_helper_budget_is_reserved_from_the_lead_grant() {
    let fixture = lead_fixture(100000).await;
    let scenario = Arc::new(Scenario::new("pub fn run"));
    let run = run_lead(&fixture, scenario.clone(), false).await;
    assert_eq!(
        run.outcome.unwrap().snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    let usage = run.controller.grant_usage_for_test(LEAD_GRANT);
    let limits = helper_limits();
    // The lead's own activation plus the helper's full reserved limits.
    assert_eq!(usage.activations, 1 + limits.activations);
    assert!(usage.invocations > limits.invocations);
    assert!(usage.tokens >= limits.tokens);
}

#[tokio::test]
async fn delegated_helper_that_does_not_fit_the_lead_budget_is_refused() {
    let fixture = lead_fixture(helper_limits().tokens - 1000).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let scenario = Arc::new(Scenario::new("never produced"));
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed),
        "the lead continues after a refused helper"
    );
    assert_eq!(scenario.helper_calls(), 0);
    assert!(helper_node(&outcome.snapshot, &lead).is_none());
    let error = scenario.last_delegate_result();
    assert!(
        error.contains("The helper 'scout' was not started")
            && error.contains("do not fit in what is left of your budget"),
        "{error}"
    );
    let commands = agent_commands(&run.controller);
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].state, ControlCommandState::Rejected);
}

#[tokio::test]
async fn delegate_refuses_a_helper_that_can_change_files() {
    let fixture = lead_fixture_with_helpers(
        100000,
        &[
            ("scout", &["read_file"]),
            ("editor", &["read_file", "write_file"]),
        ],
    )
    .await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let mut scenario = Scenario::new("never produced");
    scenario.helper = "editor".into();
    let scenario = Arc::new(scenario);
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    let first_request = scenario.lead_requests.lock().unwrap()[0].1.clone();
    let tool = first_request
        .tools
        .iter()
        .find(|tool| tool.name == "delegate")
        .unwrap();
    assert_eq!(
        tool.parameters["properties"]["helper"]["enum"],
        serde_json::json!(["scout"]),
        "only read-only helpers are offered"
    );
    assert!(!tool.description.contains("editor"));
    let error = scenario.last_delegate_result();
    assert!(
        error.contains("The helper 'editor' can change files or run commands (write_file)"),
        "{error}"
    );
    assert_eq!(scenario.helper_calls(), 0);
    assert!(helper_node(&outcome.snapshot, &lead).is_none());
    assert!(agent_commands(&run.controller).is_empty());
}

#[tokio::test]
async fn delegate_is_unavailable_without_a_delegation_policy() {
    let fixture = native_fixture_with_invocations(8).await;
    let scenario = Arc::new(Scenario::new("never produced"));
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    let requests = scenario.lead_requests.lock().unwrap();
    assert!(!requests.is_empty());
    assert!(requests
        .iter()
        .all(|(_, request)| request.tools.iter().all(|tool| tool.name != "delegate")));
    // A forced call names an undeclared tool and fails before admission.
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert!(outcome.snapshot.contract().invocations().is_empty());
    assert_eq!(outcome.snapshot.contract().graph().unwrap().nodes.len(), 2);
    assert!(agent_commands(&run.controller).is_empty());
    assert_eq!(scenario.helper_calls(), 0);
}

#[tokio::test]
async fn lost_delegate_return_reconciles_after_restart_without_rerunning_helper() {
    use axocoatl_session::execution_ownership::UpgradedFormatOwnership;
    use axocoatl_session::invocation_audit::{
        InvocationFinalEvidence, InvocationOutcomeSource, InvocationReplayPolicy,
    };
    let fixture = lead_fixture(100000).await;
    let scenario = Arc::new(Scenario::new("pub fn run"));
    let run = run_lead(&fixture, scenario.clone(), true).await;
    assert!(run.outcome.is_err());
    let controller = run.controller;
    let (before, result, policy) = controller.delegate_recovery_evidence_for_test();
    assert!(result.is_none());
    assert!(before.final_evidence.is_none());
    assert!(matches!(
        before.intent.replay_policy,
        InvocationReplayPolicy::ReconcileBeforeReplay { .. }
    ));
    assert_eq!(policy["adapter"], "delegate-child-v1");
    // The real driver's error path retains child settlement tasks. Join that
    // ownership before simulating process reconstruction in this same process.
    controller.close_registered_repository_admission().unwrap();
    controller
        .wait_for_registered_executions(Duration::from_secs(5))
        .await
        .unwrap();
    let snapshot = controller.snapshot().unwrap();
    assert_eq!(scenario.helper_calls(), 1);
    drop(run.factory);
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
    let (after, result, _) = recovered.delegate_recovery_evidence_for_test();
    assert_eq!(after.intent, before.intent);
    assert!(matches!(
        after.final_evidence,
        Some(InvocationFinalEvidence::Outcome {
            outcome: InvocationOutcome::Succeeded,
            source: InvocationOutcomeSource::Reconciliation,
            ..
        })
    ));
    let result = result.unwrap();
    assert_eq!(result["Ok"]["status"], "completed");
    assert_eq!(result["Ok"]["reconciled"], true);
    assert_eq!(result["Ok"]["result"], "pub fn run");
    assert_eq!(result["Ok"]["node_id"], policy["node_id"]);
    assert_eq!(scenario.helper_calls(), 1, "reconstruction runs nothing");
    let retained = serde_json::to_value(recovered.snapshot().unwrap().contract()).unwrap();
    let evidence = after.final_evidence;
    drop(recovered);
    let repeated = reopen();
    assert_eq!(
        repeated
            .delegate_recovery_evidence_for_test()
            .0
            .final_evidence,
        evidence
    );
    assert_eq!(
        serde_json::to_value(repeated.snapshot().unwrap().contract()).unwrap(),
        retained
    );
    assert_eq!(scenario.helper_calls(), 1);
}

fn human_action(
    controller: &crate::session_dispatch::SessionDispatchController,
    id: &str,
    action: crate::session_dispatch::HumanControlAction,
    restart: Vec<ActivationRef>,
) -> crate::session_dispatch::HumanControlActionRequest {
    let snapshot = controller.snapshot().unwrap();
    let contract = snapshot.contract();
    crate::session_dispatch::HumanControlActionRequest {
        schema_version: 1,
        command_id: CommandId::new(id).unwrap(),
        session_id: snapshot.owner().session_id.clone(),
        turn_id: snapshot.turn_id().clone(),
        execution_epoch_id: contract.epochs().last().unwrap().id.clone(),
        expected_turn_revision: contract.revision(),
        expected_graph_revision: contract.graph().unwrap().revision,
        activation: None,
        action,
        instruction: None,
        include_previous_output: false,
        context: None,
        continuation: Some(crate::session_dispatch::HumanContinuationSelection {
            restart,
            checks: vec![],
        }),
        blocker_id: None,
        human_response: None,
        partial_finish: None,
    }
}

#[tokio::test]
async fn retried_lead_reattaches_to_accepted_helper() {
    let fixture = lead_fixture(100000).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let mut scenario = Scenario::new("pub fn run");
    scenario.lead_fails_first_generation = true;
    let scenario = Arc::new(scenario);
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let first = run.outcome.unwrap();
    assert_eq!(
        first.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert_eq!(scenario.helper_calls(), 1);
    let node = helper_node(&first.snapshot, &lead).unwrap();
    let restart = first
        .snapshot
        .contract()
        .activations()
        .iter()
        .filter(|item| item.state != ActivationState::Accepted)
        .map(|item| item.activation.clone())
        .collect::<Vec<_>>();
    assert_eq!(restart.len(), 1);
    assert_eq!(restart[0].node_id, lead);
    let request = human_action(
        &run.controller,
        "continue-lead",
        crate::session_dispatch::HumanControlAction::Continue,
        restart,
    );
    let receipt = fixture
        .registry
        .submit_human_action(
            fixture.request.session_id.as_str(),
            fixture.request.turn_id.as_str(),
            request.clone(),
            3,
        )
        .unwrap();
    assert_eq!(receipt.state, ControlCommandState::Settled, "{receipt:?}");
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
    let outcome = tokio::time::timeout(Duration::from_secs(10), driver.run())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed),
        "{:?}",
        outcome.snapshot.contract()
    );
    assert_eq!(scenario.helper_calls(), 1, "the retry reattaches");
    let requests = scenario.lead_requests.lock().unwrap();
    assert!(requests
        .iter()
        .any(|(activation, _)| activation.node_id == lead && activation.generation == 2));
    drop(requests);
    let result: serde_json::Value = serde_json::from_str(&scenario.last_delegate_result()).unwrap();
    assert_eq!(result["node_id"], node.as_str());
    assert_eq!(result["result"], "pub fn run");
    assert_eq!(agent_commands(&run.controller).len(), 1);
    assert_eq!(outcome.snapshot.contract().graph().unwrap().nodes.len(), 2);
}

#[tokio::test]
async fn revoked_lead_at_helper_provider_return_fails_without_poisoning_history() {
    let fixture = lead_fixture(100000).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let mut scenario = Scenario::new("Already claimed provider returned");
    scenario.hold_helper = Some(release.clone());
    let scenario = Arc::new(scenario);
    let (controller, repository) = begin(&fixture, &fixture.request);
    let factory = Arc::new(DelegateFactory {
        controller: controller.clone(),
        scenario: scenario.clone(),
        resolved: std::sync::Mutex::new(vec![]),
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
        panic!("native driver")
    };
    let revoke = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while scenario.helper_calls() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let helper = controller
            .snapshot()
            .unwrap()
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation.node_id != lead)
            .map(|item| item.activation.clone())
            .unwrap();
        controller.revoke_control_grant(LEAD_GRANT, 1).unwrap();
        release.add_permits(1);
        helper
    };
    let (result, helper) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(prepared.run(), revoke)
    })
    .await
    .unwrap();
    let result = result.unwrap();
    assert_eq!(
        result.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert!(result
        .snapshot
        .contract()
        .current_accepted_activations()
        .is_empty());
    let item = result
        .snapshot
        .contract()
        .activations()
        .iter()
        .find(|item| item.activation == helper)
        .unwrap();
    assert_eq!(item.state, ActivationState::Failed);
    let usage = controller.activation_provider_usage(&helper).unwrap();
    assert_eq!(usage.calls, 1);
    assert!(usage.tokens.complete);
    assert_eq!(usage.tokens.usage, TokenUsageStats::new(5, 5));
    assert_eq!(scenario.helper_calls(), 1);
    assert!(
        controller.history_snapshot().is_ok(),
        "ordinary revocation leaves history readable"
    );
    assert!(controller.control_plane().is_ok());
    assert!(fixture.registry.live_native_turns().unwrap().is_empty());
}
