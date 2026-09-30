use super::*;
use axocoatl_memory::checkpoint::AgentCheckpoint;

#[path = "session_dispatch_driver_tests.rs"]
mod driver_tests;

#[path = "session_dispatch_knowledge_tests.rs"]
mod knowledge_tests;

const REQUEST: &str = "Produce and verify the requested change";
const PARENT_V1: &str = "parent-final-generation-one";
const PARENT_V2: &str = "parent-final-generation-two";
const CHILD_V1: &str = "child-final-generation-one";
const CHILD_V2: &str = "child-final-generation-two";
const CHILD_FAILED: &str = "child-failed-private-draft";

/// Fixed responses with a finite call limit, no network and no monetary effect.
/// Distinct output strings expose accidental restoration of another generation.
struct InputProvider {
    provider: &'static str,
    output: &'static str,
    tools_first: bool,
    fail: bool,
    calls: AtomicUsize,
    requests: Mutex<Vec<Vec<ChatMessage>>>,
}

impl InputProvider {
    fn new(output: &'static str, tools_first: bool, fail: bool) -> Arc<Self> {
        Self::named("controlled", output, tools_first, fail)
    }

    fn named(provider: &'static str, output: &'static str, tools_first: bool, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            provider,
            output,
            tools_first,
            fail,
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn first_input(&self) -> String {
        self.requests.lock().unwrap()[0]
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::User)
            .unwrap()
            .text_content()
            .unwrap()
            .to_owned()
    }
}

#[async_trait]
impl LlmProvider for InputProvider {
    fn provider_id(&self) -> &str {
        self.provider
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
        Some(ProviderExecutionBounds {
            token_limit: 100,
            cost_microunits: 0,
            response_bytes: 8192,
        })
    }
    async fn chat(&self, _: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        unreachable!("the actual autonomous actor streams")
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<
        Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(call < if self.tools_first { 2 } else { 1 });
        self.requests.lock().unwrap().push(request.messages);
        let tool = self.tools_first && call == 0;
        let mut events = if tool {
            vec![Ok(StreamEvent::ToolCallDelta {
                index: Some(0),
                id: "parent-private-native-call".into(),
                name: Some("effect".into()),
                args_delta: r#"{"value":"parent-private-tool-argument"}"#.into(),
            })]
        } else {
            vec![Ok(StreamEvent::TextDelta {
                delta: self.output.into(),
            })]
        };
        events.push(Ok(StreamEvent::Usage(TokenUsageStats::new(10, 2))));
        if self.fail {
            events.push(Err(ProviderError::Stream(
                "controlled response failure".into(),
            )));
        } else {
            events.push(Ok(StreamEvent::Done {
                finish_reason: if tool {
                    FinishReason::ToolUse
                } else {
                    FinishReason::Stop
                },
            }));
        }
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

#[derive(Clone)]
struct InputNode {
    input: ActivationInputManifest,
    config: AgentConfig,
    profile: ExecutionProfile,
}

struct InputFixture {
    _root: tempfile::TempDir,
    controller: SessionDispatchController,
    parent: InputNode,
    child: InputNode,
}

fn input_fixture() -> InputFixture {
    input_fixture_with_review(false)
}

fn input_fixture_with_review(required_review: bool) -> InputFixture {
    let root = tempfile::tempdir().unwrap();
    let ownership = Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let owner = ExecutionStoreOwner {
        workspace_id: "input-workspace".into(),
        session_id: SessionId::new("input-session").unwrap(),
    };
    let mut canonical = SessionExecutionStore::open(ownership, owner.clone()).unwrap();
    let mut content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let turn_id = LogicalTurnId::new("input-turn").unwrap();
    let request = content
        .retain_request(ExecutionRequestContent {
            turn_id: turn_id.clone(),
            recorded_at_unix_ms: now_ms().unwrap(),
            display_input: REQUEST.into(),
            effective_input: REQUEST.into(),
            context: vec![],
            target_definition: None,
            model: None,
        })
        .unwrap();
    let limits = GrantLimits {
        activations: 16,
        invocations: 32,
        tokens: 10_000,
        cost_microunits: 0,
    };
    let budget = content
        .retain_activation_evidence(ActivationEvidenceContent::Budget {
            limits: limits.clone(),
        })
        .unwrap();
    let approval = content
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: "Host approved this bounded parent and child graph".into(),
        })
        .unwrap();
    let mut nodes = Vec::new();
    for name in ["parent", "child"] {
        let config = AgentConfig {
            id: AgentId::new(format!("{name}-conversation")),
            name: name.into(),
            provider: "controlled".into(),
            model: "controlled-model".into(),
            tools: vec!["effect".into()],
            ..Default::default()
        };
        let profile = ExecutionProfile {
            definition: name.into(),
            provider: config.provider.clone(),
            model: config.model.clone(),
            isolation: "in-process".into(),
            tools: config.tools.clone(),
        };
        let definition_id = AgentDefinitionId::new(name).unwrap();
        let definition = content
            .retain_activation_evidence(ActivationEvidenceContent::Definition {
                definition_id: definition_id.clone(),
                revision: 1,
                profile: profile.clone(),
                configuration: serde_json::to_string(&config).unwrap(),
            })
            .unwrap();
        let guidance = content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: format!("{name}-only-initial-guidance"),
            })
            .unwrap();
        nodes.push(InputNode {
            input: ActivationInputManifest {
                manifest_id: InputManifestId::new(format!("{name}-input-1")).unwrap(),
                activation: ActivationRef {
                    session_id: owner.session_id.clone(),
                    turn_id: turn_id.clone(),
                    execution_epoch_id: ExecutionEpochId::new("input-epoch").unwrap(),
                    node_id: TurnNodeId::new(name).unwrap(),
                    generation: 1,
                    activation_id: ActivationId::new(format!("{name}-activation-1")).unwrap(),
                },
                definition: DefinitionSnapshotRef {
                    definition_id,
                    snapshot: definition.reference().clone(),
                },
                conversation_id: NodeConversationId::new(config.id.0.clone()).unwrap(),
                starting_savepoint: ConversationSavepoint::Empty,
                parents: vec![],
                guidance: vec![request.reference().clone(), guidance.reference().clone()],
                attachments: vec![],
                repository: RepositoryInput::Unavailable,
                budget: budget.reference().clone(),
                grant: None,
                revision_context: None,
            },
            config,
            profile,
        });
    }
    let policy = AuthorityGrant {
        id: "input-grant".into(),
        revision: 1,
        issuer_evidence: approval.reference().clone(),
        holder: nodes[0].input.activation.node_id.clone(),
        descendants: vec![nodes[1].input.activation.node_id.clone()],
        allow_stop_descendants: false,
        delegation: None,
        conditions: vec![],
        profiles: nodes.iter().map(|node| node.profile.clone()).collect(),
        limits,
        expires_at_ms: now_ms().unwrap() + 3_600_000,
    };
    let grant = content
        .retain_activation_evidence(ActivationEvidenceContent::Grant {
            policy: policy.clone(),
        })
        .unwrap();
    for node in &mut nodes {
        node.input.grant = Some(GrantSnapshotRef {
            grant_id: GrantId::new("input-grant").unwrap(),
            revision: 1,
            evidence: grant.reference().clone(),
        });
    }
    canonical
        .begin_with_request(
            TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("input-begin").unwrap(),
                expected_revision: 0,
                session_id: owner.session_id,
                turn_id: turn_id.clone(),
                event: TurnContractEvent::Begin {
                    epoch_id: nodes[0].input.activation.execution_epoch_id.clone(),
                    predecessor: None,
                    graph: TurnGraphSnapshot {
                        snapshot_id: GraphSnapshotId::new("input-graph").unwrap(),
                        revision: 1,
                        nodes: nodes
                            .iter()
                            .map(|node| GraphNode {
                                node_id: node.input.activation.node_id.clone(),
                                slot_id: SessionTeamSlotId::new(format!(
                                    "{}-slot",
                                    node.input.activation.node_id.as_str()
                                ))
                                .unwrap(),
                                definition: node.input.definition.clone(),
                                conversation_id: node.input.conversation_id.clone(),
                                starting_savepoint: ConversationSavepoint::Empty,
                                required: true,
                            })
                            .collect(),
                        dependencies: vec![DependencyEdge {
                            parent: nodes[0].input.activation.node_id.clone(),
                            child: nodes[1].input.activation.node_id.clone(),
                        }],
                        conditions: if required_review {
                            vec![CompletionCondition {
                                condition_id: ConditionId::new("both-results-reviewed").unwrap(),
                                kind: ConditionKind::Review { criterion: approval.reference().clone() },
                                nodes: nodes.iter().map(|node| node.input.activation.node_id.clone()).collect(),
                            }]
                        } else { vec![] },
                    },
                },
            },
            &request,
        )
        .unwrap();
    drop(content);
    let controller = SessionDispatchController::open(canonical, turn_id).unwrap();
    controller.install_grant(policy).unwrap();
    let child = nodes.pop().unwrap();
    let parent = nodes.pop().unwrap();
    InputFixture {
        _root: root,
        controller,
        parent,
        child,
    }
}

fn input_event(
    controller: &SessionDispatchController,
    event: TurnContractEvent,
) -> TurnContractEnvelope {
    let snapshot = controller.snapshot().unwrap();
    let state = controller.lock().unwrap();
    TurnContractEnvelope {
        schema_version: TURN_CONTRACT_SCHEMA_VERSION,
        command_id: CommandId::new(format!("input-event-{}", snapshot.contract().revision()))
            .unwrap(),
        expected_revision: snapshot.contract().revision(),
        session_id: state.canonical.owner().session_id.clone(),
        turn_id: state.turn_id.clone(),
        event,
    }
}

fn apply_input_event(controller: &SessionDispatchController, event: TurnContractEvent) {
    controller
        .append_host_event(input_event(controller, event))
        .unwrap();
}

fn start_input(controller: &SessionDispatchController, node: &InputNode) {
    apply_input_event(
        controller,
        TurnContractEvent::StartActivation {
            input: Box::new(node.input.clone()),
        },
    );
}

fn input_resources(
    node: &InputNode,
    provider: Arc<InputProvider>,
) -> AutonomousActivationResources {
    let mut tools = axocoatl_tools::ToolExecutor::new();
    if node.config.tools.iter().any(|tool| tool == "effect") {
        tools.register_builtin("effect", Arc::new(CountingTool::default()));
    }
    AutonomousActivationResources {
        config: node.config.clone(),
        profile: node.profile.clone(),
        provider,
        counter: Arc::new(Counter),
        tools: Arc::new(tools),
    }
}

async fn run_input(
    controller: &SessionDispatchController,
    node: &InputNode,
    provider: Arc<InputProvider>,
) -> SettledActivation {
    controller
        .prepare_autonomous_activation(
            node.input.activation.clone(),
            input_resources(node, provider),
        )
        .unwrap()
        .run()
        .await
        .unwrap()
}

fn accepted_parent(result: &SettledActivation) -> AcceptedParentInput {
    assert!(result.accepted, "{:?}", result.failure);
    AcceptedParentInput {
        activation: result.activation.clone(),
        checkpoint: result.checkpoint.clone().unwrap(),
        output: result.output.reference().clone(),
    }
}

fn next_node(node: &InputNode) -> InputNode {
    let mut next = node.clone();
    let generation = node.input.activation.generation + 1;
    let name = node.input.activation.node_id.as_str();
    next.input.manifest_id = InputManifestId::new(format!("{name}-input-{generation}")).unwrap();
    next.input.activation.activation_id =
        ActivationId::new(format!("{name}-activation-{generation}")).unwrap();
    next.input.activation.generation = generation;
    next
}

fn saved_input(
    controller: &SessionDispatchController,
    result: &SettledActivation,
    provider: &InputProvider,
) -> AgentCheckpoint {
    let state = controller.lock().unwrap();
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    let activation = snapshot
        .contract()
        .activations()
        .iter()
        .find(|item| item.activation == result.activation)
        .unwrap();
    if result.accepted {
        assert_eq!(activation.checkpoint.as_ref(), result.checkpoint.as_ref());
        assert_eq!(activation.output.as_ref(), Some(result.output.reference()));
    }
    let checkpoint = state
        .memory
        .checkpoint(result.checkpoint.as_ref().unwrap())
        .unwrap();
    assert_eq!(
        checkpoint.session_messages[0].content,
        provider.first_input()
    );
    assert_eq!(checkpoint.agent_id, activation.conversation_id.as_str());
    checkpoint
}

fn assert_projected_only(provider: &InputProvider, included: &[&str], excluded: &[&str]) {
    let requests = provider.requests.lock().unwrap();
    let messages = &requests[0];
    assert!(messages.iter().all(|message| message.tool_calls.is_empty()
        && matches!(message.role, MessageRole::System | MessageRole::User)));
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .count(),
        1
    );
    let input = messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::User)
        .unwrap()
        .text_content()
        .unwrap();
    for expected in included {
        assert!(
            input.contains(expected),
            "missing {expected:?} from {input:?}"
        );
    }
    for forbidden in excluded {
        assert!(
            !input.contains(forbidden),
            "obsolete/foreign {forbidden:?} in {input:?}"
        );
    }
}

#[tokio::test]
async fn autonomous_parent_revision_rebase_and_retry_project_exact_output_without_foreign_history()
{
    let fixture = input_fixture();
    let controller = &fixture.controller;
    start_input(controller, &fixture.parent);
    let parent_provider = InputProvider::new(PARENT_V1, true, false);
    let parent = run_input(controller, &fixture.parent, parent_provider.clone()).await;
    let first_parent_checkpoint = saved_input(controller, &parent, &parent_provider);
    assert!(first_parent_checkpoint
        .session_messages
        .iter()
        .any(|message| message.role == MessageRole::Tool));
    assert_eq!(
        first_parent_checkpoint.cumulative_token_usage,
        TokenUsageStats::new(20, 4)
    );

    let mut child_node = fixture.child.clone();
    child_node.input.parents = vec![accepted_parent(&parent)];
    start_input(controller, &child_node);
    let child_provider = InputProvider::new(CHILD_V1, false, false);
    let child = run_input(controller, &child_node, child_provider.clone()).await;
    assert!(child.accepted, "{:?}", child.failure);
    assert_projected_only(
        &child_provider,
        &[REQUEST, PARENT_V1, "child-only-initial-guidance"],
        &[
            "parent-only-initial-guidance",
            "parent-private-native-call",
            "parent-private-tool-argument",
        ],
    );
    let checkpoint = saved_input(controller, &child, &child_provider);
    assert_eq!(checkpoint.session_messages.len(), 2);
    assert_eq!(checkpoint.session_messages[1].content, CHILD_V1);
    assert_eq!(
        checkpoint.cumulative_token_usage,
        TokenUsageStats::new(10, 2)
    );

    let revision = controller
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: "child-revision-guidance".into(),
        })
        .unwrap();
    let mut revised_child = next_node(&child_node);
    revised_child.input.guidance.push(revision.clone());
    revised_child.input.revision_context = Some(RevisionContext {
        activation: child.activation.clone(),
        output: child.output.reference().clone(),
    });
    apply_input_event(
        controller,
        TurnContractEvent::ReviseAccepted {
            previous: child.activation.clone(),
            input: Box::new(revised_child.input.clone()),
            invalidated_descendants: vec![],
            evidence: revision.clone(),
        },
    );
    apply_input_event(
        controller,
        TurnContractEvent::StartPreparedActivation {
            activation: revised_child.input.activation.clone(),
        },
    );
    let revised_child_provider = InputProvider::new(CHILD_V2, false, false);
    let child_revision =
        run_input(controller, &revised_child, revised_child_provider.clone()).await;
    assert!(child_revision.accepted, "{:?}", child_revision.failure);
    assert_projected_only(
        &revised_child_provider,
        &[REQUEST, PARENT_V1, CHILD_V1, "child-revision-guidance"],
        &["parent-private-native-call"],
    );
    let checkpoint = saved_input(controller, &child_revision, &revised_child_provider);
    assert_eq!(checkpoint.session_messages.len(), 2);
    assert_eq!(
        checkpoint.cumulative_token_usage,
        TokenUsageStats::new(20, 4)
    );

    let mut revised_parent = next_node(&fixture.parent);
    revised_parent.input.revision_context = Some(RevisionContext {
        activation: parent.activation.clone(),
        output: parent.output.reference().clone(),
    });
    apply_input_event(
        controller,
        TurnContractEvent::ReviseAccepted {
            previous: parent.activation.clone(),
            input: Box::new(revised_parent.input.clone()),
            invalidated_descendants: vec![child_revision.activation.clone()],
            evidence: revision,
        },
    );
    apply_input_event(
        controller,
        TurnContractEvent::StartPreparedActivation {
            activation: revised_parent.input.activation.clone(),
        },
    );
    let revised_parent_provider = InputProvider::new(PARENT_V2, false, false);
    let parent_revision =
        run_input(controller, &revised_parent, revised_parent_provider.clone()).await;
    assert!(parent_revision.accepted, "{:?}", parent_revision.failure);
    assert_projected_only(
        &revised_parent_provider,
        &[REQUEST, PARENT_V1],
        &[
            "parent-private-native-call",
            "parent-private-tool-argument",
            CHILD_V1,
            CHILD_V2,
        ],
    );
    let parent_checkpoint = saved_input(controller, &parent_revision, &revised_parent_provider);
    assert_eq!(parent_checkpoint.session_messages.len(), 2);
    assert_eq!(
        parent_checkpoint.cumulative_token_usage,
        TokenUsageStats::new(30, 6)
    );

    let mut rebased = next_node(&revised_child);
    rebased.input.parents = vec![accepted_parent(&parent_revision)];
    rebased.input.revision_context = None;
    assert_eq!(
        rebased.input.starting_savepoint,
        child_node.input.starting_savepoint
    );
    apply_input_event(
        controller,
        TurnContractEvent::RebaseActivation {
            previous: child_revision.activation.clone(),
            input: Box::new(rebased.input.clone()),
        },
    );
    apply_input_event(
        controller,
        TurnContractEvent::StartPreparedActivation {
            activation: rebased.input.activation.clone(),
        },
    );
    let failed_provider = InputProvider::new(CHILD_FAILED, false, true);
    let failed = run_input(controller, &rebased, failed_provider.clone()).await;
    assert!(!failed.accepted);
    assert_eq!(failed.output.content().output.kind, OutputKind::Partial);
    assert_projected_only(
        &failed_provider,
        &[REQUEST, PARENT_V2, "child-revision-guidance"],
        &[PARENT_V1, CHILD_V1, CHILD_V2, "parent-private-native-call"],
    );
    assert_eq!(failed_provider.calls.load(Ordering::SeqCst), 1);
    {
        // An errored provider run checkpoints only the complete pre-turn
        // prefix. Its input is retained by the manifest, and incurred usage
        // survives without promoting the failed speculative conversation.
        let state = controller.lock().unwrap();
        let checkpoint = state
            .memory
            .checkpoint(failed.checkpoint.as_ref().unwrap())
            .unwrap();
        assert_eq!(checkpoint.agent_id, rebased.input.conversation_id.as_str());
        assert!(checkpoint.session_messages.is_empty());
        assert_eq!(
            checkpoint.cumulative_token_usage,
            TokenUsageStats::new(30, 6)
        );
        assert!(!checkpoint.cumulative_token_usage_known);
        let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
        let activation = snapshot
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation == failed.activation)
            .unwrap();
        assert_eq!(activation.state, ActivationState::Failed);
        assert!(activation.checkpoint.is_none());
        assert!(activation.output.is_none());
    }

    let retry = next_node(&rebased);
    start_input(controller, &retry);
    let retry_provider = InputProvider::new("child-retry-final", false, false);
    let retried = run_input(controller, &retry, retry_provider.clone()).await;
    assert!(retried.accepted, "{:?}", retried.failure);
    assert_eq!(retry_provider.first_input(), failed_provider.first_input());
    assert_projected_only(
        &retry_provider,
        &[REQUEST, PARENT_V2],
        &[PARENT_V1, CHILD_V1, CHILD_V2, CHILD_FAILED],
    );
    let checkpoint = saved_input(controller, &retried, &retry_provider);
    assert_eq!(checkpoint.session_messages.len(), 2);
    assert_eq!(checkpoint.session_messages[1].content, "child-retry-final");
    assert_eq!(
        checkpoint.cumulative_token_usage,
        TokenUsageStats::new(40, 8)
    );
    assert!(!checkpoint.cumulative_token_usage_known);
    // The original native artifact is not rewritten by rebase/retry.
    let state = controller.lock().unwrap();
    assert_eq!(
        serde_json::to_value(
            state
                .memory
                .checkpoint(parent.checkpoint.as_ref().unwrap())
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(&first_parent_checkpoint).unwrap()
    );
}

#[tokio::test]
async fn stale_parent_output_is_refused_before_child_provider_dispatch() {
    let fixture = input_fixture();
    start_input(&fixture.controller, &fixture.parent);
    let parent = run_input(
        &fixture.controller,
        &fixture.parent,
        InputProvider::new(PARENT_V1, false, false),
    )
    .await;
    let mut child = fixture.child.clone();
    child.input.parents = vec![accepted_parent(&parent)];
    let mut revised_parent = next_node(&fixture.parent);
    revised_parent.input.revision_context = Some(RevisionContext {
        activation: parent.activation.clone(),
        output: parent.output.reference().clone(),
    });
    apply_input_event(
        &fixture.controller,
        TurnContractEvent::ReviseAccepted {
            previous: parent.activation.clone(),
            input: Box::new(revised_parent.input.clone()),
            invalidated_descendants: vec![],
            evidence: parent.output.reference().clone(),
        },
    );
    apply_input_event(
        &fixture.controller,
        TurnContractEvent::StartPreparedActivation {
            activation: revised_parent.input.activation.clone(),
        },
    );
    let revised = run_input(
        &fixture.controller,
        &revised_parent,
        InputProvider::new(PARENT_V2, false, false),
    )
    .await;
    assert!(revised.accepted, "{:?}", revised.failure);
    // These are real, intact earlier artifacts, now superseded by exact newer acceptance.
    let provider = InputProvider::new(CHILD_V1, false, false);
    let event = input_event(
        &fixture.controller,
        TurnContractEvent::StartActivation {
            input: Box::new(child.input.clone()),
        },
    );
    assert!(fixture.controller.append_host_event(event).is_err());
    assert!(fixture
        .controller
        .prepare_autonomous_activation(
            child.input.activation.clone(),
            input_resources(&child, provider.clone())
        )
        .is_err());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(fixture
        .controller
        .snapshot()
        .unwrap()
        .contract()
        .activations()
        .iter()
        .all(|item| item.activation.node_id != child.input.activation.node_id));
}

#[tokio::test]
async fn missing_or_corrupt_accepted_parent_checkpoint_prevents_child_provider_dispatch() {
    for corrupt in [false, true] {
        let fixture = input_fixture();
        start_input(&fixture.controller, &fixture.parent);
        let parent = run_input(
            &fixture.controller,
            &fixture.parent,
            InputProvider::new(PARENT_V1, false, false),
        )
        .await;
        let mut child = fixture.child.clone();
        child.input.parents = vec![accepted_parent(&parent)];
        start_input(&fixture.controller, &child);
        let reference = parent.checkpoint.as_ref().unwrap();
        let name = format!(
            "{:x}.checkpoint",
            Sha256::digest(reference.checkpoint_id.as_str().as_bytes())
        );
        let root = fixture
            .controller
            .lock()
            .unwrap()
            .canonical
            .path()
            .parent()
            .unwrap()
            .join("activation-state");
        let artifact = find_checkpoint_file(&root, &name).unwrap();
        if corrupt {
            let mut bytes = std::fs::read(&artifact).unwrap();
            let last = bytes.len() - 1;
            bytes[last] ^= 1;
            std::fs::write(&artifact, bytes).unwrap();
        } else {
            std::fs::remove_file(&artifact).unwrap();
        }
        let provider = InputProvider::new(CHILD_V1, false, false);
        assert!(fixture
            .controller
            .prepare_autonomous_activation(
                child.input.activation.clone(),
                input_resources(&child, provider.clone())
            )
            .is_err());
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert!(provider.requests.lock().unwrap().is_empty());
    }
}

fn find_checkpoint_file(root: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == name {
            return Some(entry.path());
        }
        if entry.file_type().unwrap().is_dir() {
            if let Some(path) = find_checkpoint_file(&entry.path(), name) {
                return Some(path);
            }
        }
    }
    None
}
