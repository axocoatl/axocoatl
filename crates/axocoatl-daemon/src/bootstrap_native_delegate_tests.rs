//! Actual owned native Begin -> lead Agent -> `delegate` -> same-driver helper.
//! A Coordinator template runs through the same lead path; HTN methods retained
//! in an earlier build's approval are ignored. The finite local test providers
//! report only deterministic synthetic usage; this fixture is not a claim about
//! an external model or repository execution.
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
const SECOND_TASK: &str = "List every public struct in src/lib.rs and report only their names.";
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

async fn lead_fixture_with_helpers(
    aggregate_tokens: u64,
    helpers: &[(&str, &[&str])],
) -> NativeFixture {
    lead_fixture_as(aggregate_tokens, helpers, LeadTemplate::autonomous()).await
}

/// The lead slot's own template role and tools, its invocation allowance,
/// and the rest of its reviewed approval.
struct LeadTemplate {
    role: AgentRole,
    tools: Vec<String>,
    invocations: u32,
    operations: Vec<DelegatedOperation>,
    legacy_htn_methods_yaml: Option<String>,
    /// The team's required checks, which the lead pays for when it has bash.
    required_checks: Vec<Vec<String>>,
    /// Helper templates approved with `writes: []`.
    read_only_helpers: Vec<String>,
}
impl LeadTemplate {
    fn autonomous() -> Self {
        Self {
            role: AgentRole::Autonomous,
            tools: vec![],
            invocations: 20,
            operations: vec![DelegatedOperation::AddAgent],
            legacy_htn_methods_yaml: None,
            required_checks: vec![],
            read_only_helpers: vec![],
        }
    }
    /// A lead with bash on a team with one required check, which it pays
    /// for, with `invocations` in its allowance.
    fn paying(invocations: u32) -> Self {
        Self {
            tools: vec!["bash".into()],
            invocations,
            required_checks: vec![vec!["sh".into(), "-c".into(), "true".into()]],
            ..Self::autonomous()
        }
    }
    /// A lead that can read the repository but not change it or run
    /// commands, with `invocations` in its allowance.
    fn reader(invocations: u32) -> Self {
        Self {
            tools: vec!["read_file".into()],
            invocations,
            ..Self::autonomous()
        }
    }
    /// A Coordinator slot approved the way native Coordinators were before
    /// they ran as leads: several delegated operations and HTN methods.
    fn coordinator() -> Self {
        Self {
            role: AgentRole::Coordinator,
            tools: vec![],
            invocations: 20,
            operations: vec![
                DelegatedOperation::AddAgent,
                DelegatedOperation::StopActivation,
                DelegatedOperation::RetryActivation,
                DelegatedOperation::FinishNormally,
            ],
            legacy_htn_methods_yaml: Some(
                r#"
- task_pattern: "Do the work"
  preconditions: []
  subtasks:
    - name: "check-a"
      parameters: {description: "Check A"}
      task_type: Primitive
    - name: "check-b"
      parameters: {description: "Check B"}
      task_type: Primitive
"#
                .into(),
            ),
            required_checks: vec![],
            read_only_helpers: vec![],
        }
    }
}

/// One lead slot whose reviewed approval lets it add the given Worker helper
/// templates. Delegation is minted from it at turn start.
async fn lead_fixture_as(
    aggregate_tokens: u64,
    helpers: &[(&str, &[&str])],
    lead: LeadTemplate,
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
            let mut agents = vec![("lead", lead.role.clone(), lead.tools.clone())];
            for (name, tools) in helpers {
                agents.push((
                    name,
                    AgentRole::Worker,
                    tools.iter().map(|tool| tool.to_string()).collect(),
                ));
            }
            let mut retained = vec![];
            for (name, role, tools) in agents {
                let writes = lead
                    .read_only_helpers
                    .iter()
                    .any(|helper| helper == name)
                    .then(Vec::new);
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
                    writes: writes.clone(),
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
                    write_scope: writes,
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
                invocations: lead.invocations,
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
                operations: lead.operations.clone(),
                max_nodes: 8,
                max_edges: 0,
                legacy_htn_methods_yaml: lead.legacy_htn_methods_yaml.clone(),
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
                            "slots": [], "dependencies": [], "layout": [],
                            "required_checks": lead.required_checks},
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

/// Holds the first helper generation while its resources are prepared, before
/// it can reach a provider.
#[derive(Default)]
struct HelperSetupGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    selected: std::sync::Mutex<Option<ActivationRef>>,
}

/// What the lead asks for and how the helper behaves; every provider request
/// is recorded for the assertions. The lead delegates one task per round, in
/// order, or every task in its first round when `together`, and then answers.
struct Scenario {
    helper: String,
    tasks: Vec<String>,
    together: bool,
    /// Helpers inside their provider call. When set, each helper's provider
    /// waits there for another helper and fails if none arrives in time.
    rendezvous: Option<tokio::sync::watch::Sender<usize>>,
    answer: String,
    helper_fails: bool,
    /// The helper's first response carries its finding as text next to a
    /// call to `report`, a tool nobody declared, as small local models do.
    helper_calls_undeclared_tool_first: bool,
    /// The helper's response is refused after the provider reported its
    /// complete usage, as a malformed tool call is.
    helper_refused_after_usage: bool,
    lead_fails_first_generation: bool,
    hold_helper: Option<Arc<tokio::sync::Semaphore>>,
    hold_first_helper_setup: Option<Arc<HelperSetupGate>>,
    lead_requests: std::sync::Mutex<Vec<(ActivationRef, ChatRequest)>>,
    helper_requests: std::sync::Mutex<Vec<ChatRequest>>,
    helper_calls: AtomicUsize,
}
impl Scenario {
    fn new(answer: impl Into<String>) -> Self {
        Self {
            helper: "scout".into(),
            tasks: vec![TASK.into()],
            together: false,
            rendezvous: None,
            answer: answer.into(),
            helper_fails: false,
            helper_calls_undeclared_tool_first: false,
            helper_refused_after_usage: false,
            lead_fails_first_generation: false,
            hold_helper: None,
            hold_first_helper_setup: None,
            lead_requests: std::sync::Mutex::new(vec![]),
            helper_requests: std::sync::Mutex::new(vec![]),
            helper_calls: AtomicUsize::new(0),
        }
    }
    fn helper_calls(&self) -> usize {
        self.helper_calls.load(Ordering::SeqCst)
    }
    /// The lead's view of every delegate result in its last provider request,
    /// in call order.
    fn delegate_results(&self) -> Vec<String> {
        let requests = self.lead_requests.lock().unwrap();
        let (_, request) = requests.last().unwrap();
        request
            .messages
            .iter()
            .filter(|message| {
                message
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with("delegate-call"))
            })
            .map(|message| message.text_content().unwrap().to_owned())
            .collect()
    }
    /// The lead's view of the latest delegate result in its last request.
    fn last_delegate_result(&self) -> String {
        self.delegate_results()
            .pop()
            .expect("the lead's next request carries the delegate result")
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

/// Each early round delegates the next task; the round after the last task
/// answers from the delegate results.
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
        let tasks = match (self.scenario.together, round) {
            (true, 0) => self.scenario.tasks.iter().collect::<Vec<_>>(),
            (true, _) => vec![],
            (false, _) => self.scenario.tasks.get(round).into_iter().collect(),
        };
        if !tasks.is_empty() {
            let mut events = vec![StreamEvent::TextDelta {
                delta: LEAD_MARKER.into(),
            }];
            for (index, task) in tasks.into_iter().enumerate() {
                events.push(StreamEvent::ToolCallDelta {
                    index: Some(index),
                    id: format!("delegate-call-{round}-{index}"),
                    name: Some("delegate".into()),
                    args_delta: serde_json::json!({
                        "helper": self.scenario.helper,
                        "task": task,
                    })
                    .to_string(),
                });
            }
            return Ok(finished(events, FinishReason::ToolUse));
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
        let call = self.scenario.helper_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(hold) = &self.scenario.hold_helper {
            hold.acquire().await.unwrap().forget();
        }
        if let Some(inside) = &self.scenario.rendezvous {
            let mut others = inside.subscribe();
            inside.send_modify(|count| *count += 1);
            let met =
                tokio::time::timeout(Duration::from_secs(2), others.wait_for(|count| *count >= 2))
                    .await;
            if !matches!(met, Ok(Ok(_))) {
                inside.send_modify(|count| *count -= 1);
                return Ok(provider_failure("no other helper ran at the same time"));
            }
        }
        if self.scenario.helper_fails {
            return Ok(provider_failure("helper provider failed"));
        }
        if self.scenario.helper_calls_undeclared_tool_first && call == 0 {
            return Ok(finished(
                vec![
                    StreamEvent::TextDelta {
                        delta: "Found it: the comparison only checks neighbours.".into(),
                    },
                    StreamEvent::ToolCallDelta {
                        index: Some(0),
                        id: "report-call".into(),
                        name: Some("report".into()),
                        args_delta: serde_json::json!({"issue": "neighbours only"}).to_string(),
                    },
                ],
                FinishReason::ToolUse,
            ));
        }
        if self.scenario.helper_refused_after_usage {
            return Ok(Box::pin(tokio_stream::iter(vec![
                Ok(StreamEvent::UsageObservation(
                    axocoatl_core::MeasuredTokenUsage::known(TokenUsageStats::new(7, 3)),
                )),
                Err(ProviderError::RefusedResponse {
                    provider: "ollama".into(),
                    message: "provider returned malformed or non-object tool-call arguments".into(),
                }),
            ])));
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
        if config.role == AgentRole::Worker && input.activation.generation == 1 {
            if let Some(gate) = &self.scenario.hold_first_helper_setup {
                let selected = {
                    let mut selected = gate.selected.lock().unwrap();
                    if selected.is_none() {
                        *selected = Some(input.activation.clone());
                        true
                    } else {
                        false
                    }
                };
                if selected {
                    gate.entered.notify_one();
                    gate.release.notified().await;
                }
            }
        }
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

/// A first turn prepared for the lead but not yet driven.
struct Started<'a> {
    controller: crate::session_dispatch::SessionDispatchController,
    repository: EvidenceRef,
    bus: crate::stream::StreamBus,
    factory: Arc<DelegateFactory>,
    prepared: Box<crate::bootstrap::native_turn::PreparedNativeTurn<'a>>,
}

fn start_lead(fixture: &NativeFixture, scenario: Arc<Scenario>, lose_return: bool) -> Started<'_> {
    start_lead_with(fixture, scenario, |controller| {
        if lose_return {
            controller.lose_delegate_outcome_for_test();
        }
    })
}

/// A first turn whose controller `arm` prepares before the lead runs.
fn start_lead_with(
    fixture: &NativeFixture,
    scenario: Arc<Scenario>,
    arm: impl FnOnce(&crate::session_dispatch::SessionDispatchController),
) -> Started<'_> {
    let (controller, repository) = begin(fixture, &fixture.request);
    arm(&controller);
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
    Started {
        controller,
        repository,
        bus,
        factory,
        prepared,
    }
}

async fn run_lead(fixture: &NativeFixture, scenario: Arc<Scenario>, lose_return: bool) -> Run {
    let started = start_lead(fixture, scenario, lose_return);
    let outcome = tokio::time::timeout(Duration::from_secs(10), started.prepared.run())
        .await
        .unwrap()
        .map_err(|error| error.to_string());
    Run {
        controller: started.controller,
        repository: started.repository,
        bus: started.bus,
        factory: started.factory,
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
    assert!(
        tool.description
            .contains("find the relevant code and tests")
            && tool
                .description
                .contains("review your change against the task"),
        "the lead is told when a helper helps: {}",
        tool.description
    );
    assert!(
        tool.description.contains("steps and")
            && tool
                .description
                .contains("a step is one model call or one tool call"),
        "{}",
        tool.description
    );
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

    let plane = run.controller.control_plane().unwrap();
    let delegated = plane
        .edges
        .iter()
        .filter(|edge| edge.kind == "delegated_by")
        .collect::<Vec<_>>();
    assert_eq!(delegated.len(), 1);
    assert_eq!(delegated[0].source, lead.as_str());
    assert_eq!(delegated[0].target, node.as_str());
    assert_eq!(
        delegated[0].summary,
        crate::session_control_plane::EvidenceValue::Available {
            value: "scout".into()
        }
    );
    assert!(
        plane.nodes.iter().all(|item| item.dependencies.is_empty()),
        "a helper is not a dependency of its lead"
    );
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
async fn lead_and_helper_are_given_the_checkout_project_instructions() {
    let fixture = lead_fixture(100000).await;
    std::fs::write(
        fixture.repository._workspace.path().join("AXOCOATL.md"),
        "Check your change with `npm run check`.\n",
    )
    .unwrap();
    let scenario = Arc::new(Scenario::new("pub fn run"));
    let run = run_lead(&fixture, scenario.clone(), false).await;
    assert_eq!(
        run.outcome.unwrap().snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    let system = |messages: &[ChatMessage]| {
        messages
            .iter()
            .find(|message| message.role == axocoatl_core::MessageRole::System)
            .and_then(ChatMessage::text_content)
            .unwrap_or_default()
            .to_owned()
    };
    let lead = system(&scenario.lead_requests.lock().unwrap()[0].1.messages);
    assert!(
        lead.contains("Check your change with `npm run check`."),
        "{lead}"
    );
    assert!(lead.contains("--- from `AXOCOATL.md` ---"), "{lead}");
    let helper = system(&scenario.helper_requests.lock().unwrap()[0].messages);
    assert!(
        helper.contains("Check your change with `npm run check`."),
        "{helper}"
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

/// A helper's answer next to a call to a tool nobody declared is not lost:
/// the helper is told the tool does not exist, answers, and the lead gets it.
#[tokio::test]
async fn helper_calling_an_undeclared_tool_is_told_so_and_its_answer_reaches_the_lead() {
    let fixture = lead_fixture_with_helpers(100000, &[("scout", &["read_file"])]).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let mut scenario = Scenario::new("The comparison only checks neighbours.");
    scenario.helper_calls_undeclared_tool_first = true;
    let scenario = Arc::new(scenario);
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed),
        "{:?}",
        outcome.snapshot.contract()
    );
    assert_eq!(scenario.helper_calls(), 2);
    let node = helper_node(&outcome.snapshot, &lead).expect("one helper node");
    let accepted = outcome.snapshot.contract().current_accepted_activations();
    assert!(
        accepted.iter().any(|item| item.activation.node_id == node),
        "the helper finished"
    );

    let second = scenario.helper_requests.lock().unwrap()[1].clone();
    let refused = second
        .messages
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("report-call"))
        .expect("the undeclared call is answered in the helper's conversation");
    let refused = refused.text_content().unwrap();
    assert!(
        refused.contains("`report` is not an available tool. Available tools: ")
            && refused.contains("read_file")
            && refused.contains("If you are done, answer without calling a tool."),
        "{refused}"
    );
    let answer = scenario.last_delegate_result();
    assert!(
        answer.contains("The comparison only checks neighbours."),
        "{answer}"
    );
    assert!(!answer.contains("did not finish"), "{answer}");
    // The undeclared call was never admitted as an invocation.
    let helper_grant = outcome
        .snapshot
        .contract()
        .activations()
        .iter()
        .find(|item| item.activation.node_id == node)
        .and_then(|item| item.input.grant.as_ref())
        .unwrap()
        .grant_id
        .as_str()
        .to_owned();
    let helper_usage = run.controller.grant_usage_for_test(&helper_grant);
    assert_eq!(
        helper_usage.invocations, 2,
        "two provider calls, no tool call"
    );
    assert_eq!(helper_usage.tokens, 20);
}

/// A helper response refused after the provider reported its complete usage
/// is charged that usage, not the whole reservation, and the lead continues.
#[tokio::test]
async fn refused_helper_response_settles_the_usage_its_provider_reported() {
    let fixture = lead_fixture(100000).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let mut scenario = Scenario::new("never produced");
    scenario.helper_refused_after_usage = true;
    let scenario = Arc::new(scenario);
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
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
    let helper_grant = helper
        .input
        .grant
        .as_ref()
        .unwrap()
        .grant_id
        .as_str()
        .to_owned();
    let helper_usage = run.controller.grant_usage_for_test(&helper_grant);
    // The call reserved 100 tokens; its provider reported 10.
    assert_eq!(helper_usage.tokens, 10);
    let error = scenario.last_delegate_result();
    assert!(error.contains("did not finish"), "{error}");
}

#[tokio::test]
async fn delegated_helper_budget_is_reserved_from_the_lead_grant_and_its_unused_part_returned() {
    let fixture = lead_fixture(100000).await;
    let scenario = Arc::new(Scenario::new("pub fn run"));
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let helper = helper_node(&outcome.snapshot, &lead).unwrap();
    let helper_grant = outcome
        .snapshot
        .contract()
        .activations()
        .iter()
        .find(|item| item.activation.node_id == helper)
        .and_then(|item| item.input.grant.as_ref())
        .unwrap()
        .grant_id
        .as_str()
        .to_owned();
    let helper_usage = run.controller.grant_usage_for_test(&helper_grant);
    assert_eq!(helper_usage.activations, 1);
    // Each call reserved 100 tokens and settled to the 10 it reported.
    assert_eq!(helper_usage.tokens, 10);
    let usage = run.controller.grant_usage_for_test(LEAD_GRANT);
    // The lead's own activation and two calls, plus what the finished helper
    // used; the rest of the helper's reserved limits came back.
    assert_eq!(usage.activations, 1 + helper_usage.activations);
    assert_eq!(usage.tokens, 20 + helper_usage.tokens);
    // Two provider calls and the delegate call.
    assert_eq!(usage.invocations, 3 + helper_usage.invocations);
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

/// A helper with bash whose writes are `[]` is read-only: it is not offered
/// the file-writing tools and its shell cannot change the repository, so it
/// takes delegated work. The same template without `writes: []` is refused,
/// and the refusal says how to make it read-only.
#[tokio::test]
async fn a_helper_with_bash_takes_delegated_work_only_when_its_writes_are_empty() {
    for read_only in [true, false] {
        let mut template = LeadTemplate::autonomous();
        if read_only {
            template.read_only_helpers = vec!["shell".into()];
        }
        let fixture = lead_fixture_as(
            100000,
            &[
                ("scout", &[]),
                ("shell", &["read_file", "write_file", "bash"]),
            ],
            template,
        )
        .await;
        let lead = fixture.request.node_evidence[0].node_id.clone();
        let mut scenario = Scenario::new("pub fn run");
        scenario.helper = "shell".into();
        let scenario = Arc::new(scenario);
        let run = run_lead(&fixture, scenario.clone(), false).await;
        let snapshot = run.controller.snapshot().unwrap();
        let first_request = scenario.lead_requests.lock().unwrap()[0].1.clone();
        let tool = first_request
            .tools
            .iter()
            .find(|tool| tool.name == "delegate")
            .unwrap();
        if read_only {
            assert_eq!(
                tool.parameters["properties"]["helper"]["enum"],
                serde_json::json!(["scout", "shell"])
            );
            assert!(
                tool.description.contains(
                    "- shell: tools read_file, bash (its bash cannot change the repository)"
                ),
                "{}",
                tool.description
            );
            assert!(
                helper_node(&snapshot, &lead).is_some(),
                "the read-only helper is admitted"
            );
            let commands = agent_commands(&run.controller);
            assert_eq!(commands.len(), 1);
            assert!(matches!(
                commands[0].state,
                ControlCommandState::Applied | ControlCommandState::Settled
            ));
        } else {
            assert_eq!(
                tool.parameters["properties"]["helper"]["enum"],
                serde_json::json!(["scout"])
            );
            assert!(!tool.description.contains("shell"));
            let error = scenario.last_delegate_result();
            assert!(
                error.contains(
                    "The helper 'shell' can change files or run commands (write_file, bash)"
                ) && error.contains("setting `writes: []` on it"),
                "{error}"
            );
            assert_eq!(scenario.helper_calls(), 0);
            assert!(helper_node(&snapshot, &lead).is_none());
            assert!(agent_commands(&run.controller).is_empty());
        }
    }
}

#[tokio::test]
async fn delegate_is_unavailable_without_a_delegation_policy() {
    let fixture = native_fixture_with_invocations(8).await;
    let scenario = Arc::new(Scenario::new("never produced"));
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    {
        let requests = scenario.lead_requests.lock().unwrap();
        assert!(!requests.is_empty());
        assert!(requests
            .iter()
            .all(|(_, request)| request.tools.iter().all(|tool| tool.name != "delegate")));
    }
    // A forced call names an undeclared tool: it is answered with a tool
    // error before admission, records no invocation, and the lead finishes.
    // The dispatch gate's own refusal is covered by
    // `delegate_without_a_delegation_policy_is_refused_at_the_gate`.
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    let refused = scenario.last_delegate_result();
    assert!(
        refused.contains("`delegate` is not an available tool."),
        "{refused}"
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
    activation: Option<ActivationRef>,
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
        activation,
        action,
        instruction: None,
        include_previous_output: false,
        context: None,
        continuation: (!restart.is_empty()).then_some(
            crate::session_dispatch::HumanContinuationSelection {
                restart,
                checks: vec![],
            },
        ),
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
        None,
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

fn completed_result(text: &str) -> serde_json::Value {
    let value: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(value["status"], "completed", "{text}");
    value
}

#[tokio::test]
async fn coordinator_slot_runs_as_a_lead_that_reuses_one_helper_template_for_distinct_tasks() {
    let fixture = lead_fixture_as(100000, &[("scout", &[])], LeadTemplate::coordinator()).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let mut scenario = Scenario::new("pub fn run");
    scenario.tasks = vec![TASK.into(), SECOND_TASK.into()];
    let scenario = Arc::new(scenario);
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed),
        "{:?}",
        outcome.snapshot.contract()
    );
    // The retained HTN methods are ignored: the Coordinator template
    // streams as a lead, is offered delegate, and spends one round per task.
    {
        let requests = scenario.lead_requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0]
            .1
            .tools
            .iter()
            .any(|tool| tool.name == "delegate"));
    }
    let accepted = outcome.snapshot.contract().current_accepted_activations();
    assert_eq!(accepted.len(), 3);
    let helpers = accepted
        .iter()
        .filter(|item| item.activation.node_id != lead)
        .collect::<Vec<_>>();
    assert_eq!(helpers.len(), 2);
    assert_ne!(helpers[0].activation.node_id, helpers[1].activation.node_id);
    assert_ne!(helpers[0].conversation_id, helpers[1].conversation_id);
    assert_eq!(helpers[0].input.definition, helpers[1].input.definition);
    assert_eq!(scenario.helper_calls(), 2);
    let results = scenario.delegate_results();
    assert_eq!(results.len(), 2);
    let nodes = results
        .iter()
        .map(|result| completed_result(result)["node_id"].clone())
        .collect::<Vec<_>>();
    assert_ne!(nodes[0], nodes[1]);
    assert_eq!(outcome.snapshot.contract().graph_history().len(), 2);
    assert!(fixture.registry.live_native_turns().unwrap().is_empty());
    let commands = agent_commands(&run.controller);
    assert_eq!(commands.len(), 2);
    assert!(commands
        .iter()
        .all(|receipt| receipt.state == ControlCommandState::Settled));
}

/// Room for the lead's own calls and one helper's reserved limits, not two.
/// A finished helper gives back what it did not use, so the lead can
/// delegate again; helpers running at the same time still cannot both fit
/// (see `helpers_requested_together_cannot_exceed_the_lead_aggregate_budget`).
#[tokio::test]
async fn a_finished_helper_returns_its_unused_limits_so_the_lead_can_delegate_again() {
    let fixture = lead_fixture(15000).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let mut scenario = Scenario::new("pub fn run");
    scenario.tasks = vec![TASK.into(), SECOND_TASK.into()];
    let scenario = Arc::new(scenario);
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed),
        "{:?}",
        outcome.snapshot.contract()
    );
    assert_eq!(outcome.snapshot.contract().graph().unwrap().nodes.len(), 3);
    assert_eq!(scenario.helper_calls(), 2);
    let results = scenario.delegate_results();
    assert_eq!(results.len(), 2);
    assert_ne!(
        completed_result(&results[0])["node_id"],
        completed_result(&results[1])["node_id"]
    );
    let commands = agent_commands(&run.controller);
    assert_eq!(commands.len(), 2);
    assert!(commands
        .iter()
        .all(|receipt| receipt.state == ControlCommandState::Settled));
    assert!(outcome
        .snapshot
        .contract()
        .current_accepted_activations()
        .iter()
        .any(|item| item.activation.node_id == lead));
    let usage = run.controller.grant_usage_for_test(LEAD_GRANT);
    // Three lead calls and one call per helper, each settled to 10 tokens;
    // one activation each for the lead and both helpers.
    assert_eq!(usage.activations, 3);
    assert_eq!(usage.tokens, 50);
}

#[tokio::test]
async fn stopped_helper_continues_once_without_replaying_accepted_sibling_or_forking_driver() {
    let fixture = lead_fixture(100000).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let gate = Arc::new(HelperSetupGate::default());
    let mut scenario = Scenario::new("pub fn run");
    scenario.tasks = vec![TASK.into(), SECOND_TASK.into()];
    scenario.lead_fails_first_generation = true;
    scenario.hold_first_helper_setup = Some(gate.clone());
    let scenario = Arc::new(scenario);
    let started = start_lead(&fixture, scenario.clone(), false);
    let controller = started.controller.clone();
    let stop = async {
        gate.entered.notified().await;
        let helper = gate.selected.lock().unwrap().clone().unwrap();
        let request = human_action(
            &controller,
            "stop-one-helper",
            crate::session_dispatch::HumanControlAction::Stop,
            Some(helper.clone()),
            vec![],
        );
        let receipt = fixture
            .registry
            .submit_human_action(
                fixture.request.session_id.as_str(),
                fixture.request.turn_id.as_str(),
                request,
                2,
            )
            .unwrap();
        assert_eq!(receipt.state, ControlCommandState::Settled, "{receipt:?}");
        gate.release.notify_one();
        helper
    };
    let (first, stopped) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(started.prepared.run(), stop)
    })
    .await
    .unwrap();
    let first = first.unwrap();
    assert_eq!(
        first.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert_eq!(
        scenario.helper_calls(),
        1,
        "the stopped helper never reached its provider"
    );
    let results = scenario.delegate_results();
    assert_eq!(results.len(), 2);
    assert!(
        results[0].contains("The helper 'scout' did not finish")
            && results[0].contains(stopped.node_id.as_str()),
        "{}",
        results[0]
    );
    completed_result(&results[1]);
    let accepted = first.snapshot.contract().current_accepted_activations();
    assert_eq!(accepted.len(), 1);
    let sibling = accepted[0].activation.clone();
    assert_ne!(sibling.node_id, lead);
    let restart = first
        .snapshot
        .contract()
        .activations()
        .iter()
        .filter(|item| item.state != ActivationState::Accepted)
        .map(|item| item.activation.clone())
        .collect::<Vec<_>>();
    assert_eq!(restart.len(), 2);
    let request = human_action(
        &controller,
        "continue-stopped-helper",
        crate::session_dispatch::HumanControlAction::Continue,
        None,
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
    let driver = controller
        .prepare_native_control_driver(
            &request.command_id,
            started.repository.clone(),
            started.bus.clone(),
            started.factory.clone(),
        )
        .unwrap()
        .unwrap();
    assert!(controller
        .prepare_native_control_driver(
            &request.command_id,
            started.repository.clone(),
            started.bus.clone(),
            started.factory.clone()
        )
        .unwrap()
        .is_none());
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
    assert_eq!(outcome.snapshot.contract().graph().unwrap().nodes.len(), 3);
    assert_eq!(
        scenario.helper_calls(),
        2,
        "only the stopped helper runs again; its accepted sibling does not"
    );
    let accepted = outcome.snapshot.contract().current_accepted_activations();
    assert!(accepted.iter().any(|item| item.activation == sibling));
    assert!(accepted
        .iter()
        .any(|item| item.activation.node_id == stopped.node_id && item.activation.generation == 2));
    assert!(accepted
        .iter()
        .any(|item| item.activation.node_id == lead && item.activation.generation == 2));
    let results = scenario.delegate_results();
    assert_eq!(results.len(), 2);
    assert_eq!(
        completed_result(&results[0])["node_id"],
        stopped.node_id.as_str()
    );
    assert_eq!(
        completed_result(&results[1])["node_id"],
        sibling.node_id.as_str()
    );
    assert_eq!(agent_commands(&controller).len(), 2);
    assert_eq!(
        fixture
            .registry
            .submit_human_action(
                fixture.request.session_id.as_str(),
                fixture.request.turn_id.as_str(),
                request.clone(),
                4
            )
            .unwrap(),
        receipt
    );
    assert!(controller
        .prepare_native_control_driver(
            &request.command_id,
            started.repository,
            started.bus,
            started.factory
        )
        .unwrap()
        .is_none());
}

/// The Session after a process restart: a fresh repository owner, the stores
/// recovered from disk, and the latest turn attached to a new registry.
struct Restarted {
    registry: SessionDispatchRegistry,
    controller: crate::session_dispatch::SessionDispatchController,
    repository: EvidenceRef,
    /// The control-plane read after startup recovery's single open.
    first_open: serde_json::Value,
    request: NativeFirstTurnRequest,
    _owner: SessionRepositoryOwner,
    fixture: Fixture,
}

async fn restart(fixture: NativeFixture, turn: LogicalTurnId) -> Restarted {
    use axocoatl_session::execution_ownership::UpgradedFormatOwnership;
    let NativeFixture {
        repository: f,
        registry,
        request,
    } = fixture;
    drop(registry);
    drop(f.owner.retire_idle().unwrap());
    let sandbox = Arc::new(ControlledSandbox::new(
        f.owner.root(),
        "restarted-incarnation",
    ));
    let mut metadata = f.owner.metadata().clone();
    metadata.execution_identity.clone_from(&sandbox.incarnation);
    let registered: Arc<dyn Sandbox> = sandbox;
    f.owner
        .inner
        .sandboxes
        .lock()
        .await
        .insert(metadata.session_id.clone(), registered.clone());
    let owner = SessionRepositoryOwner {
        inner: Arc::new(RepositoryOwnerInner {
            attempt: None,
            identity: f.owner.identity().clone(),
            metadata,
            runtime: f.owner.inner.runtime.clone(),
            sandbox: registered,
            data_root: f.owner.inner.data_root.clone(),
            workspace_root: f.owner.inner.workspace_root.clone(),
            sessions: f.owner.inner.sessions.clone(),
            workspaces: f.owner.inner.workspaces.clone(),
            sandboxes: f.owner.inner.sandboxes.clone(),
            start: f.owner.inner.start.clone(),
            shutdown: f.owner.inner.shutdown.clone(),
            workspace_operation: Mutex::new(Some(f.operation.clone().lock_owned().await)),
            workspace_gate: f.operation.clone(),
            execution: Arc::new(AsyncMutex::new(())),
            state: Mutex::new(ExecutionState::default()),
            changed: Notify::new(),
        }),
    };
    owner.validate_current().await.unwrap();
    let session = owner
        .inner
        .sessions
        .lock()
        .await
        .get(&owner.metadata().session_id)
        .unwrap()
        .clone();
    let format = Arc::new(UpgradedFormatOwnership::open(f._data.path()).unwrap());
    let stores =
        crate::bootstrap::session_recovery::recover_session_stores(format, &session).unwrap();
    let registry = SessionDispatchRegistry::default();
    let token = registry.retain_existing_session(&mut Some(stores)).unwrap();
    // Startup recovery opened the turn once; read it before attaching opens
    // it again.
    let first_open = serde_json::to_value(
        registry
            .control_plane(request.session_id.as_str(), turn.as_str())
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let (controller, repository) = registry
        .attach_existing_turn(&token, turn, owner.clone())
        .unwrap();
    Restarted {
        registry,
        controller,
        repository,
        first_open,
        request,
        _owner: owner,
        fixture: f,
    }
}

/// The process stops while the lead's helper admission is recorded as
/// requested, or accepted but not yet applied. No helper ran. One reopen
/// resolves the lost return, and the continued lead delegates the same task
/// again, which runs the helper once and completes the turn.
async fn crash_in_helper_admission_then_continue(accepted: bool) {
    use axocoatl_session::invocation_audit::{InvocationFinalEvidence, InvocationOutcomeSource};
    let fixture = lead_fixture(100000).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let scenario = Arc::new(Scenario::new("pub fn run"));
    let Started {
        controller,
        factory,
        prepared,
        ..
    } = start_lead_with(&fixture, scenario.clone(), |controller| {
        controller.crash_delegate_admission_for_test(accepted)
    });
    let first = tokio::time::timeout(Duration::from_secs(10), prepared.run())
        .await
        .unwrap();
    assert!(first.is_err());
    let (before, result, _) = controller.delegate_recovery_evidence_for_test();
    assert!(result.is_none());
    assert!(before.final_evidence.is_none());
    controller.close_registered_repository_admission().unwrap();
    controller
        .wait_for_registered_executions(Duration::from_secs(5))
        .await
        .unwrap();
    let turn = controller.snapshot().unwrap().turn_id().clone();
    assert_eq!(scenario.helper_calls(), 0);
    drop(factory);
    drop(controller);

    let restarted = restart(fixture, turn.clone()).await;
    let delegate_calls = restarted.first_open["invocations"]["value"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|value| value["intent"]["tool_name"] == "delegate")
        .collect::<Vec<_>>();
    assert_eq!(delegate_calls.len(), 1);
    assert!(
        !delegate_calls[0]["final_evidence"].is_null(),
        "one open resolves the lost return: {}",
        delegate_calls[0]
    );
    let controller = restarted.controller.clone();
    let (after, result, _) = controller.delegate_recovery_evidence_for_test();
    assert_eq!(after.intent, before.intent);
    assert!(
        matches!(
            after.final_evidence,
            Some(InvocationFinalEvidence::Outcome {
                outcome: InvocationOutcome::Failed,
                source: InvocationOutcomeSource::Reconciliation,
                ..
            })
        ),
        "{:?}",
        after.final_evidence
    );
    let result = result.unwrap();
    let returned = result["Err"].as_str().unwrap();
    assert!(
        returned.contains("was not admitted, so no helper ran")
            && returned.contains("Call delegate again"),
        "{returned}"
    );
    let commands = agent_commands(&controller);
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].state,
        if accepted {
            ControlCommandState::Failed
        } else {
            ControlCommandState::Rejected
        }
    );
    let snapshot = controller.snapshot().unwrap();
    assert!(helper_node(&snapshot, &lead).is_none());
    assert_eq!(
        snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention),
        "{:?}",
        snapshot.contract()
    );

    let restart_lead = snapshot
        .contract()
        .activations()
        .iter()
        .filter(|item| item.state != ActivationState::Accepted)
        .map(|item| item.activation.clone())
        .collect::<Vec<_>>();
    assert_eq!(restart_lead.len(), 1);
    assert_eq!(restart_lead[0].node_id, lead);
    let request = human_action(
        &controller,
        "continue-after-restart",
        crate::session_dispatch::HumanControlAction::Continue,
        None,
        restart_lead,
    );
    let receipt = restarted
        .registry
        .submit_human_action(
            restarted.request.session_id.as_str(),
            turn.as_str(),
            request.clone(),
            now_ms(),
        )
        .unwrap();
    assert_eq!(receipt.state, ControlCommandState::Settled, "{receipt:?}");
    let factory = Arc::new(DelegateFactory {
        controller: controller.clone(),
        scenario: scenario.clone(),
        resolved: std::sync::Mutex::new(vec![]),
    });
    let driver = controller
        .prepare_native_control_driver(
            &request.command_id,
            restarted.repository.clone(),
            crate::stream::StreamBus::new(64),
            factory,
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
    assert_eq!(scenario.helper_calls(), 1, "the helper runs exactly once");
    let answer = completed_result(&scenario.last_delegate_result());
    let node = helper_node(&outcome.snapshot, &lead).unwrap();
    assert_eq!(answer["node_id"], node.as_str());
    let commands = agent_commands(&controller);
    assert_eq!(commands.len(), 2, "the retry is a fresh admission");
    assert_eq!(commands[1].state, ControlCommandState::Settled);
    assert!(restarted.registry.live_native_turns().unwrap().is_empty());
    // Every invocation has a known outcome, so the repository is released.
    restarted
        .registry
        .release_after_turn(restarted.request.session_id.as_str(), &turn)
        .unwrap();
    assert!(restarted.fixture.operation.try_lock().is_ok());
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[tokio::test]
async fn crash_before_helper_admission_is_accepted_resolves_after_one_reopen() {
    crash_in_helper_admission_then_continue(false).await;
}

#[tokio::test]
async fn crash_before_accepted_helper_admission_is_applied_resolves_after_one_reopen() {
    crash_in_helper_admission_then_continue(true).await;
}

/// A lead that can only read: its helper fits in what is left of its budget
/// but would leave too little for the lead to read the answer, so the helper
/// is not started and the lead finishes on its own.
async fn helper_that_leaves_the_lead_too_little_is_refused(
    aggregate_tokens: u64,
    invocations: u32,
    short: &str,
) {
    let fixture = lead_fixture_as(
        aggregate_tokens,
        &[("scout", &[])],
        LeadTemplate::reader(invocations),
    )
    .await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let scenario = Arc::new(Scenario::new("never produced"));
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed),
        "{:?}",
        outcome.snapshot.contract()
    );
    assert_eq!(scenario.helper_calls(), 0);
    assert!(helper_node(&outcome.snapshot, &lead).is_none());
    let error = scenario.last_delegate_result();
    assert!(
        error.contains("The helper 'scout' was not started")
            && error.contains("not enough to read its answer")
            && error.contains(short),
        "{error}"
    );
    assert!(
        agent_commands(&run.controller).is_empty(),
        "nothing is submitted"
    );
}

#[tokio::test]
async fn helper_that_leaves_the_lead_too_few_invocations_is_refused() {
    // One provider call and the delegate call are spent; the helper's 4
    // would use the rest, leaving no call to read its answer.
    helper_that_leaves_the_lead_too_little_is_refused(100000, 6, "0 steps").await;
}

#[tokio::test]
async fn helper_that_leaves_the_lead_too_few_tokens_is_refused() {
    // One provider call reserved 100 tokens and settled to the 10 it used;
    // the helper's 10000 would leave 50, less than the lead's next call
    // reserves.
    helper_that_leaves_the_lead_too_little_is_refused(10060, 20, "50 tokens").await;
}

/// A lead that runs commands and pays for the team's required checks cannot
/// hand its check allowance to a helper: a helper that fits in its budget
/// but would leave less than that allowance is not started.
#[tokio::test]
async fn a_lead_that_pays_for_required_checks_keeps_their_allowance_from_helpers() {
    let fixture = lead_fixture_as(100000, &[("scout", &[])], LeadTemplate::paying(14)).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let scenario = Arc::new(Scenario::new("never produced"));
    // This fixture's backend cannot supervise the checks the host runs after
    // the lead, so the turn's own outcome is not what this test observes.
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let snapshot = run.controller.snapshot().unwrap();
    assert!(snapshot
        .contract()
        .graph()
        .unwrap()
        .conditions
        .iter()
        .any(|condition| condition.condition_id.as_str() == "required-check:ready"));
    assert!(run
        .controller
        .with_grant_stores(|_, _, held| Ok(held
            .unwrap()
            .1
            .grant_pays_required_checks(LEAD_GRANT)
            .unwrap()))
        .unwrap());
    assert_eq!(scenario.helper_calls(), 0);
    assert!(helper_node(&snapshot, &lead).is_none());
    let error = scenario.last_delegate_result();
    assert!(
        error.contains("The helper 'scout' was not started")
            && error.contains("held for the host to observe your changes and run required checks"),
        "{error}"
    );
    assert!(
        agent_commands(&run.controller).is_empty(),
        "nothing is submitted"
    );
}

#[tokio::test]
async fn repeating_a_refused_helper_call_is_a_fresh_admission() {
    let fixture = lead_fixture(helper_limits().tokens - 1000).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let mut scenario = Scenario::new("never produced");
    scenario.tasks = vec![TASK.into(), TASK.into()];
    let scenario = Arc::new(scenario);
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    assert_eq!(scenario.helper_calls(), 0);
    assert!(helper_node(&outcome.snapshot, &lead).is_none());
    let results = scenario.delegate_results();
    assert_eq!(results.len(), 2);
    for result in &results {
        assert!(
            result.contains("The helper 'scout' was not started")
                && result.contains("do not fit in what is left of your budget"),
            "{result}"
        );
    }
    // The repeat is checked again as its own admission, not answered from
    // the recorded refusal.
    let commands = agent_commands(&run.controller);
    assert_eq!(commands.len(), 2);
    assert!(commands
        .iter()
        .all(|receipt| receipt.state == ControlCommandState::Rejected));
    assert_ne!(
        commands[0].request.command_id,
        commands[1].request.command_id
    );
}

#[tokio::test]
async fn identical_call_in_the_same_activation_returns_the_earlier_answer() {
    let fixture = lead_fixture(100000).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let mut scenario = Scenario::new("pub fn run");
    scenario.tasks = vec![TASK.into(), TASK.into()];
    let scenario = Arc::new(scenario);
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    assert_eq!(scenario.helper_calls(), 1, "the repeat reattaches");
    let node = helper_node(&outcome.snapshot, &lead).unwrap();
    let results = scenario.delegate_results();
    assert_eq!(results.len(), 2);
    for result in &results {
        let result = completed_result(result);
        assert_eq!(result["node_id"], node.as_str());
        assert_eq!(result["result"], "pub fn run");
    }
    assert_eq!(agent_commands(&run.controller).len(), 1);
    assert_eq!(outcome.snapshot.contract().graph().unwrap().nodes.len(), 2);
    assert_eq!(
        run.controller.grant_usage_for_test(LEAD_GRANT).activations,
        1 + 1,
        "the helper runs once, and its unused activation came back"
    );
}

#[tokio::test]
async fn unreadable_helper_proposal_is_an_error_not_a_required_node() {
    let fixture = lead_fixture(100000).await;
    let scenario = Arc::new(Scenario::new("pub fn run"));
    let run = run_lead(&fixture, scenario, false).await;
    run.outcome.unwrap();
    let admitted = agent_commands(&run.controller).remove(0);
    assert!(run
        .controller
        .is_delegate_child_for_test(&admitted)
        .unwrap());
    // A content-store read failure must not read as an older, required kind.
    let mut unreadable = admitted.clone();
    let ControlParameters::AddAgent { input, .. } = &mut unreadable.request.parameters else {
        panic!("delegate admits the helper with AddAgent")
    };
    input.grant.as_mut().unwrap().evidence = EvidenceRef::new("content-missing").unwrap();
    assert!(run
        .controller
        .is_delegate_child_for_test(&unreadable)
        .is_err());
    // A command that adds no Agent carries no proposal to read.
    let mut stop = admitted;
    let ControlParameters::AddAgent { input, .. } = &stop.request.parameters else {
        unreachable!()
    };
    stop.request.parameters = ControlParameters::StopActivation {
        activation: input.activation.clone(),
    };
    assert!(!run.controller.is_delegate_child_for_test(&stop).unwrap());
}

/// Two delegate calls of one model round admit their helpers one after the
/// other against the graph each admission leaves, and the helpers run at the
/// same time: each helper's provider waits for the other to arrive.
#[tokio::test]
async fn two_delegate_calls_in_one_round_run_their_helpers_at_the_same_time() {
    let fixture = lead_fixture(100000).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let mut scenario = Scenario::new("pub fn run");
    scenario.tasks = vec![TASK.into(), SECOND_TASK.into()];
    scenario.together = true;
    scenario.rendezvous = Some(tokio::sync::watch::channel(0).0);
    let scenario = Arc::new(scenario);
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed),
        "{:?}",
        outcome.snapshot.contract()
    );
    assert_eq!(
        scenario.lead_requests.lock().unwrap().len(),
        2,
        "one round delegates both tasks and the next reads both answers"
    );
    assert_eq!(scenario.helper_calls(), 2);
    let results = scenario.delegate_results();
    assert_eq!(results.len(), 2);
    let nodes = results
        .iter()
        .map(|result| {
            let result = completed_result(result);
            assert_eq!(result["result"], "pub fn run");
            result["node_id"].as_str().unwrap().to_owned()
        })
        .collect::<Vec<_>>();
    assert_ne!(nodes[0], nodes[1]);

    let contract = outcome.snapshot.contract();
    let history = contract.graph_history();
    assert_eq!(history.len(), 2, "one graph revision per helper");
    let start = history[0].previous.revision;
    assert_eq!(
        history[1].previous.revision,
        start + 1,
        "the second admission builds on the first"
    );
    assert_eq!(contract.graph().unwrap().revision, start + 2);
    assert_eq!(contract.graph().unwrap().nodes.len(), 3);
    let accepted = contract.current_accepted_activations();
    assert_eq!(accepted.len(), 3);
    assert!(accepted.iter().any(|item| item.activation.node_id == lead));

    let commands = agent_commands(&run.controller);
    assert_eq!(commands.len(), 2);
    assert!(commands
        .iter()
        .all(|receipt| receipt.state == ControlCommandState::Settled));

    let usage = run.controller.grant_usage_for_test(LEAD_GRANT);
    // The lead's own activation and two calls, plus what both finished
    // helpers used; the rest of their reserved limits came back.
    assert_eq!(usage.activations, 1 + 2);
    assert_eq!(usage.tokens, 20 + 2 * 10);
    assert!(fixture.registry.live_native_turns().unwrap().is_empty());
}

/// Two helpers requested together when the lead's budget holds only one:
/// one runs, the other call gets the plain refusal, and the lead finishes.
#[tokio::test]
async fn helpers_requested_together_cannot_exceed_the_lead_aggregate_budget() {
    let fixture = lead_fixture(15000).await;
    let mut scenario = Scenario::new("pub fn run");
    scenario.tasks = vec![TASK.into(), SECOND_TASK.into()];
    scenario.together = true;
    let scenario = Arc::new(scenario);
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed),
        "{:?}",
        outcome.snapshot.contract()
    );
    assert_eq!(scenario.helper_calls(), 1);
    assert_eq!(outcome.snapshot.contract().graph().unwrap().nodes.len(), 2);
    let results = scenario.delegate_results();
    assert_eq!(results.len(), 2);
    let (completed, refused): (Vec<_>, Vec<_>) = results
        .iter()
        .partition(|result| result.contains("\"status\":\"completed\""));
    assert_eq!(completed.len(), 1, "{results:?}");
    assert!(
        refused[0].contains("The helper 'scout' was not started")
            && refused[0].contains("do not fit in what is left of your budget"),
        "{}",
        refused[0]
    );
    let mut states = agent_commands(&run.controller)
        .into_iter()
        .map(|receipt| receipt.state)
        .collect::<Vec<_>>();
    states.sort_by_key(|state| format!("{state:?}"));
    assert_eq!(
        states,
        [ControlCommandState::Rejected, ControlCommandState::Settled]
    );
    assert!(run.controller.grant_usage_for_test(LEAD_GRANT).tokens <= 15000);
}

/// Two helpers that each fit, but together would leave the lead too few
/// tokens to read their answers: the second admission sees the first's
/// reservation, so only one is started.
#[tokio::test]
async fn helpers_requested_together_keep_the_lead_able_to_read_their_answers() {
    // One provider call settled to the 10 of its 100 reserved tokens it used;
    // after one helper's 10000, 10050 are left, and a second helper would
    // leave 50, less than the next call reserves.
    let fixture = lead_fixture_as(20060, &[("scout", &[])], LeadTemplate::reader(20)).await;
    let mut scenario = Scenario::new("pub fn run");
    scenario.tasks = vec![TASK.into(), SECOND_TASK.into()];
    scenario.together = true;
    let scenario = Arc::new(scenario);
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed),
        "{:?}",
        outcome.snapshot.contract()
    );
    assert_eq!(scenario.helper_calls(), 1);
    let results = scenario.delegate_results();
    assert_eq!(results.len(), 2);
    let (completed, refused): (Vec<_>, Vec<_>) = results
        .iter()
        .partition(|result| result.contains("\"status\":\"completed\""));
    assert_eq!(completed.len(), 1, "{results:?}");
    assert!(
        refused[0].contains("not enough to read its answer") && refused[0].contains("50 tokens"),
        "{}",
        refused[0]
    );
    let commands = agent_commands(&run.controller);
    assert_eq!(commands.len(), 1, "the declined helper submits nothing");
    assert_eq!(commands[0].state, ControlCommandState::Settled);
}

/// The same helper and task twice in one round admit one helper; both calls
/// return its answer.
#[tokio::test]
async fn identical_calls_in_one_round_run_one_helper() {
    let fixture = lead_fixture(100000).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let mut scenario = Scenario::new("pub fn run");
    scenario.tasks = vec![TASK.into(), TASK.into()];
    scenario.together = true;
    let scenario = Arc::new(scenario);
    let run = run_lead(&fixture, scenario.clone(), false).await;
    let outcome = run.outcome.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed),
        "{:?}",
        outcome.snapshot.contract()
    );
    assert_eq!(scenario.helper_calls(), 1);
    let node = helper_node(&outcome.snapshot, &lead).unwrap();
    let results = scenario.delegate_results();
    assert_eq!(results.len(), 2);
    for result in &results {
        let result = completed_result(result);
        assert_eq!(result["node_id"], node.as_str());
        assert_eq!(result["result"], "pub fn run");
    }
    assert_eq!(agent_commands(&run.controller).len(), 1);
    assert_eq!(outcome.snapshot.contract().graph().unwrap().nodes.len(), 2);
    assert_eq!(
        run.controller.grant_usage_for_test(LEAD_GRANT).activations,
        1 + 1,
        "the helper runs once, and its unused activation came back"
    );
}
