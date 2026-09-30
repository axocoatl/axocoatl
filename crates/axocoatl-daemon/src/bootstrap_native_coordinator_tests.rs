//! Actual owned native Begin -> Coordinator -> same-driver Worker activations.
//! The finite local test provider reports only its deterministic synthetic usage;
//! this fixture is not a claim about an external model or repository execution.
use super::*;
use crate::bootstrap::session_team::{ApprovedCoordinatorPolicy, ApprovedCoordinatorResource};
use crate::session_dispatch::NativeCoordinatorWorker;
use axocoatl_core::{AgentRole, ChatMessage, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    ProviderExecutionBounds, StreamEvent,
};
use sha2::Digest;
use std::pin::Pin;
use tokio_stream::Stream;

async fn coordinator_fixture(aggregate_tokens: u64) -> NativeFixture {
    coordinator_fixture_with_operations(
        aggregate_tokens,
        vec![
            DelegatedOperation::AddAgent,
            DelegatedOperation::StopActivation,
            DelegatedOperation::RetryActivation,
            DelegatedOperation::FinishNormally,
        ],
    )
    .await
}
async fn coordinator_fixture_with_operations(
    aggregate_tokens: u64,
    operations: Vec<DelegatedOperation>,
) -> NativeFixture {
    let mut fixture = native_fixture().await;
    let session_id = fixture.request.session_id.clone();
    let token = fixture
        .registry
        .session_team_token(session_id.as_str())
        .unwrap();
    let metadata = fixture.repository.owner.metadata().clone();
    let (grant,node)=fixture.registry.with_session_team_stores(&token,|canonical,content,_|{
        let slot_id=SessionTeamSlotId::new("coordinator-slot").unwrap();
        let identity=format!("{:x}",sha2::Sha256::digest(serde_json::to_vec(&(session_id.as_str(),slot_id.as_str())).unwrap()));
        let node_id=TurnNodeId::new(format!("team-node-{}",&identity[..24])).unwrap();
        let conversation_id=NodeConversationId::new("coordinator-conversation").unwrap();
        let mut retained=vec![];
        for (name,role) in [("coordinator",AgentRole::Coordinator),("worker",AgentRole::Worker)] {
            let definition_id=AgentDefinitionId::new(format!("{name}-definition")).unwrap();
            let config=AgentConfig{id:AgentId::new(if role==AgentRole::Coordinator {conversation_id.as_str()}else{"immutable-worker-template"}),role,
                provider:"ollama".into(),model:"test-model".into(),tools:vec![],sampling:SamplingConfig{max_tokens:Some(128),..Default::default()},..Default::default()};
            let profile=ExecutionProfile{definition:definition_id.as_str().into(),provider:"ollama".into(),model:"test-model".into(),isolation:"in-process".into(),tools:vec![],write_scope:None};
            let snapshot=content.retain_activation_evidence(ActivationEvidenceContent::Definition{definition_id:definition_id.clone(),revision:1,profile:profile.clone(),configuration:serde_json::to_string(&config).unwrap()}).unwrap().reference().clone();
            content.retain_provider_profile(canonical,&snapshot,"ollama",serde_json::json!({"fixture":"finite local provider; no external model"}).to_string()).unwrap();
            retained.push((DefinitionSnapshotRef{definition_id,snapshot},profile));
        }
        let child_limits=GrantLimits{activations:2,invocations:4,tokens:10000,cost_microunits:0};
        let parent_limits=GrantLimits{activations:12,invocations:20,tokens:aggregate_tokens,cost_microunits:0};
        let approved=ApprovedCoordinatorPolicy{workers:vec![NativeCoordinatorWorker{template_id:"worker".into(),definition:retained[1].0.clone(),limits:child_limits.clone(),adhoc_allowed:false}],operations,max_nodes:8,max_edges:0,htn_methods_yaml:Some(r#"
- task_pattern: "Do the work"
  preconditions: []
  subtasks:
    - name: "check-a"
      parameters: {description: "Check A"}
      task_type: Primitive
    - name: "check-b"
      parameters: {description: "Check B"}
      task_type: Primitive
"#.into()),resource:ApprovedCoordinatorResource{session_id:session_id.as_str().into(),workspace_id:metadata.workspace_id.clone(),working_dir:fixture.repository._workspace.path().to_path_buf(),environment_generation:metadata.environment_generation,backend:metadata.backend.clone(),network:"none".into(),require_resource_limits:false,image:None,setup_command:None,setup_approved:false,setup_reviewed:true}};
        let issuer=content.retain_activation_evidence(ActivationEvidenceContent::Guidance{text:serde_json::json!({"kind":"authenticated_session_team_apply","edit":{"command_id":"approve-coordinator","expected_configuration_revision":1,"slots":[],"dependencies":[],"layout":[]},"templates":[[slot_id.as_str(),"coordinator"]],"coordinators":[[slot_id.as_str(),approved]]}).to_string()}).unwrap().reference().clone();
        let grant=AuthorityGrant{id:"coordinator-grant".into(),revision:1,issuer_evidence:issuer,holder:node_id.clone(),descendants:vec![],allow_stop_descendants:false,delegation:None,profiles:retained.iter().map(|entry|entry.1.clone()).collect(),conditions:vec![],limits:parent_limits.clone(),expires_at_ms:u64::MAX};
        let grant_ref=content.retain_activation_evidence(ActivationEvidenceContent::Grant{policy:grant.clone()}).unwrap().reference().clone();
        let budget=content.retain_activation_evidence(ActivationEvidenceContent::Budget{limits:parent_limits}).unwrap().reference().clone();
        let mut team=SessionTeamStore::open_owned(canonical.component_namespace(ExecutionComponent::SessionTeam).unwrap(),canonical,content,None).unwrap();
        team.commit(SessionTeamCommit{schema_version:1,command_id:CommandId::new("approve-coordinator").unwrap(),expected_configuration_revision:1,
            graph:SessionTeamGraph{slots:vec![SessionTeamSlot{slot_id:slot_id.clone(),node_id:node_id.clone(),definition:retained[0].0.clone(),conversation_id,required:true,budget,grant:Some(grant_ref)}],dependencies:vec![],conditions:vec![]},initial_source:None,continuity:vec![SlotContinuityDecision{slot_id,decision:SessionTeamContinuity::Reset}],layout:vec![]},canonical,content,None).unwrap();
        Ok((grant,node_id))
    }).unwrap();
    fixture.request.expected_team_revision = 2;
    fixture.request.grants = vec![grant];
    fixture.request.node_evidence = vec![NativeNodeEvidence {
        node_id: node,
        guidance: vec![],
        attachments: vec![],
    }];
    fixture
}
struct FiniteCounter;
impl axocoatl_token::TokenCounter for FiniteCounter {
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
#[derive(Default)]
struct ChildFactoryGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    selected: std::sync::Mutex<Option<ActivationRef>>,
}
struct NestedFactory {
    controller: crate::session_dispatch::SessionDispatchController,
    calls: Arc<AtomicUsize>,
    child_calls: Arc<AtomicUsize>,
    resolved: std::sync::Mutex<Vec<ActivationInputManifest>>,
    gate: Option<Arc<ChildFactoryGate>>,
}
#[async_trait::async_trait]
impl AutonomousActivationFactory for NestedFactory {
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
        if config.role == AgentRole::Worker && input.activation.generation == 1 {
            if let Some(gate) = &self.gate {
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
        Ok(AutonomousActivationResources {
            provider: Arc::new(NestedProvider {
                controller: self.controller.clone(),
                activation: input.activation.clone(),
                child: config.role == AgentRole::Worker,
                calls: self.calls.clone(),
                child_calls: self.child_calls.clone(),
            }),
            config,
            profile,
            counter: Arc::new(FiniteCounter),
            tools: Arc::new(axocoatl_tools::ToolExecutor::new()),
        })
    }
}
struct NestedProvider {
    controller: crate::session_dispatch::SessionDispatchController,
    activation: ActivationRef,
    child: bool,
    calls: Arc<AtomicUsize>,
    child_calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl LlmProvider for NestedProvider {
    fn provider_id(&self) -> &str {
        "ollama"
    }
    fn model_id(&self) -> &str {
        "test-model"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
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
        assert!(
            !self.child,
            "Workers must use their own controlled streaming behavior"
        );
        assert!(
            self.controller
                .activation_provider_usage(&self.activation)
                .unwrap()
                .calls
                > 0
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            content: "Synthesis of the accepted child results".into(),
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: TokenUsageStats::new(5, 5),
            model: "test-model".into(),
            provider: "ollama".into(),
        })
    }
    async fn chat_stream(
        &self,
        _: ChatRequest,
    ) -> std::result::Result<
        Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        assert!(
            self.child,
            "Coordinator must use its own direct controlled provider call"
        );
        assert!(
            self.controller
                .activation_provider_usage(&self.activation)
                .unwrap()
                .calls
                > 0
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.child_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(tokio_stream::iter(vec![
            Ok(StreamEvent::TextDelta {
                delta: format!("Worker result for {}", self.activation.node_id.as_str()),
            }),
            Ok(StreamEvent::Usage(TokenUsageStats::new(5, 5))),
            Ok(StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            }),
        ])))
    }
}

#[tokio::test]
async fn coordinator_runs_reusable_worker_template_as_distinct_canonical_children() {
    let fixture = coordinator_fixture(100000).await;
    let (controller, repository) = begin(&fixture, &fixture.request);
    let factory = Arc::new(NestedFactory {
        controller: controller.clone(),
        calls: Arc::new(AtomicUsize::new(0)),
        child_calls: Arc::new(AtomicUsize::new(0)),
        resolved: std::sync::Mutex::new(vec![]),
        gate: None,
    });
    let prepared = finish_owned_setup(
        &fixture.registry,
        controller.clone(),
        repository,
        &fixture.request.source().unwrap(),
        crate::stream::StreamBus::new(64),
        factory.clone(),
    )
    .unwrap();
    let NativeFirstTurnStart::Prepared(prepared) = prepared else {
        panic!("new native driver")
    };
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), prepared.run())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed),
        "{:?}",
        outcome.snapshot.contract()
    );
    let activations = outcome.snapshot.contract().current_accepted_activations();
    assert_eq!(activations.len(), 3);
    let children = activations
        .iter()
        .filter(|activation| {
            activation.activation.node_id != fixture.request.node_evidence[0].node_id
        })
        .collect::<Vec<_>>();
    assert_eq!(children.len(), 2);
    assert_ne!(children[0].conversation_id, children[1].conversation_id);
    assert_eq!(children[0].input.definition, children[1].input.definition);
    assert_eq!(factory.child_calls.load(Ordering::SeqCst), 2);
    assert_eq!(factory.calls.load(Ordering::SeqCst), 3);
    assert_eq!(outcome.snapshot.contract().graph_history().len(), 2);
    assert!(fixture.registry.live_native_turns().unwrap().is_empty());
    let crate::session_control_plane::EvidenceValue::Available { value: commands } =
        controller.control_plane().unwrap().commands
    else {
        panic!("actual command journal")
    };
    assert_eq!(commands.len(), 2);
    assert!(commands.iter().all(|receipt| matches!(
        receipt.source,
        axocoatl_session::control_command::CommandSourceRecord::Agent { .. }
    ) && receipt.state
        == axocoatl_session::control_command::ControlCommandState::Settled));
}

#[tokio::test]
async fn coordinator_child_reservations_cannot_exceed_parent_aggregate_budget() {
    let fixture = coordinator_fixture(15000).await;
    let (controller, repository) = begin(&fixture, &fixture.request);
    let factory = Arc::new(NestedFactory {
        controller: controller.clone(),
        calls: Arc::new(AtomicUsize::new(0)),
        child_calls: Arc::new(AtomicUsize::new(0)),
        resolved: std::sync::Mutex::new(vec![]),
        gate: None,
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
        panic!("owned native driver")
    };
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), prepared.run())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert_eq!(
        outcome.snapshot.contract().graph().unwrap().nodes.len(),
        2,
        "only one explicitly bounded child can fit"
    );
    assert!(factory.child_calls.load(Ordering::SeqCst) <= 1);
    assert_eq!(
        factory.calls.load(Ordering::SeqCst),
        factory.child_calls.load(Ordering::SeqCst),
        "no parent synthesis may run on incomplete required children"
    );
    assert!(!outcome
        .snapshot
        .contract()
        .current_accepted_activations()
        .iter()
        .any(|item| item.activation.node_id == fixture.request.node_evidence[0].node_id));
}

fn nested_action(
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
async fn stopped_child_continues_once_without_replaying_accepted_sibling_or_forking_driver() {
    let fixture = coordinator_fixture(100000).await;
    let (controller, repository) = begin(&fixture, &fixture.request);
    let gate = Arc::new(ChildFactoryGate::default());
    let factory = Arc::new(NestedFactory {
        controller: controller.clone(),
        calls: Arc::new(AtomicUsize::new(0)),
        child_calls: Arc::new(AtomicUsize::new(0)),
        resolved: std::sync::Mutex::new(vec![]),
        gate: Some(gate.clone()),
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
    let stop = async {
        gate.entered.notified().await;
        let child = gate.selected.lock().unwrap().clone().unwrap();
        let request = nested_action(
            &controller,
            "stop-one-child",
            crate::session_dispatch::HumanControlAction::Stop,
            Some(child.clone()),
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
        assert_eq!(
            receipt.state,
            axocoatl_session::control_command::ControlCommandState::Settled,
            "{receipt:?}"
        );
        gate.release.notify_one();
        child
    };
    let (first, stopped) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(prepared.run(), stop)
    })
    .await
    .unwrap();
    let first = first.unwrap();
    assert_eq!(
        first.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert_eq!(factory.child_calls.load(Ordering::SeqCst), 1);
    assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
    let accepted = first.snapshot.contract().current_accepted_activations();
    assert_eq!(accepted.len(), 1);
    let sibling = accepted[0].activation.clone();
    let restart = first
        .snapshot
        .contract()
        .activations()
        .iter()
        .filter(|item| item.state != ActivationState::Accepted)
        .map(|item| item.activation.clone())
        .collect::<Vec<_>>();
    assert_eq!(restart.len(), 2);
    let request = nested_action(
        &controller,
        "continue-stopped-child",
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
    assert_eq!(
        receipt.state,
        axocoatl_session::control_command::ControlCommandState::Settled,
        "{receipt:?}"
    );
    let driver = controller
        .prepare_native_control_driver(
            &request.command_id,
            repository.clone(),
            bus.clone(),
            factory.clone(),
        )
        .unwrap()
        .unwrap();
    assert!(controller
        .prepare_native_control_driver(
            &request.command_id,
            repository.clone(),
            bus.clone(),
            factory.clone()
        )
        .unwrap()
        .is_none());
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), driver.run())
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
    assert_eq!(factory.child_calls.load(Ordering::SeqCst), 2);
    assert_eq!(factory.calls.load(Ordering::SeqCst), 3);
    let accepted = outcome.snapshot.contract().current_accepted_activations();
    assert!(accepted.iter().any(|item| item.activation == sibling));
    assert!(accepted
        .iter()
        .any(|item| item.activation.node_id == stopped.node_id && item.activation.generation == 2));
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
        .prepare_native_control_driver(&request.command_id, repository, bus, factory)
        .unwrap()
        .is_none());
}

#[path = "bootstrap_native_control_tests.rs"]
mod control_tests;

#[path = "bootstrap_native_knowledge_followup_tests.rs"]
mod knowledge_followup_tests;

#[path = "bootstrap_native_revision_tests.rs"]
mod revision_tests;

#[path = "bootstrap_native_revocation_tests.rs"]
mod revocation_tests;
