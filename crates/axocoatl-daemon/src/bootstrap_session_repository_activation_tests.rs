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
use axocoatl_session::control_authority::{AuthorityGrant, ExecutionProfile, GrantLimits};
use axocoatl_session::execution_content::{
    ActivationEvidenceContent, ExecutionContentStore, ExecutionRequestContent,
};
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
    options: &[axocoatl_session::check_options::RequiredCheckOptions],
) -> Vec<CompletionCondition> {
    use axocoatl_session::turn_checks::{
        check_definitions_with_options, readiness_text, CheckGroup,
    };
    let group = CheckGroup::required();
    let definitions = check_definitions_with_options(checks, options).unwrap();
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
    run_with_check_options(f, tools, repository_recorded, writes, checks, &[])
}

/// As `run_with`, with each required check's options as Team Apply
/// admits them.
fn run_with_check_options(
    f: &mut Fixture,
    tools: &[&str],
    repository_recorded: bool,
    writes: Option<&[&str]>,
    checks: &[Vec<String>],
    options: &[axocoatl_session::check_options::RequiredCheckOptions],
) -> Run {
    let canonical = f._canonical.take().unwrap();
    let session_id = canonical.owner().session_id.clone();
    let registry = SessionDispatchRegistry::default();
    let token = registry
        .retain_existing_session(&mut Some(held_stores(canonical)))
        .unwrap();
    let team = registry.session_team_token(session_id.as_str()).unwrap();
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
        session_id,
        turn_id: LogicalTurnId::new("repository-turn").unwrap(),
        execution_epoch_id: ExecutionEpochId::new("repository-epoch").unwrap(),
        node_id: TurnNodeId::new("repository-node").unwrap(),
        generation: 1,
        activation_id: ActivationId::new("repository-activation").unwrap(),
    };
    let definition_id = AgentDefinitionId::new(profile.definition.clone()).unwrap();
    let (definition, conditions) = registry
        .with_session_team_stores(&team, |_, content, _| {
            let definition = content
                .retain_activation_evidence(ActivationEvidenceContent::Definition {
                    definition_id: definition_id.clone(),
                    revision: 1,
                    profile: profile.clone(),
                    configuration: serde_json::to_string(&config).unwrap(),
                })
                .unwrap();
            Ok((
                DefinitionSnapshotRef {
                    definition_id: definition_id.clone(),
                    snapshot: definition.reference().clone(),
                },
                required_check_conditions(content, &activation.node_id, checks, options),
            ))
        })
        .unwrap();
    let spec = crate::session_dispatch::SuccessorTurn {
        command_id: CommandId::new("repository-begin").unwrap(),
        turn_id: activation.turn_id.clone(),
        epoch_id: activation.execution_epoch_id.clone(),
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
        request: ExecutionRequestContent {
            turn_id: activation.turn_id.clone(),
            recorded_at_unix_ms: 1,
            display_input: "Use the retained checkout".into(),
            effective_input: "Use the retained checkout".into(),
            context: vec![],
            target_definition: None,
            model: None,
        },
    };
    let (controller, reference) = registry
        .begin_first_turn_checked(&token, f.owner.clone(), spec, |_, _, _| Ok(()))
        .unwrap();
    let request = controller
        .snapshot()
        .unwrap()
        .request_ref()
        .unwrap()
        .clone();
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
                    guidance: vec![request],
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

    // `GET …/grants` reports what the grant was charged, as the authority
    // settled it: one activation and its one reported model call.
    let team = r
        .registry
        .session_team_token(r.activation.session_id.as_str())
        .unwrap();
    let view = r
        .registry
        .with_session_team_grant_stores(&team, |canonical, content, held| {
            crate::session_dispatch::retained_grant_view(
                canonical,
                content,
                &r.activation.turn_id,
                held,
            )
            .map_err(|error| DaemonError::SessionConflict(error.to_string()))
        })
        .unwrap();
    let view = serde_json::to_value(&view).unwrap();
    let usage = &view["grants"][0]["usage"];
    assert_eq!(usage["activations"], 1, "{view}");
    assert!(usage["invocations"].as_u64().unwrap() >= 1, "{view}");
    assert_eq!(usage["tokens"], 12, "{view}");
    assert_eq!(usage["cost_microunits"], 0, "{view}");
    assert_eq!(view["grants"][0]["policy"]["id"], "repository-grant");
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
    std::fs::create_dir_all(f._workspace.path().join("config")).unwrap();
    std::fs::write(f._workspace.path().join("config/x"), "original\n").unwrap();
    std::fs::write(f._workspace.path().join("config/y"), "a\n").unwrap();
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
    assert_eq!(
        std::fs::read_to_string(f._workspace.path().join("config/x")).unwrap(),
        "original\n"
    );
    assert_eq!(
        std::fs::read_to_string(f._workspace.path().join("config/y")).unwrap(),
        "a\n"
    );
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

pub(super) async fn actual_sandbox(f: &mut Fixture) -> Arc<axocoatl_isolation::SessionSandbox> {
    actual_sandbox_with(f, None).await
}

async fn actual_sandbox_with(
    f: &mut Fixture,
    workload: Option<axocoatl_isolation::WorkloadUsers>,
) -> Arc<axocoatl_isolation::SessionSandbox> {
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
        workload,
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

/// In a hardened Session container a writer's tools run as the writer user
/// and every process of a read-only helper as the helper user, both without
/// capabilities; the host's captures of the helper's work still run as the
/// writer, so the helper's answer is accepted.
#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_hardened_activations_run_as_their_workload_users() {
    let users = axocoatl_isolation::WorkloadUsers {
        writer: (1000, 1000),
        helper: (1001, 1001),
    };
    let command =
        "echo uid=$(id -u); echo home=$HOME; echo cap=$(grep '^CapEff:' /proc/self/status | cut -f2)";
    for (writes, uid, home) in [
        (None, "uid=1000", "home=/home/axocoatl"),
        (Some(&[][..]), "uid=1001", "home=/tmp/axocoatl-home."),
    ] {
        let mut f = fixture().await;
        let sandbox = actual_sandbox_with(&mut f, Some(users)).await;
        git_init(f._workspace.path());
        std::fs::write(f._workspace.path().join("readme.txt"), "hello\n").unwrap();
        let r = match writes {
            Some(writes) => run_scoped(&mut f, &["bash", "read_file"], writes),
            None => run(&mut f, &["bash"], true),
        };
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
        let idle = f.owner.execution_is_idle();
        sandbox.stop_checked().await.unwrap();
        let settled = result.unwrap().unwrap();
        assert!(idle.unwrap());
        let seen: Vec<String> = provider.requests.lock().unwrap()[1]
            .iter()
            .filter_map(ChatMessage::text_content)
            .map(str::to_string)
            .collect();
        assert!(provider.saw(1, uid), "{writes:?}: {seen:?}");
        assert!(provider.saw(1, home), "{writes:?}: {seen:?}");
        assert!(
            provider.saw(1, "cap=0000000000000000"),
            "{writes:?}: {seen:?}"
        );
        assert!(settled.accepted, "{writes:?}: {:?}", settled.failure);
    }
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

/// The glob tool in the real sandbox: path patterns match (1.0 matched only
/// bare names, so `**/*.test.js` and `lib/*.js` found nothing), dependency
/// trees are skipped, and a miss says so plainly.
#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_repository_glob_matches_path_patterns() {
    let mut f = fixture().await;
    let sandbox = actual_sandbox(&mut f).await;
    for file in [
        "lib/a.js",
        "lib/b.test.js",
        "lib/deep/d.test.js",
        "manifest.js",
        "src/manifest-builder.js",
        "node_modules/pkg/f.test.js",
    ] {
        let path = f._workspace.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }
    let r = run(&mut f, &["glob"], true);
    let provider = Provider::new(vec![
        ("glob", serde_json::json!({"pattern":"**/*.test.js"})),
        ("glob", serde_json::json!({"pattern":"lib/*.js"})),
        ("glob", serde_json::json!({"pattern":"**/manifest*.js"})),
        ("glob", serde_json::json!({"pattern":"nothing/*.js"})),
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
    let idle = f.owner.execution_is_idle();
    sandbox.stop_checked().await.unwrap();
    let settled = result.unwrap().unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert!(idle.unwrap());
    assert!(provider.saw(1, r#""files":["lib/b.test.js","lib/deep/d.test.js"]"#));
    assert!(provider.saw(2, r#""files":["lib/a.js","lib/b.test.js"]"#));
    assert!(provider.saw(3, r#""files":["manifest.js","src/manifest-builder.js"]"#));
    assert!(provider.saw(4, "no files match 'nothing/*.js'"));
}

pub(super) fn git_init(path: &std::path::Path) {
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

/// A read-only helper's shell cannot open a TCP connection, even to a live
/// listener on loopback, nor bind a port to listen on: the kernel refuses both
/// (EACCES). A writer's identical command, scoped or not, connects to the
/// same listener, and its attempt to reach an outside address fails only
/// because this sandbox has no network.
#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_read_only_helper_shell_cannot_use_tcp_but_a_writer_can() {
    // nc proves a connection to the live listener; wget names why a
    // connection to a closed port or an outside address failed.
    let command = "ok=no; for attempt in $(seq 1 50); do \
          if echo from-agent | nc -w 2 127.0.0.1 4747; then ok=yes; break; fi; \
          sleep 0.1; \
        done; echo \"connect=$ok\"; \
        wget -q -T 2 -O /dev/null http://127.0.0.1:4749/ 2>&1; \
        wget -q -T 2 -O /dev/null http://1.1.1.1/ 2>&1; \
        timeout 2 nc -l -p 4848 2>&1; true";
    for writes in [Some(&[][..]), Some(&["lib/"][..]), None] {
        let mut f = fixture().await;
        let sandbox = actual_sandbox(&mut f).await;
        git_init(f._workspace.path());
        std::fs::write(f._workspace.path().join("existing.txt"), "original\n").unwrap();
        // Outside the repository, so the listener changes nothing the
        // activation's captures judge.
        sandbox.spawn_background("while :; do nc -l -p 4747 >> /tmp/listener-received; done");
        let r = match writes {
            Some(writes) => run_scoped(&mut f, &["bash", "read_file"], writes),
            None => run(&mut f, &["bash", "read_file"], true),
        };
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
        let received = sandbox
            .exec(&["cat", "/tmp/listener-received"], Duration::from_secs(10))
            .await;
        let idle = f.owner.execution_is_idle();
        sandbox.stop_checked().await.unwrap();
        let settled = result.unwrap().unwrap();
        assert!(idle.unwrap());
        assert!(settled.accepted, "{writes:?}: {:?}", settled.failure);
        let received = received.unwrap().stdout;
        if writes.is_some_and(<[&str]>::is_empty) {
            assert!(provider.saw(1, "connect=no"));
            assert!(provider.saw(1, "(127.0.0.1): Permission denied"));
            assert!(provider.saw(1, "(1.1.1.1): Permission denied"));
            assert!(provider.saw(1, "bind: Permission denied"));
            assert!(!provider.saw(1, "Connection refused"));
            assert!(!provider.saw(1, "Network unreachable"));
            assert_eq!(received, "");
        } else {
            assert!(provider.saw(1, "connect=yes"), "{writes:?}");
            assert!(
                provider.saw(1, "(127.0.0.1): Connection refused"),
                "{writes:?}"
            );
            assert!(
                provider.saw(1, "(1.1.1.1): Network unreachable"),
                "{writes:?}"
            );
            assert!(!provider.saw(1, "Permission denied"), "{writes:?}");
            assert_eq!(received, "from-agent\n");
        }
    }
}

/// An upstream HTTP server on its own Podman network, for egress checks.
/// Everything carries `io.axocoatl.test=egress-<pid>-daemon` and is removed
/// on drop.
struct EgressUpstream {
    label: String,
    network: String,
    container: String,
    ip: String,
    subnet: String,
}

impl EgressUpstream {
    fn podman(args: &[&str]) -> std::process::Output {
        std::process::Command::new("podman")
            .args(args)
            .output()
            .unwrap()
    }

    fn start() -> Self {
        let pid = std::process::id();
        let octet = 100 + pid % 100;
        let upstream = Self {
            label: format!("io.axocoatl.test=egress-{pid}-daemon"),
            network: format!("axo-egress-test-{pid}-daemon"),
            container: format!("axo-egress-up-{pid}-daemon"),
            ip: format!("10.95.{octet}.10"),
            subnet: format!("10.95.{octet}.0/24"),
        };
        let _ = Self::podman(&[
            "rm",
            "--force",
            "--time",
            "0",
            "--ignore",
            &upstream.container,
        ]);
        let _ = Self::podman(&["network", "rm", "--force", &upstream.network]);
        let created = Self::podman(&[
            "network",
            "create",
            "--label",
            &upstream.label,
            "--subnet",
            &upstream.subnet,
            &upstream.network,
        ]);
        assert!(created.status.success(), "{created:?}");
        let started = Self::podman(&[
            "run", "-d", "--name", &upstream.container, "--label", &upstream.label,
            "--network", &upstream.network, "--ip", &upstream.ip,
            "docker.io/library/node:22-alpine", "node", "-e",
            "require('http').createServer((q,r)=>{console.log('ACCESS '+q.url);r.end('hello '+q.url+'\\n')}).listen(8000)",
        ]);
        assert!(started.status.success(), "{started:?}");
        upstream
    }

    fn access_log(&self) -> String {
        String::from_utf8_lossy(&Self::podman(&["logs", &self.container]).stdout).into_owned()
    }
}

impl Drop for EgressUpstream {
    fn drop(&mut self) {
        let _ = Self::podman(&["rm", "--force", "--time", "0", "--ignore", &self.container]);
        let _ = Self::podman(&["network", "rm", "--force", &self.network]);
        // The Sessions' egress and service-socket volumes carry this label.
        let volumes = Self::podman(&[
            "volume",
            "ls",
            "-q",
            "--filter",
            &format!("label={}", self.label),
        ]);
        for volume in String::from_utf8_lossy(&volumes.stdout).split_whitespace() {
            let _ = Self::podman(&["volume", "rm", volume]);
        }
    }
}

async fn actual_egress_sandbox(
    f: &mut Fixture,
    upstream: &EgressUpstream,
    authority: Arc<crate::session_egress::SessionEgress>,
) -> Arc<axocoatl_isolation::SessionSandbox> {
    actual_egress_sandbox_with(f, upstream, authority, None).await
}

async fn actual_egress_sandbox_with(
    f: &mut Fixture,
    upstream: &EgressUpstream,
    authority: Arc<crate::session_egress::SessionEgress>,
    workload: Option<axocoatl_isolation::WorkloadUsers>,
) -> Arc<axocoatl_isolation::SessionSandbox> {
    use axocoatl_isolation::{SandboxNetwork, SandboxPolicy, SessionSandbox};
    use sha2::{Digest, Sha256};
    let image =
        std::env::var("AXO_SUPERVISOR_TEST_IMAGE").expect("set the explicit prepared tools image");
    let policy = SandboxPolicy {
        allow_untrusted_image: true,
        network: SandboxNetwork::Egress,
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
        // As a Session's start does: with routes the container mounts the
        // trust files for the Session's authority.
        egress: Some(axocoatl_isolation::egress::EgressAttachment {
            trust_files: authority.trust_files().unwrap(),
            authority,
            sidecar_network: Some(upstream.network.clone()),
            max_connections: 32,
            labels: vec![upstream.label.clone()],
        }),
        workload,
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

/// Under `network: egress` a writer's shell reaches an allowed host through
/// the proxy with a credential bound to its own tool call, and the record
/// holds bind, open, close and unbind in that order. A read-only helper's
/// shell gets no credential, its proxy connection is refused by Landlock,
/// and nothing is bound for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman (CONTAINER_CONNECTION), AXO_SUPERVISOR_TEST_IMAGE and the egress-capable embedded helper"]
async fn actual_egress_writer_gets_a_bound_credential_and_a_read_only_helper_none() {
    use crate::session_egress::tests::{FakeRecord, FakeResolver};
    use crate::session_egress::{EgressPolicyConfig, SessionEgress};
    use axocoatl_session::network_record::{BindingKind, Decision as Recorded, NetworkEvent};
    let upstream = EgressUpstream::start();
    // With its own proxy settings; then the proxy without a credential over
    // loopback TCP; an unlisted host; and the proxy socket directly.
    let command = "wget -q -T 5 -O - http://upstream.test:8000/from-agent 2>&1; echo \"rc=$?\"; \
                   http_proxy=http://127.0.0.1:3128 wget -q -T 5 -O - http://upstream.test:8000/no-credential 2>&1; \
                   wget -q -T 5 -O - http://not-listed.test/ 2>&1; \
                   printf 'CONNECT upstream.test:8000 HTTP/1.1\\r\\n\\r\\n' | nc local:/run/axocoatl-egress/proxy.sock | head -1; true";
    for writes in [None, Some(&[][..])] {
        let mut f = fixture().await;
        let record = Arc::new(FakeRecord::default());
        let resolver = FakeResolver::with(&[("upstream.test", &[upstream.ip.as_str()])]);
        let egress = SessionEgress::open(
            f.owner.metadata().session_id.clone(),
            EgressPolicyConfig {
                session_allow: vec![axocoatl_config::EgressAllowYaml::Host(
                    axocoatl_config::EgressHostYaml {
                        host: "upstream.test".into(),
                        ports: Some(vec![8000]),
                    },
                )],
                session_private: vec![upstream.subnet.clone()],
                browser: None,
                ..Default::default()
            },
            record.clone(),
            resolver.clone(),
            Some(f.owner.inner.data_root.child("egress-env").unwrap()),
        )
        .await
        .unwrap();
        let sandbox = actual_egress_sandbox(&mut f, &upstream, egress.clone()).await;
        git_init(f._workspace.path());
        let r = match writes {
            Some(writes) => run_scoped(&mut f, &["bash"], writes),
            None => run(&mut f, &["bash"], true),
        };
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
        // Close frames follow the response by a moment.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let idle = f.owner.execution_is_idle();
        sandbox.stop_checked().await.unwrap();
        let settled = result.unwrap().unwrap();
        assert!(idle.unwrap());
        assert!(settled.accepted, "{writes:?}: {:?}", settled.failure);
        let events = record.events();
        let binds: Vec<&NetworkEvent> = events
            .iter()
            .filter(|event| matches!(event, NetworkEvent::Bind { .. }))
            .collect();
        let refused_without_credential = |events: &[NetworkEvent]| {
            events.iter().any(|event| {
                matches!(event, NetworkEvent::Open { decision: Recorded::Deny, reason: Some(reason), token: None, binding: None, .. }
                    if reason == "no_credential")
            })
        };
        if writes.is_some() {
            // No credential, no name resolution, and Landlock refuses the
            // helper's TCP connection to the proxy. The direct socket is the
            // one path left, and the proxy refuses it for lack of a
            // credential; that refusal is recorded.
            assert!(
                provider.saw(1, "(127.0.0.1): Permission denied"),
                "{events:?}"
            );
            assert!(!provider.saw(1, "hello /"));
            assert!(provider.saw(1, "HTTP/1.1 407"));
            assert!(binds.is_empty(), "{binds:?}");
            assert!(refused_without_credential(&events), "{events:?}");
            assert!(!events.iter().any(|event| matches!(
                event,
                NetworkEvent::Open {
                    decision: Recorded::Allow,
                    ..
                }
            )));
            assert!(resolver.queries().is_empty());
            continue;
        }
        assert!(provider.saw(1, "hello /from-agent"), "{events:?}");
        assert!(provider.saw(1, "rc=0"));
        assert!(provider.saw(1, "407 Proxy Authentication Required"));
        assert!(provider.saw(1, "403 Forbidden"));
        assert!(provider.saw(1, "HTTP/1.1 407"));
        assert!(refused_without_credential(&events), "{events:?}");
        // Only the Agent's shell is bound; the host's captures are not.
        assert_eq!(binds.len(), 1, "{events:?}");
        let NetworkEvent::Bind { token, binding, .. } = binds[0] else {
            unreachable!()
        };
        assert_eq!(binding.kind, BindingKind::Agent);
        let invocation = binding.invocation_id.clone().unwrap();
        assert!(r
            .controller
            .snapshot()
            .unwrap()
            .contract()
            .invocations()
            .iter()
            .any(|recorded| recorded.invocation_id.as_str() == invocation));
        assert_eq!(
            binding.activation_id.as_deref(),
            Some(r.activation.activation_id.as_str())
        );
        assert_eq!(
            binding.process.as_deref(),
            Some(format!("{invocation}:0").as_str())
        );
        let position =
            |wanted: &dyn Fn(&NetworkEvent) -> bool| events.iter().position(wanted).unwrap();
        let bind = position(&|event| matches!(event, NetworkEvent::Bind { .. }));
        let open = position(&|event| {
            matches!(event, NetworkEvent::Open { decision: Recorded::Allow, host, token: Some(tag), binding: Some(bound), .. }
                if host == "upstream.test" && tag == token && bound.invocation_id.as_deref() == Some(invocation.as_str()))
        });
        let close =
            position(&|event| matches!(event, NetworkEvent::Close { down, .. } if *down > 0));
        let refused = position(&|event| {
            matches!(event, NetworkEvent::Open { decision: Recorded::Deny, host, reason: Some(reason), .. }
                if host == "not-listed.test" && reason == "not_allowed")
        });
        let unbind = position(
            &|event| matches!(event, NetworkEvent::Unbind { token: unbound, .. } if unbound == token),
        );
        assert!(bind < open && open < close && close < unbind, "{events:?}");
        assert!(refused < unbind);
        // The refused name never reached a resolver.
        assert_eq!(resolver.queries(), ["upstream.test"]);
        assert_eq!(
            upstream.access_log().matches("ACCESS /from-agent").count(),
            1
        );
        assert_eq!(egress.live_bindings(), 0);
    }
}

/// J1, end to end through real containers: a writer's `git` in an egress
/// Session reaches a route host through the real proxy, which relays its TLS
/// bytes to the daemon. The daemon ends TLS with the Session's authority,
/// which `git` trusts only through the trust volume and `GIT_SSL_CAINFO`,
/// checks each request against the route's rules, adds the credential read
/// from its own environment in place of the client's, and connects to the
/// upstream (a TLS server in this process, trusted through the route's
/// `upstream_ca` and this computer's own verifier). A request no rule allows
/// and a request for another `Host` are refused before they leave. The
/// credential is nowhere in the container (environments, `/etc`, `/tmp`,
/// the Workspace), the env files, the containers' configuration or the
/// record. Run as the image's user and as hardened workload users.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman (CONTAINER_CONNECTION), AXO_SUPERVISOR_TEST_IMAGE and the egress-capable embedded helper"]
async fn actual_egress_route_ends_tls_in_the_daemon_and_adds_the_credential() {
    use crate::egress_broker::UpstreamConnector;
    use crate::session_egress::route_tests::{credentials, loopback_is_public, secret, Upstream};
    use crate::session_egress::tests::{FakeRecord, FakeResolver};
    use crate::session_egress::{EgressPolicyConfig, RouteSettings, SessionEgress};
    use axocoatl_session::network_record::{BindingKind, Decision as Recorded, NetworkEvent};
    use base64::Engine as _;
    use std::os::unix::fs::PermissionsExt;
    let secret = secret();
    let upstream_label = EgressUpstream::start();
    let upstream = Upstream::start("git.test").await;
    let port = upstream.addr.port();
    // The upstream's own authority, owner-only and outside the Workspace.
    let ca_dir = tempfile::tempdir().unwrap();
    let ca_file = ca_dir.path().join("upstream-ca.pem");
    std::fs::write(&ca_file, upstream.ca.pem()).unwrap();
    std::fs::set_permissions(&ca_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let route: axocoatl_config::EgressRouteYaml = serde_yaml::from_str(&format!(
        "{{host: git.test, ports: [{port}], credential: test, \
          inject: {{basic: {{username: x-access-token}}}}, upstream_ca: '{}', \
          env_placeholders: [API_TOKEN], \
          rules: [{{methods: [GET], path: /acme/app.git/info/refs, query: {{service: git-upload-pack}}}}, \
                  {{methods: [POST], path: /acme/app.git/git-upload-pack}}]}}",
        ca_file.display()
    ))
    .unwrap();
    let url = format!("https://git.test:{port}/acme/app.git");
    let other = format!("https://git.test:{port}/acme/other.git");
    let command = format!(
        "env > /tmp/agent-env.txt; head -1 /etc/axocoatl/ca/session-ca.pem; \
         echo \"ssl=$SSL_CERT_FILE git=$GIT_SSL_CAINFO token=$API_TOKEN\"; \
         git ls-remote {url} 2>&1; echo \"rc=$?\"; \
         git -c http.extraHeader='Authorization: Bearer client-own' ls-remote {url} >/dev/null 2>&1; echo \"own=$?\"; \
         git ls-remote {other} 2>&1 | tail -1; \
         git -c http.extraHeader='Host: other.test' ls-remote {url} 2>&1 | tail -1; true"
    );
    let users = axocoatl_isolation::WorkloadUsers {
        writer: (1000, 1000),
        helper: (1001, 1001),
    };
    for workload in [None, Some(users)] {
        let mut f = fixture().await;
        let record = Arc::new(FakeRecord::default());
        let env_dir = f.owner.inner.data_root.child("egress-env").unwrap();
        let workspace = f._workspace.path().to_path_buf();
        let egress = SessionEgress::open_session(
            f.owner.metadata().session_id.clone(),
            EgressPolicyConfig {
                routes: vec![route.clone()],
                credentials: credentials(),
                ..EgressPolicyConfig::default()
            },
            record.clone(),
            FakeResolver::with(&[("git.test", &["127.0.0.1"])]),
            Some(env_dir.clone()),
            loopback_is_public,
            RouteSettings {
                upstream: Arc::new(UpstreamConnector::with_local_check(Arc::new(|_| false))),
                workspaces: {
                    let workspace = workspace.clone();
                    Arc::new(move || vec![workspace.clone()])
                },
                ..RouteSettings::default()
            },
        )
        .await
        .unwrap();
        let sandbox =
            actual_egress_sandbox_with(&mut f, &upstream_label, egress.clone(), workload).await;
        git_init(f._workspace.path());
        let r = run(&mut f, &["bash"], true);
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
        tokio::time::sleep(Duration::from_millis(500)).await;
        // The credential is nowhere in the container: not in the Agent's
        // environment, any process's, /etc, /tmp or the Workspace.
        let environments = sandbox
            .exec(
                &[
                    "sh",
                    "-c",
                    "cat /tmp/agent-env.txt; for f in /proc/[0-9]*/environ; do tr '\\0' '\\n' < $f; done 2>/dev/null; true",
                ],
                Duration::from_secs(30),
            )
            .await
            .unwrap()
            .stdout;
        assert!(
            environments.contains("GIT_SSL_CAINFO=/etc/axocoatl/ca/bundle.pem"),
            "{workload:?}"
        );
        assert!(!environments.contains(secret), "{workload:?}");
        let found = sandbox
            .exec(
                &[
                    "sh",
                    "-c",
                    "grep -rIl -F -e \"$1\" /etc /tmp \"$2\" 2>/dev/null; true",
                    "sh",
                    secret,
                    &workspace.display().to_string(),
                ],
                Duration::from_secs(60),
            )
            .await
            .unwrap()
            .stdout;
        assert_eq!(found.trim(), "", "{workload:?}");
        // Nor in the containers' configuration.
        let session = f.owner.metadata().session_id.clone();
        for container in [format!("axo-ses-{session}"), format!("axo-egr-{session}")] {
            let inspect = std::process::Command::new("podman")
                .args(["inspect", &container])
                .output()
                .unwrap();
            assert!(inspect.status.success(), "{container}: {inspect:?}");
            assert!(!String::from_utf8_lossy(&inspect.stdout).contains(secret));
        }
        let mounts = std::process::Command::new("podman")
            .args([
                "inspect",
                "--format",
                "{{range .Mounts}}{{.Name}}:{{.Destination}}:{{.RW}} {{end}}",
                &format!("axo-ses-{session}"),
            ])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&mounts.stdout)
                .contains(&format!("axo-ca-{session}:/etc/axocoatl/ca:false")),
            "{mounts:?}"
        );
        let idle = f.owner.execution_is_idle();
        sandbox.stop_checked().await.unwrap();
        let settled = result.unwrap().unwrap();
        assert!(idle.unwrap());
        assert!(settled.accepted, "{workload:?}: {:?}", settled.failure);
        let seen_text: Vec<String> = provider.requests.lock().unwrap()[1]
            .iter()
            .filter_map(ChatMessage::text_content)
            .map(str::to_string)
            .collect();
        // Git trusted the Session's authority through the trust files, and
        // the route's placeholder stands in for a token.
        for wanted in [
            "-----BEGIN CERTIFICATE-----",
            "ssl=/etc/axocoatl/ca/bundle.pem git=/etc/axocoatl/ca/bundle.pem token=axocoatl-route:git.test",
            // `git ls-remote` printed the advertised branch (the tool's
            // output is JSON, so the tab is escaped).
            "1111111111111111111111111111111111111111\\trefs/heads/main",
            "rc=0",
            "own=0",
            "The requested URL returned error: 403",
            "The requested URL returned error: 421",
        ] {
            assert!(provider.saw(1, wanted), "{workload:?}: {wanted}: {seen_text:?}");
        }

        // The upstream saw the allowed requests only, each with the route's
        // credential and never the client's own header.
        let basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{secret}"))
        );
        let seen = upstream.seen();
        let ours: Vec<_> = seen
            .iter()
            .filter(|request| request.path.starts_with("/acme/"))
            .collect();
        assert!(ours.len() >= 2, "{seen:?}");
        for request in &ours {
            assert_eq!(request.path, "/acme/app.git/info/refs", "{seen:?}");
            assert_eq!(
                request.authorization,
                std::slice::from_ref(&basic),
                "{seen:?}"
            );
        }

        // The record: the route's connections, each allowed request with
        // the credential's name, the refusals with their reasons, and never
        // the value.
        let events = record.events();
        for event in &events {
            event.validate().unwrap();
            assert!(!serde_json::to_string(event).unwrap().contains(secret));
        }
        assert!(
            events
                .iter()
                .any(|event| matches!(event, NetworkEvent::Open {
            decision: Recorded::Allow, rule: Some(rule), host, binding: Some(binding), ..
        } if rule == "route#0" && host == "git.test" && binding.kind == BindingKind::Agent)),
            "{events:#?}"
        );
        // With workload users the route's connections also name the program
        // (git's HTTPS helper, started by git); without them, none does.
        for event in &events {
            let NetworkEvent::Open { host, peer, .. } = event else {
                continue;
            };
            match (workload, peer) {
                (None, peer) => assert_eq!(*peer, None, "{event:?}"),
                (Some(_), Some(peer)) if host == "git.test" => {
                    assert!(
                        peer.exe
                            .as_deref()
                            .is_some_and(|exe| exe.contains("/git-remote-http")),
                        "{peer:?}"
                    );
                    assert_eq!(peer.uid, Some(1000), "{peer:?}");
                    assert!(
                        peer.ancestors
                            .first()
                            .is_some_and(|git| git.ends_with("/git")),
                        "{peer:?}"
                    );
                }
                (Some(_), peer) => panic!("{host}: {peer:?}"),
            }
        }
        let requests = |decision: Recorded, reason: Option<&str>| {
            events
                .iter()
                .filter(|event| {
                    matches!(event, NetworkEvent::Request { decision: d, reason: r, .. }
                    if *d == decision && r.as_deref() == reason)
                })
                .count()
        };
        assert!(requests(Recorded::Allow, None) >= 2, "{events:#?}");
        assert!(events
            .iter()
            .all(|event| !matches!(event, NetworkEvent::Request {
            decision: Recorded::Allow, credential, ..
        } if credential.as_deref() != Some("test"))));
        assert!(
            requests(Recorded::Deny, Some("route_denied")) >= 1,
            "{events:#?}"
        );
        assert!(
            requests(Recorded::Deny, Some("host_mismatch")) >= 1,
            "{events:#?}"
        );
        // The env files are gone with their grants, and none held the value.
        for entry in std::fs::read_dir(env_dir.path()).unwrap() {
            let contents = std::fs::read_to_string(entry.unwrap().path()).unwrap_or_default();
            assert!(!contents.contains(secret));
        }
        assert_eq!(egress.live_bindings(), 0);
    }
}

/// J2 and J3, end to end through real containers and the real decision
/// point: in a hardened egress Session the record of each connection a
/// writer's tool makes names the program that opened it (path, SHA-256,
/// user and parents, as the container's init process found them), a line
/// the tool writes itself to claim another program is refused before
/// anything is recorded, and the tool runs under the supervisor's seccomp
/// filter. With the image's user nothing is named and nothing is filtered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman (CONTAINER_CONNECTION), AXO_SUPERVISOR_TEST_IMAGE and the egress-capable embedded helper"]
async fn actual_hardened_egress_names_each_connections_program_and_filters_agent_commands() {
    use crate::session_egress::tests::{FakeRecord, FakeResolver};
    use crate::session_egress::{EgressPolicyConfig, SessionEgress};
    use axocoatl_session::network_record::{BindingKind, Decision as Recorded, NetworkEvent};
    let upstream = EgressUpstream::start();
    let command = "wget -q -T 5 -O - http://upstream.test:8000/from-agent 2>&1; echo \"rc=$?\"; \
                   git ls-remote http://upstream.test:8000/repo.git >/dev/null 2>&1; echo \"git=$?\"; \
                   grep -E '^(Seccomp_filters|NoNewPrivs):' /proc/self/status; \
                   unshare -U true 2>&1; echo \"unshare=$?\"; \
                   printf 'AXO-PEER/1 {\"exe\":\"/usr/bin/git\",\"uid\":0,\"gid\":0}\\r\\nCONNECT upstream.test:8000 HTTP/1.1\\r\\n\\r\\n' | \
                   nc -w 3 127.0.0.1 3128 2>&1 | tail -1; true";
    let users = axocoatl_isolation::WorkloadUsers {
        writer: (1000, 1000),
        helper: (1001, 1001),
    };
    for workload in [Some(users), None] {
        let mut f = fixture().await;
        let record = Arc::new(FakeRecord::default());
        let resolver = FakeResolver::with(&[("upstream.test", &[upstream.ip.as_str()])]);
        let egress = SessionEgress::open(
            f.owner.metadata().session_id.clone(),
            EgressPolicyConfig {
                session_allow: vec![axocoatl_config::EgressAllowYaml::Host(
                    axocoatl_config::EgressHostYaml {
                        host: "upstream.test".into(),
                        ports: Some(vec![8000]),
                    },
                )],
                session_private: vec![upstream.subnet.clone()],
                browser: None,
                ..Default::default()
            },
            record.clone(),
            resolver.clone(),
            Some(f.owner.inner.data_root.child("egress-env").unwrap()),
        )
        .await
        .unwrap();
        let sandbox = actual_egress_sandbox_with(&mut f, &upstream, egress.clone(), workload).await;
        git_init(f._workspace.path());
        let r = run(&mut f, &["bash"], true);
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
        tokio::time::sleep(Duration::from_millis(500)).await;
        // The files the container runs, and Podman's own filter count, for
        // the expectations below.
        let facts = sandbox
            .exec(
                &[
                    "sh",
                    "-c",
                    "w=$(readlink -f \"$(command -v wget)\"); echo \"$w\"; sha256sum \"$w\" | cut -d' ' -f1; \
                     r=$(readlink -f \"$(git --exec-path)/git-remote-http\"); echo \"$r\"; sha256sum \"$r\" | cut -d' ' -f1; \
                     readlink -f \"$(command -v git)\"; readlink -f /bin/sh; \
                     grep '^Seccomp_filters:' /proc/self/status | cut -f2",
                ],
                Duration::from_secs(30),
            )
            .await
            .unwrap()
            .stdout;
        let facts: Vec<&str> = facts.lines().collect();
        let [wget, wget_sha, remote, remote_sha, git, shell, podman_filters] = facts[..] else {
            panic!("{facts:?}");
        };
        let idle = f.owner.execution_is_idle();
        sandbox.stop_checked().await.unwrap();
        let settled = result.unwrap().unwrap();
        assert!(idle.unwrap());
        assert!(settled.accepted, "{workload:?}: {:?}", settled.failure);
        assert!(provider.saw(1, "hello /from-agent"), "{workload:?}");
        assert!(provider.saw(1, "rc=0"));
        let events = record.events();
        for event in &events {
            event.validate().unwrap();
        }
        let opens: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                NetworkEvent::Open {
                    decision: Recorded::Allow,
                    host,
                    binding: Some(binding),
                    peer,
                    ..
                } if host == "upstream.test" && binding.kind == BindingKind::Agent => Some(peer),
                _ => None,
            })
            .collect();
        // wget, then git's smart and dumb HTTP requests.
        assert!(opens.len() >= 2, "{events:#?}");
        // Nobody stood in for another program: the forged line got a 400
        // and was never recorded.
        assert!(provider.saw(1, "identity_not_accepted"), "{workload:?}");
        assert!(!events.iter().any(|event| matches!(event,
            NetworkEvent::Open { peer: Some(peer), .. } if peer.uid == Some(0))));
        let Some(_) = workload else {
            assert!(opens.iter().all(|peer| peer.is_none()), "{events:#?}");
            // The image's user, with Podman's filter only.
            assert!(provider.saw(1, &format!("Seccomp_filters:\\t{podman_filters}")));
            continue;
        };
        let named = |exe: &str| {
            opens
                .iter()
                .find_map(|peer| {
                    peer.as_ref()
                        .filter(|peer| peer.exe.as_deref() == Some(exe))
                })
                .unwrap_or_else(|| panic!("no connection by {exe}: {events:#?}"))
        };
        let peer = named(wget);
        assert_eq!(
            (peer.uid, peer.gid, peer.error.as_deref()),
            (Some(1000), Some(1000), None)
        );
        assert_eq!(peer.exe_sha256.as_deref(), Some(wget_sha));
        assert_eq!(
            peer.ancestors.first().map(String::as_str),
            Some(shell),
            "{peer:?}"
        );
        assert!(peer
            .ancestors
            .iter()
            .any(|parent| parent == "/axocoatl-exec-supervisor"));
        let peer = named(remote);
        assert_eq!(
            (peer.uid, peer.error.as_deref()),
            (Some(1000), None),
            "{peer:?}"
        );
        assert_eq!(peer.exe_sha256.as_deref(), Some(remote_sha));
        assert_eq!(
            peer.ancestors.first().map(String::as_str),
            Some(git),
            "{peer:?}"
        );
        assert!(opens.iter().all(|peer| peer.is_some()), "{events:#?}");
        // The record line itself carries the program.
        let line = serde_json::to_string(
            events
                .iter()
                .find(|event| matches!(event, NetworkEvent::Open { peer: Some(_), .. }))
                .unwrap(),
        )
        .unwrap();
        assert!(line.contains("\"peer\":{\"pid\":"), "{line}");
        // J3: the Agent's shell ran under the supervisor's filter, one more
        // than Podman's, and could not make a user namespace.
        let filters: u32 = podman_filters.parse().unwrap();
        assert!(
            provider.saw(1, &format!("Seccomp_filters:\\t{}", filters + 1)),
            "{workload:?}"
        );
        assert!(provider.saw(1, "NoNewPrivs:\\t1"));
        assert!(provider.saw(1, "unshare=1"));
        assert!(provider.saw(1, "Operation not permitted"));
    }
}

/// Run one writer activation whose Agent runs `command` with `bash` in an
/// egress Session decided by the real decision point (in-memory record,
/// fake resolver). Returns what the model saw and the recorded events.
async fn run_egress_writer(
    upstream: &EgressUpstream,
    allow: Vec<axocoatl_config::EgressAllowYaml>,
    answers: &[(&str, &[&str])],
    command: &str,
) -> (
    Arc<Provider>,
    Vec<axocoatl_session::network_record::NetworkEvent>,
    Arc<crate::session_egress::tests::FakeResolver>,
) {
    use crate::session_egress::tests::{FakeRecord, FakeResolver};
    use crate::session_egress::{EgressPolicyConfig, SessionEgress};
    let mut f = fixture().await;
    let record = Arc::new(FakeRecord::default());
    let resolver = FakeResolver::with(answers);
    let egress = SessionEgress::open(
        f.owner.metadata().session_id.clone(),
        EgressPolicyConfig {
            session_allow: allow,
            session_private: vec![upstream.subnet.clone()],
            browser: None,
            ..Default::default()
        },
        record.clone(),
        resolver.clone(),
        Some(f.owner.inner.data_root.child("egress-env").unwrap()),
    )
    .await
    .unwrap();
    let sandbox = actual_egress_sandbox(&mut f, upstream, egress).await;
    git_init(f._workspace.path());
    let r = run(&mut f, &["bash"], true);
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
    tokio::time::sleep(Duration::from_millis(500)).await;
    sandbox.stop_checked().await.unwrap();
    let settled = result.unwrap().unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    (provider, record.events(), resolver)
}

fn egress_host(host: &str, ports: &[u16]) -> axocoatl_config::EgressAllowYaml {
    axocoatl_config::EgressAllowYaml::Host(axocoatl_config::EgressHostYaml {
        host: host.into(),
        ports: Some(ports.to_vec()),
    })
}

/// The destinations an Agent might use to reach the host, the VM gateway,
/// metadata services or loopback through an allowed name, each refused with
/// its reason through the real proxy, and recorded. Unlisted names never
/// reach the resolver.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman (CONTAINER_CONNECTION), AXO_SUPERVISOR_TEST_IMAGE and the egress-capable embedded helper"]
async fn actual_egress_refuses_special_destinations_with_their_reasons() {
    use axocoatl_session::network_record::{Decision as Recorded, NetworkEvent};
    let upstream = EgressUpstream::start();
    // The shell builds the proxy credential from its own environment, as a
    // program that speaks CONNECT itself would.
    let command = r#"tok=${HTTPS_PROXY#http://axo:}; tok=${tok%@127.0.0.1:3128}
auth=$(printf 'axo:%s' "$tok" | base64 | tr -d '\n')
try() { printf 'CONNECT %s HTTP/1.1\r\nProxy-Authorization: Basic %s\r\n\r\n' "$1" "$auth" | nc -w 5 127.0.0.1 3128 | head -1 | tr -d '\r'; }
for target in localhost:8080 host.containers.internal:8080 192.168.127.254:8080 169.254.1.2:8080 metadata.google.internal:80 loop.test:8000 2130706433:80 '[::ffff:127.0.0.1]:8080' 1.1.1.1:443 data.attacker.test:443; do
  echo "result $target $(try "$target")"
done
getent hosts data.attacker.test >/dev/null 2>&1 && echo dns=yes || echo dns=no
env -u HTTPS_PROXY -u https_proxy -u HTTP_PROXY -u http_proxy wget -q -T 3 -O /dev/null "http://upstream.test:8000/no-proxy" 2>&1; echo "noproxy=$?""#;
    let (provider, events, resolver) = run_egress_writer(
        &upstream,
        vec![
            egress_host("host.containers.internal", &[8080]),
            egress_host("metadata.google.internal", &[80]),
            egress_host("loop.test", &[8000]),
        ],
        &[
            ("host.containers.internal", &["192.168.127.254"]),
            ("metadata.google.internal", &["169.254.169.254"]),
            ("loop.test", &["127.0.0.1"]),
        ],
        command,
    )
    .await;
    let expected = [
        ("localhost", 8080, 403, "not_allowed"),
        // The Podman machine's host gateway is refused even when a listed
        // name resolves to it, and before the allowlist for a literal.
        (
            "host.containers.internal",
            8080,
            403,
            "forbidden_destination",
        ),
        ("192.168.127.254", 8080, 403, "forbidden_destination"),
        ("169.254.1.2", 8080, 403, "forbidden_destination"),
        ("metadata.google.internal", 80, 403, "forbidden_destination"),
        ("loop.test", 8000, 403, "forbidden_destination"),
        ("2130706433", 80, 400, "invalid_host"),
        ("[::ffff:127.0.0.1]", 8080, 403, "forbidden_destination"),
        ("1.1.1.1", 443, 403, "not_allowed"),
        ("data.attacker.test", 443, 403, "not_allowed"),
    ];
    for (host, port, status, reason) in expected {
        assert!(
            events.iter().any(|event| matches!(event,
                NetworkEvent::Open { decision: Recorded::Deny, host: h, port: p, status: Some(s), reason: Some(r), token: Some(_), .. }
                    if h == host && *p == port && *s == status && r == reason)),
            "{host}:{port} {status} {reason}: {events:?}"
        );
        let seen = if status == 400 {
            "HTTP/1.1 400"
        } else {
            "HTTP/1.1 403"
        };
        assert!(
            provider.saw(1, &format!("result {host}:{port} {seen}")),
            "{host}"
        );
    }
    assert!(!events.iter().any(|event| matches!(
        event,
        NetworkEvent::Open {
            decision: Recorded::Allow,
            ..
        }
    )));
    // Only the listed names were resolved, on the host.
    let mut queried = resolver.queries();
    queried.sort();
    assert_eq!(
        queried,
        [
            "host.containers.internal",
            "loop.test",
            "metadata.google.internal"
        ]
    );
    assert!(provider.saw(1, "dns=no"));
    // Without the proxy variables there is no route and nothing reaches the
    // proxy or its record.
    assert!(provider.saw(1, "noproxy=1"));
    assert!(!events
        .iter()
        .any(|event| matches!(event, NetworkEvent::Open { host, .. } if host == "upstream.test")));
}

/// Two hundred parallel requests through a proxy capped at 32 connections:
/// all complete, and the record has one allowed open per upstream access.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Podman (CONTAINER_CONNECTION), AXO_SUPERVISOR_TEST_IMAGE and the egress-capable embedded helper"]
async fn actual_egress_records_every_one_of_two_hundred_parallel_requests() {
    use axocoatl_session::network_record::{Decision as Recorded, NetworkEvent};
    let upstream = EgressUpstream::start();
    let command = "for i in $(seq 1 200); do wget -q -T 60 -O /dev/null http://upstream.test:8000/p$i & done; wait; echo done";
    let (provider, events, _) = run_egress_writer(
        &upstream,
        vec![egress_host("upstream.test", &[8000])],
        &[("upstream.test", &[upstream.ip.as_str()])],
        command,
    )
    .await;
    assert!(provider.saw(1, "done"));
    let allowed = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                NetworkEvent::Open {
                    decision: Recorded::Allow,
                    ..
                }
            )
        })
        .count();
    let closed = events
        .iter()
        .filter(|event| matches!(event, NetworkEvent::Close { down, .. } if *down > 0))
        .count();
    let accessed = upstream.access_log().matches("ACCESS /p").count();
    assert_eq!((allowed, closed, accessed), (200, 200, 200));
}

/// Measurement, not a check: the wall time of 4 small `bash` tool calls in
/// one writer activation under `network: none` and under `network: egress`
/// (each egress call mints a credential, writes its env file and records
/// bind and unbind). Each call writes one file, because the tool loop stops
/// a run of calls that change nothing, and the fixture grant allows 12
/// model and tool invocations. Prints the per-call times.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement; requires Podman (CONTAINER_CONNECTION) and AXO_SUPERVISOR_TEST_IMAGE"]
async fn actual_egress_tool_call_overhead_measurement() {
    use crate::session_egress::tests::{FakeRecord, FakeResolver};
    use crate::session_egress::{EgressPolicyConfig, SessionEgress};
    const CALLS: usize = 4;
    let upstream = EgressUpstream::start();
    let mut results = Vec::new();
    for round in 0..3 {
        for egress in [false, true] {
            let mut f = fixture().await;
            let sandbox = if egress {
                let authority = SessionEgress::open(
                    f.owner.metadata().session_id.clone(),
                    EgressPolicyConfig::default(),
                    Arc::new(FakeRecord::default()),
                    FakeResolver::with(&[]),
                    Some(f.owner.inner.data_root.child("egress-env").unwrap()),
                )
                .await
                .unwrap();
                actual_egress_sandbox(&mut f, &upstream, authority).await
            } else {
                actual_sandbox(&mut f).await
            };
            git_init(f._workspace.path());
            let r = run(&mut f, &["bash"], true);
            let provider = Provider::new(
                (0..CALLS)
                    .map(|call| {
                        (
                            "bash",
                            serde_json::json!({ "command": format!("echo {call} > call-{call}.txt") }),
                        )
                    })
                    .collect(),
            );
            let started = std::time::Instant::now();
            let result = r
                .controller
                .prepare_repository_activation(
                    r.activation.clone(),
                    r.resources(provider.clone()),
                    r.resource.clone(),
                )
                .unwrap()
                .run()
                .await;
            let elapsed = started.elapsed();
            sandbox.stop_checked().await.unwrap();
            let settled = result.unwrap();
            assert!(settled.accepted, "{:?}", settled.failure);
            assert_eq!(provider.calls.load(Ordering::SeqCst), CALLS + 1);
            let per_call = elapsed.as_secs_f64() * 1000.0 / CALLS as f64;
            eprintln!(
                "measurement round {round} network {}: {CALLS} bash calls in {:.0} ms, {per_call:.1} ms per call",
                if egress { "egress" } else { "none" },
                elapsed.as_secs_f64() * 1000.0
            );
            results.push((egress, per_call));
        }
    }
    let mean = |egress: bool| {
        let values: Vec<f64> = results
            .iter()
            .filter(|(mode, _)| *mode == egress)
            .map(|(_, value)| *value)
            .collect();
        values.iter().sum::<f64>() / values.len() as f64
    };
    eprintln!(
        "measurement mean per call: none {:.1} ms, egress {:.1} ms, difference {:.1} ms",
        mean(false),
        mean(true),
        mean(true) - mean(false)
    );
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

/// The same refusals with an actual supervisor that would run the write: the
/// existing files outside lib/ keep their exact bytes, and the captures
/// confirm nothing changed.
#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_scoped_write_file_is_refused_before_any_effect() {
    let mut f = fixture().await;
    let sandbox = actual_sandbox(&mut f).await;
    let root = f._workspace.path().to_owned();
    git_init(&root);
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::fs::create_dir_all(root.join("lib")).unwrap();
    std::fs::write(root.join("config/x"), "original\n").unwrap();
    std::fs::write(root.join("config/y"), "a\n").unwrap();
    let r = run_scoped(&mut f, &["write_file", "edit_file"], &["lib/"]);
    let provider = Provider::new(vec![
        (
            "write_file",
            serde_json::json!({"path":"config/x", "content":"outside"}),
        ),
        (
            "edit_file",
            serde_json::json!({"path":"config/y", "old":"a", "new":"b"}),
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
    let x = std::fs::read_to_string(root.join("config/x"));
    let y = std::fs::read_to_string(root.join("config/y"));
    let idle = f.owner.execution_is_idle();
    sandbox.stop_checked().await.unwrap();
    let settled = result.unwrap().unwrap();
    assert!(idle.unwrap());
    assert_eq!(x.unwrap(), "original\n");
    assert_eq!(y.unwrap(), "a\n");
    assert!(provider.saw(
        1,
        "config/x is outside the paths this Agent may change (lib/)"
    ));
    assert!(provider.saw(
        2,
        "config/y is outside the paths this Agent may change (lib/)"
    ));
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

#[path = "bootstrap_session_runtime_policy_tests.rs"]
mod runtime_policy_tests;

#[path = "bootstrap_session_browser_tests.rs"]
mod browser_tests;

#[path = "bootstrap_session_browser_egress_tests.rs"]
mod browser_egress_tests;

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
