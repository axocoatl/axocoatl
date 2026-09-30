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
    operations: Vec<(&'static str, serde_json::Value)>,
}
impl Provider {
    fn new(operations: Vec<(&'static str, serde_json::Value)>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            requests: Mutex::new(vec![]),
            operations,
        })
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
        ..Default::default()
    };
    let profile = ExecutionProfile {
        definition: "repository-definition".into(),
        provider: config.provider.clone(),
        model: config.model.clone(),
        isolation: "in-process".into(),
        tools: config.tools.clone(),
        write_scope: None,
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
                        conditions: vec![],
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
