//! Actual owned native Begin -> lead Agent -> host-run required reviewer ->
//! verdict, through the same driver. The finite local test providers answer
//! with scripted text and report deterministic synthetic usage; this fixture is
//! not a claim about an external model.
use super::*;
use axocoatl_core::{AgentRole, ChatMessage, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    ProviderExecutionBounds, StreamEvent,
};
use axocoatl_session::turn_review::{ReviewVerdict, REVIEW_CONDITION_ID, REVIEW_NODE_ID};
use std::pin::Pin;
use tokio_stream::Stream;

const FINDING: &str = "src/lib.rs:3: the new function has no test";

/// One lead slot on a team whose Apply names a read-only reviewer for
/// `max_rounds` rounds.
async fn review_fixture(max_rounds: u32) -> NativeFixture {
    review_fixture_with(max_rounds, &[], &[], &["read_file"]).await
}

/// As [`review_fixture`], with the Apply's required `checks` and each
/// Agent's tools. A lead with bash pays for the checks.
async fn review_fixture_with(
    max_rounds: u32,
    checks: &[Vec<String>],
    lead_tools: &[&str],
    reviewer_tools: &[&str],
) -> NativeFixture {
    let mut fixture = native_fixture().await;
    let session_id = fixture.request.session_id.clone();
    let token = fixture
        .registry
        .session_team_token(session_id.as_str())
        .unwrap();
    let (grant, node) = fixture
        .registry
        .with_session_team_stores(&token, |canonical, content, _| {
            let slot_id = SessionTeamSlotId::new("lead-slot").unwrap();
            let node_id = TurnNodeId::new("lead-node").unwrap();
            let conversation_id = NodeConversationId::new("lead-conversation").unwrap();
            let mut retained = vec![];
            let owned = |tools: &[&str]| -> Vec<String> {
                tools.iter().map(|tool| (*tool).to_owned()).collect()
            };
            for (name, role, tools, writes, id) in [
                (
                    "lead",
                    AgentRole::Autonomous,
                    owned(lead_tools),
                    None,
                    conversation_id.as_str().to_owned(),
                ),
                (
                    "reviewer",
                    AgentRole::Worker,
                    owned(reviewer_tools),
                    Some(vec![]),
                    "approved-reviewer-fixture".to_owned(),
                ),
            ] {
                let definition_id = AgentDefinitionId::new(format!("{name}-definition")).unwrap();
                let config = AgentConfig {
                    id: AgentId::new(id),
                    name: name.into(),
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
                    DefinitionSnapshotRef {
                        definition_id,
                        snapshot,
                    },
                    profile,
                ));
            }
            // Paying for checks keeps a pass per round back from the lead.
            let lead_limits = GrantLimits {
                activations: 4,
                invocations: if checks.is_empty() { 8 } else { 20 },
                tokens: 100000,
                cost_microunits: 0,
            };
            let review_limits = GrantLimits {
                activations: 3,
                invocations: 6,
                tokens: 100000,
                cost_microunits: 0,
            };
            let mut approval = serde_json::json!({
                "kind": "authenticated_session_team_apply",
                "edit": {"command_id": "approve-review", "expected_configuration_revision": 1,
                    "slots": [], "dependencies": [], "layout": [],
                    "required_review": {"template_id": "reviewer", "max_rounds": max_rounds,
                        "limits": review_limits}},
                "templates": [[slot_id.as_str(), "lead"]],
                "review": {"template_id": "reviewer", "definition": retained[1].0,
                    "max_rounds": max_rounds, "limits": review_limits}
            });
            if !checks.is_empty() {
                approval["edit"]["required_checks"] = serde_json::json!(checks);
            }
            let issuer = content
                .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                    text: approval.to_string(),
                })
                .unwrap()
                .reference()
                .clone();
            let grant = AuthorityGrant {
                id: "lead-grant".into(),
                revision: 1,
                issuer_evidence: issuer,
                holder: node_id.clone(),
                descendants: vec![],
                allow_stop_descendants: false,
                delegation: None,
                profiles: vec![retained[0].1.clone()],
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
                    command_id: CommandId::new("approve-review").unwrap(),
                    expected_configuration_revision: 1,
                    graph: SessionTeamGraph {
                        slots: vec![SessionTeamSlot {
                            slot_id: slot_id.clone(),
                            node_id: node_id.clone(),
                            definition: retained[0].0.clone(),
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

/// What each reviewer round answers; every provider request is recorded.
struct Scenario {
    verdicts: Vec<String>,
    /// The lead's first generation loses its provider connection.
    lead_fails_first: bool,
    lead_requests: std::sync::Mutex<Vec<(ActivationRef, String)>>,
    reviewer_requests: std::sync::Mutex<Vec<(ActivationRef, String)>>,
}
impl Scenario {
    fn new(verdicts: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            verdicts: verdicts.iter().map(|verdict| verdict.to_string()).collect(),
            lead_fails_first: false,
            lead_requests: std::sync::Mutex::new(vec![]),
            reviewer_requests: std::sync::Mutex::new(vec![]),
        })
    }
    fn lead_generations(&self) -> Vec<u32> {
        self.lead_requests
            .lock()
            .unwrap()
            .iter()
            .map(|(activation, _)| activation.generation)
            .collect()
    }
}

type EventStream =
    Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>;

/// The lead answers once per generation; the reviewer answers its round's
/// scripted verdict.
struct ScriptedProvider {
    scenario: Arc<Scenario>,
    activation: ActivationRef,
    reviewer: bool,
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
        let generation = self.activation.generation;
        let answer = if self.reviewer {
            self.scenario
                .reviewer_requests
                .lock()
                .unwrap()
                .push((self.activation.clone(), text));
            self.scenario
                .verdicts
                .get(generation as usize - 1)
                .or(self.scenario.verdicts.last())
                .cloned()
                .unwrap_or_default()
        } else {
            self.scenario
                .lead_requests
                .lock()
                .unwrap()
                .push((self.activation.clone(), text));
            if self.scenario.lead_fails_first && generation == 1 {
                return Ok(Box::pin(tokio_stream::iter(vec![Err(
                    ProviderError::Stream("lead provider lost its connection".into()),
                )])));
            }
            format!("Lead answer, generation {generation}")
        };
        let events = vec![
            StreamEvent::TextDelta { delta: answer },
            StreamEvent::Usage(TokenUsageStats::new(5, 5)),
            StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ];
        Ok(Box::pin(tokio_stream::iter(events.into_iter().map(Ok))))
    }
}

struct ReviewFactory {
    controller: crate::session_dispatch::SessionDispatchController,
    scenario: Arc<Scenario>,
    resolved: std::sync::Mutex<Vec<ActivationInputManifest>>,
}
#[async_trait::async_trait]
impl AutonomousActivationFactory for ReviewFactory {
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
        let reviewer = config.role == AgentRole::Worker;
        Ok(AutonomousActivationResources {
            provider: Arc::new(ScriptedProvider {
                scenario: self.scenario.clone(),
                activation: input.activation.clone(),
                reviewer,
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
    factory: Arc<ReviewFactory>,
    repository: EvidenceRef,
    bus: crate::stream::StreamBus,
    outcome: crate::session_dispatch::TurnDriveOutcome,
}

async fn run_turn(fixture: &NativeFixture, scenario: Arc<Scenario>) -> Run {
    run_turn_within(fixture, scenario, Duration::from_secs(10))
        .await
        .unwrap()
}

/// Begin the turn and drive it to its first outcome within `limit`, or say
/// why it did not get there.
async fn run_turn_within(
    fixture: &NativeFixture,
    scenario: Arc<Scenario>,
    limit: Duration,
) -> std::result::Result<Run, String> {
    let (controller, repository) = begin(fixture, &fixture.request);
    let retained = repository.clone();
    let bus = crate::stream::StreamBus::new(64);
    let factory = Arc::new(ReviewFactory {
        controller: controller.clone(),
        scenario,
        resolved: std::sync::Mutex::new(vec![]),
    });
    let NativeFirstTurnStart::Prepared(prepared) = finish_owned_setup(
        &fixture.registry,
        controller.clone(),
        repository,
        &fixture.request.source().unwrap(),
        bus.clone(),
        factory.clone(),
    )
    .unwrap() else {
        panic!("owned native driver")
    };
    let outcome = tokio::time::timeout(limit, prepared.run())
        .await
        .map_err(|_| format!("the turn did not settle within {limit:?}"))?
        .map_err(|failure| failure.to_string())?;
    Ok(Run {
        controller,
        factory,
        repository: retained,
        bus,
        outcome,
    })
}

/// Continue the paused turn of `run`, restarting `restart` and running
/// `checks`, as the person would, and drive it to its next outcome.
async fn continue_turn(
    fixture: &NativeFixture,
    run: &mut Run,
    id: &str,
    restart: Vec<ActivationRef>,
    checks: Vec<ConditionId>,
) {
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
        continuation: Some(crate::session_dispatch::HumanContinuationSelection { restart, checks }),
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
        .unwrap()
        .unwrap();
}

/// Every recorded review observation, oldest first, with its proof.
fn review_observations(
    run: &Run,
) -> Vec<(
    axocoatl_session::turn_contract::ConditionObservation,
    serde_json::Value,
)> {
    let contract = run.outcome.snapshot.contract();
    contract
        .conditions()
        .iter()
        .filter(|observation| observation.condition_id.as_str() == REVIEW_CONDITION_ID)
        .map(|observation| {
            let proof = run
                .controller
                .with_team_stores(|_, content, _| {
                    let ActivationEvidenceContent::Guidance { text } = &content
                        .resolve_activation_evidence(&observation.evidence)
                        .unwrap()
                    else {
                        panic!("a review proof is retained guidance")
                    };
                    Ok(serde_json::from_str(text).unwrap())
                })
                .unwrap();
            (observation.clone(), proof)
        })
        .collect()
}

fn reviewer_node() -> TurnNodeId {
    TurnNodeId::new(REVIEW_NODE_ID).unwrap()
}

/// The reviewer the Apply named is admitted beside the lead as optional
/// work in a fresh conversation, with its own grant, and the review
/// condition covers the lead. An approval of the lead's exact answer
/// completes the turn after one round; the lead ran once.
#[tokio::test]
async fn a_reviewer_that_approves_completes_the_turn() {
    let fixture = review_fixture(2).await;
    std::fs::write(
        fixture.repository._workspace.path().join("AXOCOATL.md"),
        "Every public function keeps its documented edge cases.\n",
    )
    .unwrap();
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let scenario = Scenario::new(&["VERDICT: APPROVE\nNothing must change."]);
    let run = run_turn(&fixture, scenario.clone()).await;
    let contract = run.outcome.snapshot.contract();
    assert_eq!(
        contract.state(),
        Some(LogicalTurnState::Completed),
        "{contract:?}"
    );
    assert!(run.outcome.finalized.is_some());
    let graph = contract.graph().unwrap();
    let reviewer = graph
        .nodes
        .iter()
        .find(|node| node.node_id == reviewer_node())
        .unwrap();
    assert!(
        !reviewer.required,
        "the reviewer is not work the lead waits for"
    );
    assert_eq!(reviewer.starting_savepoint, ConversationSavepoint::Empty);
    assert!(reviewer
        .conversation_id
        .as_str()
        .starts_with("review-conversation-"));
    assert!(graph.dependencies.is_empty());
    let condition = graph
        .conditions
        .iter()
        .find(|condition| condition.condition_id.as_str() == REVIEW_CONDITION_ID)
        .unwrap();
    assert_eq!(condition.nodes, vec![lead.clone()]);
    assert_eq!(scenario.lead_generations(), vec![1]);
    let requests = scenario.reviewer_requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    let (activation, text) = &requests[0];
    assert_eq!(activation.generation, 1);
    assert!(text.contains("Do the work"), "the request: {text}");
    assert!(
        text.contains("Every public function keeps its documented edge cases."),
        "the reviewer is given the checkout's AXOCOATL.md: {text}"
    );
    assert!(text.contains("Lead answer, generation 1"), "{text}");
    assert!(text.contains("VERDICT: APPROVE"), "{text}");
    assert!(text.contains("no change to show"), "{text}");
    // The reviewer ran on its own grant, in the conversation admitted for it.
    let reviewed = run
        .factory
        .resolved
        .lock()
        .unwrap()
        .iter()
        .find(|input| input.activation.node_id == reviewer_node())
        .cloned()
        .unwrap();
    assert_eq!(reviewed.conversation_id, reviewer.conversation_id);
    assert!(reviewed
        .grant
        .as_ref()
        .unwrap()
        .grant_id
        .as_str()
        .starts_with("review-grant-"));
    let observations = review_observations(&run);
    assert_eq!(observations.len(), 1);
    let (observation, proof) = &observations[0];
    assert_eq!(observation.outcome, ConditionOutcome::Passed);
    assert_eq!(proof["verdict"], "approve");
    assert_eq!(proof["round"], 1);
    assert_eq!(proof["findings"], "Nothing must change.");
    assert!(contract.condition_satisfied(&observation.condition_id));
    let view = run.controller.control_plane().unwrap();
    let review = view.required_review.unwrap();
    assert_eq!(review.state, "approved");
    assert_eq!(review.verdict, Some(ReviewVerdict::Approve));
    assert_eq!(review.round, Some(1));
    assert_eq!(review.max_rounds, 2);
    assert_eq!(review.reviewer, "reviewer");
    assert!(review.current);
}

/// A request for changes goes back to the lead as a host revision in a new
/// epoch: the lead's next generation reads the findings and its previous
/// answer, the review runs again on the new answer, and its approval
/// completes the turn in round 2. The first verdict was bound to the lead's
/// first answer and no longer counts once that answer was revised.
#[tokio::test]
async fn changes_go_back_to_the_lead_and_a_second_round_approves() {
    let fixture = review_fixture(2).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let scenario = Scenario::new(&[&format!("VERDICT: CHANGES\n{FINDING}"), "VERDICT: APPROVE"]);
    let run = run_turn(&fixture, scenario.clone()).await;
    let contract = run.outcome.snapshot.contract();
    assert_eq!(
        contract.state(),
        Some(LogicalTurnState::Completed),
        "{contract:?}"
    );
    assert_eq!(scenario.lead_generations(), vec![1, 2]);
    let lead_requests = scenario.lead_requests.lock().unwrap().clone();
    let revised = &lead_requests[1].1;
    assert!(revised.contains(FINDING), "{revised}");
    assert!(
        revised.contains("asked for changes in review round 1 of 2"),
        "{revised}"
    );
    assert!(
        revised.contains("Lead answer, generation 1"),
        "the lead reads its previous answer"
    );
    // The lead is asked to answer each finding by id, and is told the
    // unnumbered findings are one finding, F1.
    assert!(
        revised.contains(
            "Answer every finding below in an ADJUDICATIONS block: a fenced JSON array of"
        ) && revised.contains("one entry per finding, then fix each finding you accept."),
        "{revised}"
    );
    assert!(revised.contains("they are one finding, F1."), "{revised}");
    let reviewer_requests = scenario.reviewer_requests.lock().unwrap().clone();
    assert_eq!(reviewer_requests.len(), 2);
    for (_, text) in &reviewer_requests {
        assert!(
            text.contains("Number each finding F1, F2, ... at the start of its line"),
            "every round's prompt asks for numbered findings: {text}"
        );
    }
    assert!(reviewer_requests[1].1.contains("Lead answer, generation 2"));
    assert!(!reviewer_requests[1].1.contains("Lead answer, generation 1"));
    assert!(reviewer_requests[1].1.contains("review round 2 of 2"));
    // The host continued in a new epoch that revised the lead.
    let epochs = contract.epochs();
    assert_eq!(epochs.len(), 2);
    let plan = epochs[1].continuation.as_ref().unwrap();
    assert!(plan.selections.iter().any(|selection| matches!(selection,
        ContinuationSelection::Revise { previous, .. } if previous.node_id == lead)));
    assert!(plan
        .condition_runs
        .iter()
        .any(|id| id.as_str() == REVIEW_CONDITION_ID));
    let records = run
        .controller
        .with_team_stores(|canonical, _, _| Ok(canonical.records().unwrap().to_vec()))
        .unwrap();
    assert!(records.iter().any(|record| {
        record
            .command_id
            .as_str()
            .starts_with("host-review-continue-")
            && matches!(record.event, TurnContractEvent::Continue { .. })
    }));
    let observations = review_observations(&run);
    assert_eq!(observations.len(), 2);
    let (first, first_proof) = &observations[0];
    assert_eq!(first.outcome, ConditionOutcome::Failed);
    assert_eq!(first_proof["verdict"], "changes");
    assert_eq!(first_proof["findings"], FINDING);
    assert_eq!(first_proof["continued"], true);
    assert_eq!(first.activations[0].generation, 1);
    let (second, second_proof) = &observations[1];
    assert_eq!(second.outcome, ConditionOutcome::Passed);
    assert_eq!(second_proof["round"], 2);
    assert_eq!(second.activations[0].generation, 2);
    // A loadout run reads every round from the same proofs, with the
    // findings sent back split by id.
    let rounds: Vec<_> = run
        .controller
        .with_team_stores(|_, content, _| {
            Ok(
                axocoatl_session::turn_review::review_proofs(&run.outcome.snapshot, content)
                    .unwrap()
                    .iter()
                    .map(|proof| proof.to_round())
                    .collect(),
            )
        })
        .unwrap();
    assert_eq!(rounds.len(), 2);
    assert!(rounds[0].continued && !rounds[1].continued && rounds[1].passed);
    assert_eq!(rounds[0].findings.len(), 1);
    assert_eq!(rounds[0].findings[0].id, "F1");
    assert_eq!(rounds[0].findings[0].text, FINDING);
    assert!(rounds[1].findings.is_empty());
    // Only the verdict about the current answer counts.
    let current = contract
        .current_condition(&first.condition_id)
        .unwrap()
        .clone();
    assert_eq!(current, *second);
    let review = run
        .controller
        .control_plane()
        .unwrap()
        .required_review
        .unwrap();
    assert_eq!(review.state, "approved");
    assert_eq!(review.round, Some(2));
}

/// Changes in the last round the host runs leave the turn needing attention
/// with the findings shown and the lead not run again; the person decides.
#[tokio::test]
async fn changes_after_the_last_round_need_attention_with_the_findings() {
    let fixture = review_fixture(2).await;
    let scenario = Scenario::new(&[&format!("VERDICT: CHANGES\n{FINDING}")]);
    let run = run_turn(&fixture, scenario.clone()).await;
    let contract = run.outcome.snapshot.contract();
    assert_eq!(contract.state(), Some(LogicalTurnState::NeedsAttention));
    assert!(run.outcome.finalized.is_none());
    assert_eq!(scenario.lead_generations(), vec![1, 2]);
    assert_eq!(scenario.reviewer_requests.lock().unwrap().len(), 2);
    let observations = review_observations(&run);
    assert_eq!(observations.len(), 2);
    assert_eq!(observations[1].1["continued"], false);
    let view = run.controller.control_plane().unwrap();
    let review = view.required_review.unwrap();
    assert_eq!(review.state, "changes");
    assert_eq!(review.verdict, Some(ReviewVerdict::Changes));
    assert_eq!(review.findings, FINDING);
    assert_eq!(review.round, Some(2));
    assert!(review.current);
    assert!(
        review.reason.contains("the last round the host runs"),
        "{}",
        review.reason
    );
    // The person can run the review again or revise the lead themselves.
    let controls = view.turn_controls.unwrap();
    assert!(controls
        .check_choices
        .iter()
        .any(|choice| choice.condition_id.as_str() == REVIEW_CONDITION_ID));
}

/// An answer without a verdict line approves nothing. While a round remains
/// the host asks the reviewer again once, about the same result; when that
/// answer has none either, or no round remains, the turn needs attention and
/// the lead is not sent anything.
#[tokio::test]
async fn an_unreadable_verdict_fails_closed() {
    for (max_rounds, asked) in [(1, 1), (3, 2)] {
        let fixture = review_fixture(max_rounds).await;
        let answer = "Looks good to me.\nVERDICT: APPROVE, with nits";
        let scenario = Scenario::new(&[answer]);
        let run = run_turn(&fixture, scenario.clone()).await;
        let contract = run.outcome.snapshot.contract();
        assert_eq!(contract.state(), Some(LogicalTurnState::NeedsAttention));
        assert_eq!(scenario.lead_generations(), vec![1]);
        assert_eq!(scenario.reviewer_requests.lock().unwrap().len(), asked);
        assert_eq!(contract.epochs().len(), 1);
        let observations = review_observations(&run);
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].0.outcome, ConditionOutcome::Failed);
        assert_eq!(observations[0].1["verdict"], "unreadable");
        assert_eq!(observations[0].1["round"], asked);
        let review = run
            .controller
            .control_plane()
            .unwrap()
            .required_review
            .unwrap();
        assert_eq!(review.state, "failed");
        assert!(
            review
                .reason
                .starts_with("The reviewer's answer had no single VERDICT"),
            "{}",
            review.reason
        );
        assert_eq!(
            review.reason.contains("after the host asked it again"),
            asked > 1,
            "{}",
            review.reason
        );
        assert_eq!(review.findings, answer);
    }
}

/// An answer without a verdict line is not a verdict: with a round left, the
/// host asks the reviewer again about the exact same result, showing it that
/// answer, and the second answer's approval completes the turn without
/// running the lead again.
#[tokio::test]
async fn a_reviewer_without_a_verdict_is_asked_again_about_the_same_result() {
    let fixture = review_fixture(2).await;
    let scenario = Scenario::new(&[
        "I found no defects in the change.",
        "VERDICT: APPROVE\nNothing must change.",
    ]);
    let run = run_turn(&fixture, scenario.clone()).await;
    let contract = run.outcome.snapshot.contract();
    assert_eq!(
        contract.state(),
        Some(LogicalTurnState::Completed),
        "{contract:?}"
    );
    assert_eq!(scenario.lead_generations(), vec![1]);
    assert_eq!(contract.epochs().len(), 1);
    let requests = scenario.reviewer_requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].1.contains("did not contain a verdict line"));
    let (activation, again) = &requests[1];
    assert_eq!(activation.generation, 2);
    assert!(again.contains("Lead answer, generation 1"), "{again}");
    assert!(again.contains("review round 2 of 2"), "{again}");
    assert!(
        again.contains("Your previous answer did not contain a verdict line."),
        "{again}"
    );
    assert!(
        again.contains("I found no defects in the change."),
        "the reviewer reads the answer it gave: {again}"
    );
    // The unreadable answer recorded nothing; the second round's approval is
    // the one verdict, about the lead's first and only answer.
    let observations = review_observations(&run);
    assert_eq!(observations.len(), 1);
    let (observation, proof) = &observations[0];
    assert_eq!(observation.outcome, ConditionOutcome::Passed);
    assert_eq!(observation.activations[0].generation, 1);
    assert_eq!(proof["round"], 2);
    assert_eq!(proof["verdict"], "approve");
    assert_eq!(proof["reviewer"]["generation"], 2);
    let review = run
        .controller
        .control_plane()
        .unwrap()
        .required_review
        .unwrap();
    assert_eq!(review.state, "approved");
    assert_eq!(review.round, Some(2));
}

/// A reviewer that writes its findings first and its verdict on the last
/// line, as a live reviewer did, is read: its approval completes the turn in
/// the first round, and the findings are the text before the verdict.
#[tokio::test]
async fn a_verdict_on_the_last_line_completes_the_turn() {
    let fixture = review_fixture(2).await;
    let findings = "Based on my review I found no defects.\n\n1. Every pair is checked.";
    let scenario = Scenario::new(&[&format!("{findings}\n\nVERDICT: APPROVE")]);
    let run = run_turn(&fixture, scenario.clone()).await;
    let contract = run.outcome.snapshot.contract();
    assert_eq!(
        contract.state(),
        Some(LogicalTurnState::Completed),
        "{contract:?}"
    );
    assert!(run.outcome.finalized.is_some());
    assert_eq!(scenario.lead_generations(), vec![1]);
    assert_eq!(scenario.reviewer_requests.lock().unwrap().len(), 1);
    let observations = review_observations(&run);
    assert_eq!(observations.len(), 1);
    let (observation, proof) = &observations[0];
    assert_eq!(observation.outcome, ConditionOutcome::Passed);
    assert_eq!(proof["verdict"], "approve");
    assert_eq!(proof["round"], 1);
    assert_eq!(proof["findings"], findings);
}

/// A team without a required review admits exactly the graph it did before:
/// no reviewer node, no review condition and no control plane review.
#[tokio::test]
async fn a_team_without_a_review_admits_no_reviewer() {
    let fixture = native_fixture().await;
    let token = fixture
        .registry
        .session_team_token(fixture.request.session_id.as_str())
        .unwrap();
    let setup = prepare_admission(
        &fixture.registry,
        &token,
        &SecureDir::open(fixture.repository._data.path()).unwrap(),
        &fixture.request,
        &fixture.request.source().unwrap(),
    )
    .unwrap();
    let graph = &setup.content.graph;
    assert_eq!(
        graph
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>(),
        ["node-0", "node-1"]
    );
    assert!(graph.conditions.is_empty());
    assert!(axocoatl_session::turn_review::review_node(graph).is_none());
    let (controller, _) = begin(&fixture, &fixture.request);
    assert!(controller
        .control_plane()
        .unwrap()
        .required_review
        .is_none());
}

/// A person's Continue that restarts only a failed lead leaves the reviewer,
/// which never ran, blocked in that epoch. Once the lead finishes the host
/// starts it anyway, in a new epoch that prepares it and runs only the
/// review, and its approval completes the turn.
#[tokio::test]
async fn a_reviewer_left_blocked_by_a_continue_still_reviews_the_result() {
    let fixture = review_fixture(2).await;
    let lead = fixture.request.node_evidence[0].node_id.clone();
    let scenario = Arc::new(Scenario {
        lead_fails_first: true,
        ..Arc::into_inner(Scenario::new(&["VERDICT: APPROVE"])).unwrap()
    });
    let mut run = run_turn(&fixture, scenario.clone()).await;
    let contract = run.outcome.snapshot.contract();
    assert_eq!(contract.state(), Some(LogicalTurnState::NeedsAttention));
    assert!(scenario.reviewer_requests.lock().unwrap().is_empty());
    let review = run
        .controller
        .control_plane()
        .unwrap()
        .required_review
        .unwrap();
    assert_eq!(review.state, "not_run");
    let failed = contract
        .activations()
        .iter()
        .find(|item| item.activation.node_id == lead)
        .unwrap()
        .activation
        .clone();
    continue_turn(&fixture, &mut run, "restart-lead", vec![failed], vec![]).await;
    let contract = run.outcome.snapshot.contract();
    assert_eq!(
        contract.state(),
        Some(LogicalTurnState::Completed),
        "{contract:?}"
    );
    assert_eq!(scenario.lead_generations(), vec![1, 2]);
    assert_eq!(scenario.reviewer_requests.lock().unwrap().len(), 1);
    let epochs = contract.epochs();
    assert_eq!(epochs.len(), 3);
    let person = epochs[1].continuation.as_ref().unwrap();
    assert!(person.selections.iter().any(|selection| matches!(selection,
        ContinuationSelection::LeaveUnmaterializedBlocked { node_id, .. } if *node_id == reviewer_node())));
    let host = epochs[2].continuation.as_ref().unwrap();
    assert!(host.selections.iter().any(|selection| matches!(selection,
        ContinuationSelection::PrepareUnmaterialized { input } if input.activation.node_id == reviewer_node())));
    assert_eq!(
        host.condition_runs
            .iter()
            .map(ConditionId::as_str)
            .collect::<Vec<_>>(),
        [REVIEW_CONDITION_ID]
    );
    let observations = review_observations(&run);
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].0.outcome, ConditionOutcome::Passed);
    assert_eq!(observations[0].0.activations[0].generation, 2);
}

/// Required checks and a required review together, on the actual supervised
/// sandbox, with a reviewer that has bash and so captures the tree it reads.
/// The host runs the checks on the lead's result and then the reviewer. The
/// reviewer is read-only, so its acceptance leaves the checks' readiness
/// current, and its verdict is recorded from its answer about the exact tree
/// the checks passed on. An approval completes the turn. A request for
/// changes sends the findings to the lead, the checks run again on its new
/// result, and the second round's approval completes the turn.
#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_required_checks_and_review_complete_the_turn() {
    use super::super::activation_tests::{actual_sandbox, git_init};
    let ready = ConditionId::new("required-check:ready").unwrap();
    for changes_first in [false, true] {
        let checks = vec![vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "test -f notes.txt".to_owned(),
        ]];
        let mut fixture = review_fixture_with(2, &checks, &["bash"], &["read_file", "bash"]).await;
        let sandbox = actual_sandbox(&mut fixture.repository).await;
        git_init(fixture.repository._workspace.path());
        std::fs::write(
            fixture.repository._workspace.path().join("notes.txt"),
            "draft\n",
        )
        .unwrap();
        let changes = format!("VERDICT: CHANGES\n{FINDING}");
        let verdicts: &[&str] = if changes_first {
            &[&changes, "VERDICT: APPROVE"]
        } else {
            &["VERDICT: APPROVE\nNothing must change."]
        };
        let scenario = Scenario::new(verdicts);
        let run = run_turn_within(&fixture, scenario.clone(), Duration::from_secs(300)).await;
        sandbox.stop_checked().await.unwrap();

        let run = run.unwrap();
        let contract = run.outcome.snapshot.contract();
        let view = run.controller.control_plane().unwrap();
        let readiness = view.required_check_readiness.clone().unwrap();
        let review = view.required_review.clone().unwrap();
        assert_eq!(
            contract.state(),
            Some(LogicalTurnState::Completed),
            "changes first: {changes_first}; readiness: {readiness:?}; review: {review:?}"
        );
        assert!(run.outcome.finalized.is_some());
        // The checks' readiness is still current after the reviewer finished.
        assert!(contract.condition_satisfied(&ready));
        assert_eq!(readiness.state, "passed", "{readiness:?}");
        let candidate = readiness.candidate_sha256.clone().unwrap();
        let rounds = if changes_first { 2 } else { 1 };
        // A pass of both captures and the check per round.
        assert_eq!(contract.condition_runs().len(), 3 * rounds);
        assert_eq!(
            scenario.lead_generations(),
            (1..=rounds as u32).collect::<Vec<_>>()
        );
        let requests = scenario.reviewer_requests.lock().unwrap().clone();
        assert_eq!(requests.len(), rounds);
        let (_, prompt) = requests.last().unwrap();
        assert!(
            prompt.contains(&format!("Repository tree reviewed: {candidate}")),
            "the reviewer is shown the tree the checks passed on: {prompt}"
        );
        let observations = review_observations(&run);
        assert_eq!(observations.len(), rounds);
        if changes_first {
            let (first, proof) = &observations[0];
            assert_eq!(first.outcome, ConditionOutcome::Failed);
            assert_eq!(proof["verdict"], "changes");
            assert_eq!(proof["continued"], true);
            assert!(scenario.lead_requests.lock().unwrap()[1]
                .1
                .contains(FINDING));
        }
        let (last, proof) = observations.last().unwrap();
        assert_eq!(last.outcome, ConditionOutcome::Passed, "{proof}");
        assert_eq!(proof["verdict"], "approve");
        assert_eq!(proof["round"], rounds);
        // The reviewer's own capture saw the tree it was shown.
        assert_eq!(proof["candidate_sha256"], candidate.as_str());
        assert_eq!(proof["reviewed_sha256"], candidate.as_str());
        assert_eq!(review.state, "approved");
        assert_eq!(review.verdict, Some(ReviewVerdict::Approve));
        assert_eq!(review.round, Some(rounds as u32));
        assert!(review.current);
    }
}
