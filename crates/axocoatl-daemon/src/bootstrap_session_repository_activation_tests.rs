use super::*;
use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
use crate::session_dispatch::{
    AutonomousActivationResources, RepositoryActivationResource, SessionDispatchController,
};
use axocoatl_core::{AgentConfig, AgentId, ChatMessage, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    ProviderExecutionBounds, StreamEvent,
};
use axocoatl_memory::activation_state::ActivationStateStore;
use axocoatl_session::control_authority::{AuthorityGrant, ExecutionProfile, GrantLimits};
use axocoatl_session::execution_content::{
    ActivationEvidenceContent, ExecutionContentStore, ExecutionRequestContent,
};
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::turn_contract::*;
use axocoatl_token::TokenCounter;
use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_stream::Stream;

struct Counter;
impl TokenCounter for Counter {
    fn count_text(&self, text: &str) -> usize {
        text.len() / 4 + 1
    }
    fn count_messages(&self, messages: &[ChatMessage]) -> usize {
        messages.len() * 10
    }
    fn count_tool_definition(&self, value: &serde_json::Value) -> usize {
        self.count_text(&value.to_string())
    }
}

struct Provider {
    calls: AtomicUsize,
    requests: Mutex<Vec<Vec<ChatMessage>>>,
    /// Names of the tools each request offered the model.
    offered: Mutex<Vec<Vec<String>>>,
    operations: Vec<(&'static str, serde_json::Value)>,
}
impl Provider {
    fn new(operations: Vec<(&'static str, serde_json::Value)>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            requests: Mutex::new(vec![]),
            offered: Mutex::new(vec![]),
            operations,
        })
    }
    /// Whether the tool result of request `index` contains `text`.
    fn saw(&self, index: usize, text: &str) -> bool {
        self.requests.lock().unwrap()[index]
            .iter()
            .filter_map(ChatMessage::text_content)
            .any(|content| content.contains(text))
    }
}
#[async_trait::async_trait]
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
    fn execution_bounds(&self, _: &ChatRequest) -> Option<ProviderExecutionBounds> {
        Some(ProviderExecutionBounds {
            token_limit: 100,
            cost_microunits: 0,
            response_bytes: 65536,
        })
    }
    async fn chat(&self, _: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        unreachable!()
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<
        Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            call <= self.operations.len(),
            "fixture provider exceeded its finite response set"
        );
        self.offered
            .lock()
            .unwrap()
            .push(request.tools.iter().map(|tool| tool.name.clone()).collect());
        self.requests.lock().unwrap().push(request.messages);
        let tool = self.operations.get(call);
        let mut events = if let Some((name, arguments)) = tool {
            vec![Ok(StreamEvent::ToolCallDelta {
                index: Some(0),
                id: format!("native-{call}"),
                name: Some((*name).into()),
                args_delta: serde_json::to_string(arguments).unwrap(),
            })]
        } else {
            vec![Ok(StreamEvent::TextDelta {
                delta: "repository operation complete".into(),
            })]
        };
        events.push(Ok(StreamEvent::Usage(TokenUsageStats::new(10, 2))));
        events.push(Ok(StreamEvent::Done {
            finish_reason: if tool.is_some() {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
        }));
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

struct Run {
    registry: SessionDispatchRegistry,
    controller: SessionDispatchController,
    activation: ActivationRef,
    resource: RepositoryActivationResource,
    config: AgentConfig,
    profile: ExecutionProfile,
}
impl Run {
    fn resources(&self, provider: Arc<Provider>) -> AutonomousActivationResources {
        // Intentionally empty: repository tools must come from the retained owner,
        // never this arbitrary caller-supplied executor.
        AutonomousActivationResources {
            config: self.config.clone(),
            profile: self.profile.clone(),
            provider,
            counter: Arc::new(Counter),
            tools: Arc::new(axocoatl_tools::ToolExecutor::new()),
        }
    }
}

fn run(f: &mut Fixture, tools: &[&str], repository_recorded: bool) -> Run {
    run_with(f, tools, repository_recorded, None, &[])
}

/// A repository activation whose Agent may change only `writes`.
fn run_scoped(f: &mut Fixture, tools: &[&str], writes: &[&str]) -> Run {
    run_with(f, tools, true, Some(writes), &[])
}

/// A repository activation whose turn has these required checks, as
/// admission adds them to the graph.
fn run_checked(f: &mut Fixture, tools: &[&str], checks: &[Vec<String>]) -> Run {
    run_with(f, tools, true, None, checks)
}

fn required_check_conditions(
    content: &mut ExecutionContentStore,
    node: &TurnNodeId,
    checks: &[Vec<String>],
) -> Vec<CompletionCondition> {
    use axocoatl_session::turn_checks::{check_definitions, readiness_text, CheckGroup};
    let group = CheckGroup::required();
    let definitions = check_definitions(checks).unwrap();
    if definitions.is_empty() {
        return vec![];
    }
    let mut conditions: Vec<_> = definitions
        .into_iter()
        .enumerate()
        .map(|(index, definition)| CompletionCondition {
            condition_id: ConditionId::new(group.condition_id(index)).unwrap(),
            kind: ConditionKind::RepositoryCheck {
                definition: content
                    .retain_repository_check_definition(definition)
                    .unwrap()
                    .reference()
                    .clone(),
            },
            nodes: vec![node.clone()],
        })
        .collect();
    let criterion = content
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: readiness_text(checks),
        })
        .unwrap()
        .reference()
        .clone();
    conditions.push(CompletionCondition {
        condition_id: ConditionId::new(group.ready_id()).unwrap(),
        kind: ConditionKind::Review { criterion },
        nodes: vec![node.clone()],
    });
    conditions
}

fn run_with(
    f: &mut Fixture,
    tools: &[&str],
    repository_recorded: bool,
    writes: Option<&[&str]>,
    checks: &[Vec<String>],
) -> Run {
    let mut canonical = f._canonical.take().unwrap();
    let mut content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let memory = ActivationStateStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ActivationState)
            .unwrap(),
    )
    .unwrap();
    let config = AgentConfig {
        id: AgentId::new("conversation"),
        name: "Repository actor".into(),
        provider: "controlled".into(),
        model: "controlled-model".into(),
        tools: tools.iter().map(|tool| (*tool).into()).collect(),
        writes: writes.map(|writes| writes.iter().map(|path| (*path).into()).collect()),
        ..Default::default()
    };
    let profile = ExecutionProfile {
        definition: "repository-definition".into(),
        provider: config.provider.clone(),
        model: config.model.clone(),
        isolation: "in-process".into(),
        tools: config.tools.clone(),
        write_scope: config.writes.clone(),
    };
    let activation = ActivationRef {
        session_id: canonical.owner().session_id.clone(),
        turn_id: LogicalTurnId::new("repository-turn").unwrap(),
        execution_epoch_id: ExecutionEpochId::new("repository-epoch").unwrap(),
        node_id: TurnNodeId::new("repository-node").unwrap(),
        generation: 1,
        activation_id: ActivationId::new("repository-activation").unwrap(),
    };
    let definition_id = AgentDefinitionId::new(profile.definition.clone()).unwrap();
    let definition = content
        .retain_activation_evidence(ActivationEvidenceContent::Definition {
            definition_id: definition_id.clone(),
            revision: 1,
            profile: profile.clone(),
            configuration: serde_json::to_string(&config).unwrap(),
        })
        .unwrap();
    let definition = DefinitionSnapshotRef {
        definition_id,
        snapshot: definition.reference().clone(),
    };
    let request = content
        .retain_request(ExecutionRequestContent {
            turn_id: activation.turn_id.clone(),
            recorded_at_unix_ms: 1,
            display_input: "Use the retained checkout".into(),
            effective_input: "Use the retained checkout".into(),
            context: vec![],
            target_definition: None,
            model: None,
        })
        .unwrap();
    let conditions = required_check_conditions(&mut content, &activation.node_id, checks);
    canonical
        .begin_with_request(
            TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("repository-begin").unwrap(),
                expected_revision: 0,
                session_id: activation.session_id.clone(),
                turn_id: activation.turn_id.clone(),
                event: TurnContractEvent::Begin {
                    epoch_id: activation.execution_epoch_id.clone(),
                    predecessor: None,
                    graph: TurnGraphSnapshot {
                        snapshot_id: GraphSnapshotId::new("repository-graph").unwrap(),
                        revision: 1,
                        nodes: vec![GraphNode {
                            node_id: activation.node_id.clone(),
                            slot_id: SessionTeamSlotId::new("repository-slot").unwrap(),
                            definition: definition.clone(),
                            conversation_id: NodeConversationId::new("conversation").unwrap(),
                            starting_savepoint: ConversationSavepoint::Empty,
                            required: true,
                        }],
                        dependencies: vec![],
                        conditions,
                    },
                },
            },
            &request,
        )
        .unwrap();
    let controller = SessionDispatchController::open_retained(
        crate::session_dispatch::RetainedSessionStores {
            canonical,
            content,
            memory,
        },
        activation.turn_id.clone(),
    )
    .unwrap_or_else(|failure| panic!("{}", failure.error));
    let registry = SessionDispatchRegistry::default();
    let reference = registry
        .register(controller.clone(), f.owner.clone())
        .unwrap();
    let resource = controller
        .repository_activation_resource(&reference)
        .unwrap();
    let approval = controller
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: "Fixture host permits its finite local tool calls".into(),
        })
        .unwrap();
    let limits = GrantLimits {
        activations: 2,
        invocations: 12,
        tokens: 1000,
        cost_microunits: 0,
    };
    let policy = AuthorityGrant {
        id: "repository-grant".into(),
        revision: 1,
        issuer_evidence: approval,
        holder: activation.node_id.clone(),
        descendants: vec![],
        allow_stop_descendants: false,
        delegation: None,
        conditions: vec![],
        profiles: vec![profile.clone()],
        limits: limits.clone(),
        expires_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000,
    };
    let grant = controller
        .retain_activation_evidence(ActivationEvidenceContent::Grant {
            policy: policy.clone(),
        })
        .unwrap();
    let budget = controller
        .retain_activation_evidence(ActivationEvidenceContent::Budget { limits })
        .unwrap();
    controller.install_grant(policy).unwrap();
    controller
        .append_host_event(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("repository-start").unwrap(),
            expected_revision: 1,
            session_id: activation.session_id.clone(),
            turn_id: activation.turn_id.clone(),
            event: TurnContractEvent::StartActivation {
                input: Box::new(ActivationInputManifest {
                    manifest_id: InputManifestId::new("repository-input").unwrap(),
                    activation: activation.clone(),
                    definition,
                    conversation_id: NodeConversationId::new("conversation").unwrap(),
                    starting_savepoint: ConversationSavepoint::Empty,
                    parents: vec![],
                    guidance: vec![request.reference().clone()],
                    attachments: vec![],
                    repository: if repository_recorded {
                        RepositoryInput::Recorded {
                            snapshot: reference,
                        }
                    } else {
                        RepositoryInput::Unavailable
                    },
                    budget,
                    grant: Some(GrantSnapshotRef {
                        grant_id: GrantId::new("repository-grant").unwrap(),
                        revision: 1,
                        evidence: grant,
                    }),
                    revision_context: None,
                }),
            },
        })
        .unwrap();
    Run {
        registry,
        controller,
        activation,
        resource,
        config,
        profile,
    }
}

#[tokio::test]
async fn repository_activation_requires_exact_registered_owner_and_preserves_plain_text() {
    let mut f = fixture().await;
    let r = run(&mut f, &[], false);
    let provider = Provider::new(vec![]);
    let before = historical_read_tree(f._data.path());
    assert!(r
        .controller
        .prepare_repository_activation(
            r.activation.clone(),
            r.resources(provider.clone()),
            r.resource.clone()
        )
        .is_err());
    assert_eq!(historical_read_tree(f._data.path()), before);
    let settled = r
        .controller
        .prepare_autonomous_activation(r.activation.clone(), r.resources(provider.clone()))
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(settled.accepted);
    let messages = provider.requests.lock().unwrap();
    assert!(messages[0]
        .iter()
        .any(|message| message.text_content() == Some("Use the retained checkout")));
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn repository_activation_refuses_missing_foreign_retired_and_unsupported_resources_before_effects(
) {
    for case in ["missing", "foreign", "retired", "background", "pty"] {
        let mut f = fixture().await;
        let tools = match case {
            "background" => vec!["bash_background"],
            "pty" => vec!["spawn_terminal"],
            _ => vec!["write_file"],
        };
        let r = run(&mut f, &tools, true);
        let provider = Provider::new(vec![]);
        let mut other = fixture().await;
        let foreign = run(&mut other, &["write_file"], true);
        if case == "retired" {
            f.owner.request_supervised_stop().unwrap();
        }
        let before = historical_read_tree(f._data.path());
        let result = if case == "missing" {
            r.controller
                .prepare_autonomous_activation(r.activation.clone(), r.resources(provider.clone()))
        } else {
            r.controller.prepare_repository_activation(
                r.activation.clone(),
                r.resources(provider.clone()),
                if case == "foreign" {
                    foreign.resource
                } else {
                    r.resource.clone()
                },
            )
        };
        assert!(result.is_err(), "{case}");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0, "{case}");
        assert_eq!(historical_read_tree(f._data.path()), before, "{case}");
        assert!(f.operation.try_lock().is_err());
    }
}

#[tokio::test]
async fn repository_metadata_reaches_actual_actor_and_stale_runtime_refuses_tools() {
    let mut f = fixture().await;
    let r = run(&mut f, &["write_file"], true);
    let provider = Provider::new(vec![(
        "write_file",
        serde_json::json!({"path":"no-effect", "content":"never written"}),
    )]);
    let prepared = r
        .controller
        .prepare_repository_activation(
            r.activation.clone(),
            r.resources(provider.clone()),
            r.resource.clone(),
        )
        .unwrap();
    // Actual retained runtime registry changed after binding, before physical execution.
    f.owner.inner.sandboxes.lock().await.clear();
    let settled = prepared.run().await.unwrap();
    assert!(!f._workspace.path().join("no-effect").exists());
    assert!(f.owner.execution_is_idle().unwrap());
    let requests = provider.requests.lock().unwrap();
    let input = requests[0]
        .iter()
        .filter_map(ChatMessage::text_content)
        .find(|text| text.contains("descriptive input only"))
        .unwrap();
    assert!(input.contains(f.owner.execution_identity()));
    assert!(input.contains(r.resource.reference().as_str()));
    assert_eq!(settled.activation, r.activation);
    assert!(r
        .registry
        .control_plane(
            f.owner.metadata().session_id.as_str(),
            r.activation.turn_id.as_str()
        )
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn closed_registration_cannot_execute_a_prepared_repository_activation() {
    let mut f = fixture().await;
    let r = run(&mut f, &["write_file"], true);
    let provider = Provider::new(vec![(
        "write_file",
        serde_json::json!({"path":"closed-effect", "content":"no"}),
    )]);
    let prepared = r
        .controller
        .prepare_repository_activation(
            r.activation.clone(),
            r.resources(provider.clone()),
            r.resource.clone(),
        )
        .unwrap();
    r.registry.close_all_admission().unwrap();
    assert!(prepared.run().await.is_err());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(!f._workspace.path().join("closed-effect").exists());
}

#[tokio::test]
async fn scoped_write_file_is_refused_before_any_effect() {
    let mut f = fixture().await;
    let r = run_scoped(&mut f, &["write_file", "edit_file"], &["lib/"]);
    let provider = Provider::new(vec![
        (
            "write_file",
            serde_json::json!({"path":"config/x", "content":"outside"}),
        ),
        (
            "edit_file",
            serde_json::json!({"path":"lib/../config/y", "old":"a", "new":"b"}),
        ),
    ]);
    let settled = r
        .controller
        .prepare_repository_activation(
            r.activation.clone(),
            r.resources(provider.clone()),
            r.resource.clone(),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    assert!(provider.saw(
        1,
        "config/x is outside the paths this Agent may change (lib/). Leave it unchanged and \
         describe the needed change in your answer."
    ));
    assert!(provider.saw(2, "lib/../config/y uses '..'"));
    // Refused by the scope, never by the supervisor that would have run it.
    assert!(!provider.saw(1, "supervision") && !provider.saw(2, "supervision"));
    assert!(!f._workspace.path().join("config").exists());
    assert!(f.owner.execution_is_idle().unwrap());
    let snapshot = r.controller.snapshot().unwrap();
    // Both refused calls, and the host's Before and After captures.
    assert_eq!(snapshot.contract().invocations().len(), 4);
    assert!(snapshot
        .contract()
        .invocations()
        .iter()
        .all(|invocation| invocation.evidence.disposition() == EffectDisposition::OutcomeRecorded));
    // Even without a shell its captures decide, and this fixture has no
    // supervisor to take them: nothing establishes that only lib/ changed.
    assert!(!settled.accepted);
    let failure = settled.failure.unwrap();
    assert!(
        failure.starts_with("its repository captures cannot establish"),
        "{failure}"
    );
}

#[tokio::test]
async fn read_only_helper_is_not_offered_write_tools() {
    let tools = ["read_file", "write_file", "edit_file", "grep"];
    let mut f = fixture().await;
    let r = run_scoped(&mut f, &tools, &[]);
    let provider = Provider::new(vec![]);
    let settled = r
        .controller
        .prepare_repository_activation(
            r.activation.clone(),
            r.resources(provider.clone()),
            r.resource.clone(),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    let offered = provider.offered.lock().unwrap()[0].clone();
    assert!(
        offered.iter().any(|tool| tool == "read_file"),
        "{offered:?}"
    );
    assert!(offered.iter().any(|tool| tool == "grep"), "{offered:?}");
    assert!(
        !offered
            .iter()
            .any(|tool| tool == "write_file" || tool == "edit_file"),
        "{offered:?}"
    );
    // The stored definition keeps its tools; only what is offered narrows.
    assert_eq!(r.config.tools, tools);
    assert_eq!(r.profile.write_scope, Some(vec![]));

    // The same definition with no scope is offered every tool.
    let mut f = fixture().await;
    let r = run(&mut f, &tools, true);
    let provider = Provider::new(vec![]);
    r.controller
        .prepare_repository_activation(
            r.activation.clone(),
            r.resources(provider.clone()),
            r.resource.clone(),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    let offered = provider.offered.lock().unwrap()[0].clone();
    assert!(
        offered.iter().any(|tool| tool == "write_file"),
        "{offered:?}"
    );
    assert!(
        offered.iter().any(|tool| tool == "edit_file"),
        "{offered:?}"
    );
}

#[tokio::test]
async fn write_scope_lookup_fails_closed() {
    let mut f = fixture().await;
    let r = run_scoped(&mut f, &["bash", "write_file"], &["lib/"]);
    // Not yet registered with authority: its scope cannot be read, so its
    // changes cannot be judged and the activation could not be accepted.
    let violation = r
        .controller
        .write_scope_violation(&r.activation)
        .unwrap()
        .unwrap();
    assert!(
        violation.starts_with("its admitted write scope cannot be read"),
        "{violation}"
    );
    let prepared = r
        .controller
        .prepare_repository_activation(
            r.activation.clone(),
            r.resources(Provider::new(vec![])),
            r.resource.clone(),
        )
        .unwrap();
    // Once registered, the durable record is read. With a shell, captures
    // that were never taken cannot rule out an out-of-scope change.
    let violation = r
        .controller
        .write_scope_violation(&r.activation)
        .unwrap()
        .unwrap();
    assert!(
        violation.starts_with("its repository captures cannot establish"),
        "{violation}"
    );
    assert!(violation.contains("(lib/)"), "{violation}");
    drop(prepared);

    // An unscoped activation is never judged, whatever its captures.
    let mut f = fixture().await;
    let r = run(&mut f, &["bash"], true);
    let _prepared = r
        .controller
        .prepare_repository_activation(
            r.activation.clone(),
            r.resources(Provider::new(vec![])),
            r.resource.clone(),
        )
        .unwrap();
    assert_eq!(
        r.controller.write_scope_violation(&r.activation).unwrap(),
        None
    );
}

async fn actual_sandbox(f: &mut Fixture) -> Arc<axocoatl_isolation::SessionSandbox> {
    use axocoatl_isolation::{SandboxNetwork, SandboxPolicy, SessionSandbox};
    use sha2::{Digest, Sha256};
    let image =
        std::env::var("AXO_SUPERVISOR_TEST_IMAGE").expect("set the explicit prepared tools image");
    let policy = SandboxPolicy {
        allow_untrusted_image: true,
        network: SandboxNetwork::None,
        runtime_authority: Some(format!(
            "{:x}",
            Sha256::digest(f.owner.metadata().session_id.as_bytes())
        )),
        supervisor_installation: Some(
            f.owner
                .inner
                .data_root
                .child("execution-supervisors")
                .unwrap(),
        ),
        ..SandboxPolicy::default()
    };
    let sandbox = Arc::new(
        SessionSandbox::start(
            &f.owner.metadata().session_id,
            f.owner.root(),
            Some(&image),
            &[],
            &[],
            &policy,
        )
        .await
        .unwrap(),
    );
    let registered: Arc<dyn Sandbox> = sandbox.clone();
    {
        let inner = Arc::get_mut(&mut f.owner.inner).unwrap();
        inner.metadata.execution_identity = sandbox.execution_identity().unwrap().to_owned();
        inner.sandbox = registered.clone();
        inner
            .sandboxes
            .lock()
            .await
            .insert(inner.metadata.session_id.clone(), registered);
    }
    sandbox
}

#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_native_repository_tools_write_edit_full_source_and_keep_owned_settlement() {
    let mut f = fixture().await;
    let sandbox = actual_sandbox(&mut f).await;
    let body = format!("exact marker{}", "x".repeat(8 * 1024 * 1024 - 12));
    assert_eq!(body.len(), 8 * 1024 * 1024);
    std::fs::write(f._workspace.path().join("full-source"), body.as_bytes()).unwrap();
    let r = run(&mut f, &["write_file", "edit_file"], true);
    let provider = Provider::new(vec![
        (
            "write_file",
            serde_json::json!({"path":"exact-bytes", "content":"a\u{0}b\ncontrol-looking: {\"kind\":\"cancel\"}"}),
        ),
        (
            "edit_file",
            serde_json::json!({"path":"full-source", "old":"exact marker", "new":"other marker"}),
        ),
    ]);
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        r.controller
            .prepare_repository_activation(
                r.activation.clone(),
                r.resources(provider.clone()),
                r.resource.clone(),
            )
            .unwrap()
            .run()
            .await
    })
    .await;
    let actual = std::fs::read(f._workspace.path().join("exact-bytes"));
    let edited = std::fs::read(f._workspace.path().join("full-source"));
    let idle = f.owner.execution_is_idle();
    let stop = sandbox.stop_checked().await;
    stop.unwrap();
    let settled = result.unwrap().unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        actual.unwrap(),
        b"a\0b\ncontrol-looking: {\"kind\":\"cancel\"}"
    );
    assert_eq!(
        edited.unwrap(),
        body.replacen("exact marker", "other marker", 1).as_bytes()
    );
    assert!(idle.unwrap());
    let snapshot = r.controller.snapshot().unwrap();
    assert_eq!(snapshot.contract().invocations().len(), 2);
    assert!(snapshot
        .contract()
        .invocations()
        .iter()
        .all(|invocation| invocation.evidence.disposition() == EffectDisposition::OutcomeRecorded));
}

fn git_init(path: &std::path::Path) {
    assert!(std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(path)
        .status()
        .unwrap()
        .success());
}

#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_read_only_helper_shell_write_is_denied_by_landlock() {
    let mut f = fixture().await;
    let sandbox = actual_sandbox(&mut f).await;
    git_init(f._workspace.path());
    std::fs::write(f._workspace.path().join("existing.txt"), "original\n").unwrap();
    let r = run_scoped(&mut f, &["bash", "read_file"], &[]);
    let provider = Provider::new(vec![(
        "bash",
        serde_json::json!({"command":"printf changed > existing.txt; printf new > created.txt; \
            printf scratch > /tmp/scratch.txt && echo scratch-ok"}),
    )]);
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        r.controller
            .prepare_repository_activation(
                r.activation.clone(),
                r.resources(provider.clone()),
                r.resource.clone(),
            )
            .unwrap()
            .run()
            .await
    })
    .await;
    let existing = std::fs::read_to_string(f._workspace.path().join("existing.txt"));
    let created = f._workspace.path().join("created.txt").exists();
    let idle = f.owner.execution_is_idle();
    sandbox.stop_checked().await.unwrap();
    let settled = result.unwrap().unwrap();
    assert_eq!(existing.unwrap(), "original\n");
    assert!(!created);
    assert!(idle.unwrap());
    // The shell ran and could still use its scratch space.
    assert!(provider.saw(1, "scratch-ok"));
    // Both host captures ran despite the restriction, so the unchanged tree
    // is established and the helper's answer is accepted.
    assert!(settled.accepted, "{:?}", settled.failure);
    let snapshot = r.controller.snapshot().unwrap();
    assert_eq!(snapshot.contract().invocations().len(), 3);
}

#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_shell_change_outside_scope_fails_the_activation() {
    let mut f = fixture().await;
    let sandbox = actual_sandbox(&mut f).await;
    git_init(f._workspace.path());
    std::fs::create_dir_all(f._workspace.path().join("lib")).unwrap();
    std::fs::write(f._workspace.path().join("lib/y"), "original\n").unwrap();
    let r = run_scoped(&mut f, &["bash"], &["lib/"]);
    let provider = Provider::new(vec![(
        "bash",
        serde_json::json!({"command":"mkdir -p config && printf x > config/x && printf y > lib/y"}),
    )]);
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        r.controller
            .prepare_repository_activation(
                r.activation.clone(),
                r.resources(provider.clone()),
                r.resource.clone(),
            )
            .unwrap()
            .run()
            .await
    })
    .await;
    let outside = std::fs::read_to_string(f._workspace.path().join("config/x"));
    let inside = std::fs::read_to_string(f._workspace.path().join("lib/y"));
    let idle = f.owner.execution_is_idle();
    sandbox.stop_checked().await.unwrap();
    let settled = result.unwrap().unwrap();
    // A scoped writer's shell is not restricted; its captures judge it.
    assert_eq!(outside.unwrap(), "x");
    assert_eq!(inside.unwrap(), "y");
    assert!(idle.unwrap());
    assert!(!settled.accepted);
    let failure = settled.failure.unwrap();
    assert!(failure.starts_with("it changed config/x"), "{failure}");
    assert!(failure.contains("(lib/)"), "{failure}");
}

/// A read-only helper's shell writes configuration only to a scratch home of
/// its own: the Session's shared home stays unwritable, so nothing the helper
/// writes there reaches the host's captures or any later process.
#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_read_only_helper_shell_cannot_write_the_shared_home() {
    let mut f = fixture().await;
    let sandbox = actual_sandbox(&mut f).await;
    git_init(f._workspace.path());
    std::fs::write(f._workspace.path().join("existing.txt"), "original\n").unwrap();
    let r = run_scoped(&mut f, &["bash", "read_file"], &[]);
    let provider = Provider::new(vec![(
        "bash",
        serde_json::json!({"command":"case $HOME in /tmp/axocoatl-home.*) echo scratch-home ;; esac; \
            printf '[core]\\n\\texcludesFile = /tmp/hide\\n' > \"$HOME/.gitconfig\" \
            && echo scratch-written; \
            for home in /root /home/*; do \
              printf x > \"$home/.axocoatl-helper-probe\" 2>/dev/null && echo \"shared-written $home\"; \
            done; true"}),
    )]);
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        r.controller
            .prepare_repository_activation(
                r.activation.clone(),
                r.resources(provider.clone()),
                r.resource.clone(),
            )
            .unwrap()
            .run()
            .await
    })
    .await;
    let idle = f.owner.execution_is_idle();
    sandbox.stop_checked().await.unwrap();
    let settled = result.unwrap().unwrap();
    assert!(idle.unwrap());
    assert!(provider.saw(1, "scratch-home"));
    assert!(provider.saw(1, "scratch-written"));
    assert!(!provider.saw(1, "shared-written"));
    assert!(settled.accepted, "{:?}", settled.failure);
}

/// Configuration a scoped writer's shell writes in the shared home can
/// neither hide its out-of-scope file from the host's After capture nor make
/// that capture run a program.
#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_home_configuration_cannot_hide_a_change_or_run_in_the_capture() {
    let mut f = fixture().await;
    let sandbox = actual_sandbox(&mut f).await;
    git_init(f._workspace.path());
    std::fs::create_dir_all(f._workspace.path().join("lib")).unwrap();
    std::fs::write(f._workspace.path().join("lib/y"), "original\n").unwrap();
    let r = run_scoped(&mut f, &["bash"], &["lib/"]);
    // Each configured program would leave a marker beside the workspace.
    let command = "root=$(pwd -P); mark=\"$root/lib/ran\"; \
        printf '#!/bin/sh\\ntouch %s\\ncat\\n' \"$mark\" > /tmp/axo-program && chmod +x /tmp/axo-program; \
        printf 'config/\\n' > /tmp/axo-hide; \
        mkdir -p \"$HOME/.config/git\"; \
        printf '[core]\\n\\texcludesFile = /tmp/axo-hide\\n\\tfsmonitor = /tmp/axo-program\\n[diff]\\n\\texternal = /tmp/axo-program\\n[filter \"x\"]\\n\\tclean = /tmp/axo-program\\n' > \"$HOME/.gitconfig\"; \
        printf '* filter=x\\n' > \"$HOME/.config/git/attributes\"; \
        mkdir -p config && printf secret > config/secret.txt && printf y > lib/y && echo planted";
    let provider = Provider::new(vec![("bash", serde_json::json!({ "command": command }))]);
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        r.controller
            .prepare_repository_activation(
                r.activation.clone(),
                r.resources(provider.clone()),
                r.resource.clone(),
            )
            .unwrap()
            .run()
            .await
    })
    .await;
    let ran = f._workspace.path().join("lib/ran").exists();
    let idle = f.owner.execution_is_idle();
    sandbox.stop_checked().await.unwrap();
    let settled = result.unwrap().unwrap();
    assert!(idle.unwrap());
    assert!(provider.saw(1, "planted"));
    assert!(!ran, "the host capture ran a program the Agent configured");
    assert!(!settled.accepted);
    // Where the home directory is inside the repository, the configuration
    // files are named too.
    let failure = settled.failure.unwrap();
    assert!(failure.starts_with("it changed "), "{failure}");
    assert!(failure.contains("config/secret.txt"), "{failure}");
}

/// A committed repository of 300 files, far beyond an 8 KiB manifest prefix.
fn sizeable_repository(root: &std::path::Path) {
    git_init(root);
    std::fs::create_dir_all(root.join("lib")).unwrap();
    std::fs::create_dir_all(root.join("config")).unwrap();
    for index in 0..300 {
        std::fs::write(
            root.join(format!("lib/file-{index}.js")),
            format!("{index}\n"),
        )
        .unwrap();
    }
    std::fs::write(root.join("config/prod.js"), "prod\n").unwrap();
    std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
    for args in [
        &["add", "-A"][..],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
    ] {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap()
            .success());
    }
}

/// Runs one shell command as an Agent limited to `writes` in an actual
/// sandbox and settles the activation.
async fn settle_scoped_shell(
    f: &mut Fixture,
    writes: &[&str],
    command: &str,
) -> (crate::session_dispatch::SettledActivation, Arc<Provider>) {
    let sandbox = actual_sandbox(f).await;
    let r = run_scoped(f, &["bash"], writes);
    let provider = Provider::new(vec![("bash", serde_json::json!({ "command": command }))]);
    let result = tokio::time::timeout(Duration::from_secs(180), async {
        r.controller
            .prepare_repository_activation(
                r.activation.clone(),
                r.resources(provider.clone()),
                r.resource.clone(),
            )
            .unwrap()
            .run()
            .await
    })
    .await;
    let idle = f.owner.execution_is_idle();
    sandbox.stop_checked().await.unwrap();
    assert!(idle.unwrap());
    (result.unwrap().unwrap(), provider)
}

/// In a repository far larger than an 8 KiB manifest, a scoped writer's
/// shell hides an edit behind an index flag and a new file behind its own
/// ignore file, and plants a Git hook. The complete comparison names each.
#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_scoped_shell_cannot_blind_the_complete_judgement() {
    let mut f = fixture().await;
    sizeable_repository(f._workspace.path());
    let command = "git update-index --skip-worktree config/prod.js && printf x > config/prod.js \
        && printf '*\\n' > config/.gitignore && printf e > config/evil.js \
        && printf '#!/bin/sh\\n' > .git/hooks/pre-commit && printf y > lib/file-1.js \
        && git diff --quiet HEAD -- config && git status --porcelain -- config | wc -l";
    let (settled, provider) = settle_scoped_shell(&mut f, &["lib/"], command).await;
    // Git itself no longer shows either change.
    assert!(
        provider.saw(1, "\"stdout\":\"0\\n\""),
        "{:?}",
        provider.requests.lock().unwrap()[1]
    );
    assert!(!settled.accepted);
    let failure = settled.failure.unwrap();
    assert!(
        failure.starts_with(
            "it changed .git/hooks/pre-commit, config/.gitignore, config/prod.js outside the \
             paths this Agent may change (lib/)"
        ),
        "{failure}"
    );
}

/// The kept Before manifest is inside the sandbox, where the Agent's shell
/// can reach it. Removing or rewriting it fails the activation closed.
#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_tampering_with_the_kept_manifest_fails_closed() {
    for tamper in [
        "for kept in /tmp/axocoatl-baseline.*; do printf x >> \"$kept/manifest\"; done",
        "rm -rf /tmp/axocoatl-baseline.*",
    ] {
        let mut f = fixture().await;
        sizeable_repository(f._workspace.path());
        let command = format!("{tamper}; printf x > config/prod.js; echo tampered");
        let (settled, provider) = settle_scoped_shell(&mut f, &["lib/"], &command).await;
        assert!(provider.saw(1, "tampered"));
        assert!(!settled.accepted);
        let failure = settled.failure.unwrap();
        assert!(
            failure.starts_with("its repository captures cannot establish"),
            "{tamper}: {failure}"
        );
    }
}

/// Ordinary work inside the scope, staged with Git, is accepted.
#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_scoped_shell_work_inside_its_paths_is_accepted() {
    let mut f = fixture().await;
    sizeable_repository(f._workspace.path());
    let command = "printf y > lib/file-1.js && printf n > lib/new.js && git add lib \
        && mkdir -p build && printf o > build/out.js && git status --short | wc -l";
    let (settled, _) = settle_scoped_shell(&mut f, &["lib/"], command).await;
    assert!(settled.accepted, "{:?}", settled.failure);
}

/// A file-tool-only writer limited to lib/ writes a file there that an
/// earlier shell hard-linked to config/x. The write passes the path check,
/// but the host's captures see config/x change and fail the activation.
#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_file_tool_writer_through_a_hard_link_is_judged() {
    let mut f = fixture().await;
    let sandbox = actual_sandbox(&mut f).await;
    let root = f._workspace.path().to_owned();
    git_init(&root);
    std::fs::create_dir_all(root.join("lib")).unwrap();
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::fs::write(root.join("config/x"), "original\n").unwrap();
    std::fs::hard_link(root.join("config/x"), root.join("lib/h")).unwrap();
    let r = run_scoped(&mut f, &["write_file", "read_file"], &["lib/"]);
    let provider = Provider::new(vec![(
        "write_file",
        serde_json::json!({"path":"lib/h", "content":"changed through the link\n"}),
    )]);
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        r.controller
            .prepare_repository_activation(
                r.activation.clone(),
                r.resources(provider.clone()),
                r.resource.clone(),
            )
            .unwrap()
            .run()
            .await
    })
    .await;
    let outside = std::fs::read_to_string(root.join("config/x"));
    let idle = f.owner.execution_is_idle();
    sandbox.stop_checked().await.unwrap();
    let settled = result.unwrap().unwrap();
    assert!(idle.unwrap());
    assert_eq!(outside.unwrap(), "changed through the link\n");
    assert!(!settled.accepted);
    let failure = settled.failure.unwrap();
    assert!(failure.starts_with("it changed config/x"), "{failure}");
    assert!(failure.contains("(lib/)"), "{failure}");
    // The write, and the host's Before and After captures.
    let snapshot = r.controller.snapshot().unwrap();
    assert_eq!(snapshot.contract().invocations().len(), 3);
}

/// The host's capture port runs only in the host's own observation groups;
/// an Agent's tool call naming it is refused before anything is recorded.
#[tokio::test]
async fn an_agent_cannot_call_the_host_capture_port() {
    use axocoatl_actor::ToolInvocationRequest;
    use axocoatl_llm::ToolCall;
    let mut f = fixture().await;
    let r = run_scoped(&mut f, &["write_file"], &["lib/"]);
    let prepared = r
        .controller
        .prepare_repository_activation(
            r.activation.clone(),
            r.resources(Provider::new(vec![])),
            r.resource.clone(),
        )
        .unwrap();
    let request = ToolInvocationRequest {
        actor_id: "conversation".into(),
        provider_id: "controlled".into(),
        model_id: "controlled-model".into(),
        provider_response_group: 1,
        provider_call_index: 0,
        provider_call_count: 1,
        tool_call: ToolCall {
            id: "agent-capture".into(),
            name: axocoatl_session::control_authority::REPOSITORY_CAPTURE_PORT.into(),
            arguments: serde_json::json!({
                "command": axocoatl_session::execution_content::REPOSITORY_SNAPSHOT_COMMAND
            }),
            provider_metadata: Default::default(),
        },
    };
    let refused = prepared
        .execution_boundary_for_test()
        .admit(&request)
        .await
        .err()
        .unwrap();
    assert!(refused.contains("belongs to the host"), "{refused}");
    assert!(r
        .controller
        .snapshot()
        .unwrap()
        .contract()
        .invocations()
        .is_empty());
}

#[tokio::test]
async fn exact_invocation_executor_cannot_change_arguments_replay_or_outlive_stop() {
    use axocoatl_actor::{ToolInvocationOutcome, ToolInvocationRequest};
    use axocoatl_llm::ToolCall;
    let mut f = fixture().await;
    let r = run(&mut f, &["write_file"], true);
    let prepared = r
        .controller
        .prepare_repository_activation(
            r.activation.clone(),
            r.resources(Provider::new(vec![])),
            r.resource.clone(),
        )
        .unwrap();
    let arguments = serde_json::json!({"path":"exact", "content":"retained"});
    let request = ToolInvocationRequest {
        actor_id: "conversation".into(),
        provider_id: "controlled".into(),
        model_id: "controlled-model".into(),
        provider_response_group: 1,
        provider_call_index: 0,
        provider_call_count: 1,
        tool_call: ToolCall {
            id: "exact-native".into(),
            name: "write_file".into(),
            arguments: arguments.clone(),
            provider_metadata: Default::default(),
        },
    };
    let admitted = prepared
        .execution_boundary_for_test()
        .admit(&request)
        .await
        .unwrap();
    let executor = admitted.tool_executor().unwrap();
    let changed = executor
        .execute(
            "write_file",
            serde_json::json!({"path":"other", "content":"changed"}),
        )
        .await
        .unwrap_err();
    assert!(changed.to_string().contains("differ"));
    let unchanged = executor
        .execute("write_file", arguments.clone())
        .await
        .unwrap_err();
    assert!(unchanged.to_string().contains("supervision"), "{unchanged}");
    assert!(executor
        .execute("write_file", arguments.clone())
        .await
        .unwrap_err()
        .to_string()
        .contains("already consumed"));
    admitted
        .record_outcome(&ToolInvocationOutcome::Returned(Err(unchanged.to_string())))
        .await
        .unwrap();
    r.controller.stop_activation(&r.activation).unwrap();
    assert!(executor.execute("write_file", arguments).await.is_err());
    assert!(f.owner.execution_is_idle().unwrap());
    assert!(!f._workspace.path().join("exact").exists());
    assert!(!f._workspace.path().join("other").exists());
    assert_eq!(
        r.controller
            .snapshot()
            .unwrap()
            .contract()
            .invocations()
            .len(),
        1
    );
}

#[tokio::test]
async fn cancellation_while_waiting_for_session_start_never_dispatches_or_releases_the_workspace() {
    let mut f = fixture().await;
    let r = run(&mut f, &["write_file"], true);
    let provider = Provider::new(vec![(
        "write_file",
        serde_json::json!({"path":"cancelled", "content":"no effect"}),
    )]);
    let start = f.owner.inner.start.clone().lock_owned().await;
    let prepared = r
        .controller
        .prepare_repository_activation(
            r.activation.clone(),
            r.resources(provider.clone()),
            r.resource.clone(),
        )
        .unwrap();
    let task = tokio::spawn(prepared.run());
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.owner.inner.state.lock().unwrap().active.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    r.controller.stop_activation(&r.activation).unwrap();
    let settled = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!settled.accepted);
    assert!(f.owner.execution_is_idle().unwrap());
    assert!(f.operation.try_lock().is_err());
    assert!(!f._workspace.path().join("cancelled").exists());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    drop(start);
}

#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_repository_stop_and_lost_supervisor_keep_distinct_ownership_truth() {
    for lose_supervisor in [false, true] {
        let mut f = fixture().await;
        let sandbox = actual_sandbox(&mut f).await;
        let r = run(&mut f, &["bash"], true);
        // The second fixture deliberately destroys only its own helper after
        // recording a real effect. Its surviving command then needs explicit
        // fixture cleanup; an absent acknowledgement cannot become settlement.
        let command = if lose_supervisor {
            "printf started > process-started; kill -KILL \"$PPID\"; sleep 60"
        } else {
            "trap '' TERM; printf started > process-started; while :; do sleep 1; done"
        };
        let provider = Provider::new(vec![("bash", serde_json::json!({"command":command}))]);
        let prepared = r
            .controller
            .prepare_repository_activation(
                r.activation.clone(),
                r.resources(provider.clone()),
                r.resource.clone(),
            )
            .unwrap();
        let task = tokio::spawn(prepared.run());
        let observed = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if std::fs::read(f._workspace.path().join("process-started"))
                    .is_ok_and(|bytes| bytes == b"started")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        if !lose_supervisor && observed.is_ok() {
            r.controller.stop_activation(&r.activation).unwrap();
            // Drop the external actor waiter after Stop. The independently
            // owned invocation and process tasks must still reach settlement.
            task.abort();
        }
        let outcome = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if lose_supervisor {
                    if f.owner.unresolved_execution().unwrap().is_some() {
                        break;
                    }
                } else if f.owner.execution_is_idle().unwrap() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        let unknown = f.owner.unresolved_execution();
        let idle = f.owner.execution_is_idle();
        let workspace_locked = f.operation.try_lock().is_err();
        let snapshot = r.controller.snapshot();
        let cleanup = sandbox.stop_checked().await;
        cleanup.unwrap();
        observed.unwrap();
        outcome.unwrap();
        assert!(workspace_locked);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        if lose_supervisor {
            assert!(unknown.unwrap().is_some());
            assert!(!idle.unwrap());
            assert!(snapshot
                .unwrap()
                .contract()
                .invocations()
                .iter()
                .any(|invocation| invocation.evidence.disposition()
                    == EffectDisposition::OutcomeUnknown));
        } else {
            assert!(unknown.unwrap().is_none());
            assert!(idle.unwrap());
        }
        // The owned scope can still retain late diagnostic evidence. It cannot
        // resurrect admission after either Stop or supervisor loss.
        if lose_supervisor {
            assert!(r
                .controller
                .repository_activation_resource(r.resource.reference())
                .is_err());
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_repository_edit_refuses_invalid_utf8_without_rewriting_existing_bytes() {
    let mut f = fixture().await;
    let sandbox = actual_sandbox(&mut f).await;
    let original = b"known needle\xffretained binary bytes\n";
    std::fs::write(f._workspace.path().join("invalid-source"), original).unwrap();
    let r = run(&mut f, &["edit_file"], true);
    let provider = Provider::new(vec![(
        "edit_file",
        serde_json::json!({"path":"invalid-source", "old":"known needle", "new":"changed needle"}),
    )]);
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        r.controller
            .prepare_repository_activation(
                r.activation.clone(),
                r.resources(provider.clone()),
                r.resource.clone(),
            )
            .unwrap()
            .run()
            .await
    })
    .await;
    let actual = std::fs::read(f._workspace.path().join("invalid-source"));
    let idle = f.owner.execution_is_idle();
    let cleanup = sandbox.stop_checked().await;
    cleanup.unwrap();
    result.unwrap().unwrap();
    assert_eq!(actual.unwrap(), original);
    assert!(idle.unwrap());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert!(provider.requests.lock().unwrap()[1]
        .iter()
        .filter_map(ChatMessage::text_content)
        .any(|content| content.contains("not valid UTF-8")));
    assert_eq!(
        r.controller
            .snapshot()
            .unwrap()
            .contract()
            .invocations()
            .len(),
        1
    );
}

#[path = "bootstrap_session_repository_driver_tests.rs"]
mod driver_tests;

#[path = "bootstrap_native_ways_tests.rs"]
mod ways_tests;

#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn recovered_closed_registry_preserves_command_and_tool_evidence_without_execution_or_writes()
{
    let mut f = fixture_with_origin(None, false, true).await;
    let sandbox = actual_sandbox(&mut f).await;
    let r = run(&mut f, &["write_file"], true);
    let provider = Provider::new(vec![(
        "write_file",
        serde_json::json!({"path":"retained-result.txt", "content":"exact result"}),
    )]);
    let settled = r
        .controller
        .prepare_repository_activation(
            r.activation.clone(),
            r.resources(provider.clone()),
            r.resource.clone(),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(settled.accepted);
    assert_eq!(
        std::fs::read_to_string(f._workspace.path().join("retained-result.txt")).unwrap(),
        "exact result"
    );
    let session_id = f.owner.metadata().session_id.clone();
    let turn_id = r.activation.turn_id.clone();
    let receipt = rejected_history_action(
        &r.registry,
        &session_id,
        turn_id.as_str(),
        "retained-rejected-command",
    );
    let snapshot = r.controller.snapshot().unwrap();
    r.controller
        .close_and_promote(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("close-complete-read-fixture").unwrap(),
            expected_revision: snapshot.contract().revision(),
            session_id: snapshot.owner().session_id.clone(),
            turn_id: turn_id.clone(),
            event: TurnContractEvent::Close {
                closure: TurnClosure::Finished,
            },
        })
        .unwrap();
    r.registry
        .release_after_turn(&session_id, &turn_id)
        .unwrap();
    let before = r
        .registry
        .control_plane(&session_id, turn_id.as_str())
        .unwrap()
        .unwrap();
    assert!(
        matches!(&before.commands, crate::session_control_plane::EvidenceValue::Available {value} if value == &vec![receipt])
    );
    let wire = serde_json::to_value(&before).unwrap();
    assert!(wire["invocations"]["value"]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value["intent"]["tool_name"] == "write_file"
            && value["scope"] == "durable_invocation_audit"
            && !value["final_evidence"].is_null()));
    let calls = provider.calls.load(Ordering::SeqCst);
    drop(r);
    let session = f
        .owner
        .inner
        .sessions
        .lock()
        .await
        .get(&session_id)
        .unwrap()
        .clone();
    let format = Arc::new(
        axocoatl_session::execution_ownership::UpgradedFormatOwnership::open(f._data.path())
            .unwrap(),
    );
    let stores =
        crate::bootstrap::session_recovery::recover_session_stores(format, &session).unwrap();
    let registry = SessionDispatchRegistry::default();
    let token = registry.retain_existing_session(&mut Some(stores)).unwrap();
    let files = historical_read_tree(f._data.path());
    for _ in 0..3 {
        let after = registry
            .control_plane(&session_id, turn_id.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(after.commands, before.commands);
        assert_eq!(after.invocations, before.invocations);
        assert_eq!(after.turn_revision, before.turn_revision);
        assert_eq!(after.request, before.request);
        assert_eq!(
            after
                .nodes
                .iter()
                .flat_map(|node| node.activations.iter().map(|item| &item.evidence))
                .collect::<Vec<_>>(),
            before
                .nodes
                .iter()
                .flat_map(|node| node.activations.iter().map(|item| &item.evidence))
                .collect::<Vec<_>>()
        );
        let controls = after.turn_controls.unwrap();
        assert!(!controls.finish.enabled && !controls.continue_turn.enabled);
        assert!(
            !controls.finish.requires_revalidation
                && !controls.partial_finish.capability.requires_revalidation
        );
        assert_eq!(
            controls.partial_finish.available_sinks,
            before
                .turn_controls
                .as_ref()
                .unwrap()
                .partial_finish
                .available_sinks
        );
        assert!(after.warnings.is_empty());
    }
    assert_eq!(historical_read_tree(f._data.path()), files);
    assert_eq!(provider.calls.load(Ordering::SeqCst), calls);
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    assert!(registry
        .pending_identity(&token, &f.owner.inner.data_root)
        .is_ok());
    sandbox.stop_checked().await.unwrap();
}
