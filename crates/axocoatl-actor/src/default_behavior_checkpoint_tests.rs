// Included inside default_behavior::tests to exercise real controlled execution,
// complete native tool history, and the existing error-prefix accounting path.
struct CheckpointPortProbe {
    initial: std::sync::Mutex<Option<AgentCheckpoint>>,
    restored: std::sync::atomic::AtomicUsize,
    staged: std::sync::Mutex<Vec<serde_json::Value>>,
    limit: usize,
    fail_restore: bool,
    fail_stage: bool,
    stage_started: Option<Arc<tokio::sync::Notify>>,
    stage_release: Option<Arc<tokio::sync::Notify>>,
}

impl CheckpointPortProbe {
    fn new(initial: Option<AgentCheckpoint>) -> Self {
        Self {
            initial: std::sync::Mutex::new(initial),
            restored: std::sync::atomic::AtomicUsize::new(0),
            staged: std::sync::Mutex::new(Vec::new()),
            limit: axocoatl_memory::MAX_CHECKPOINT_BYTES,
            fail_restore: false,
            fail_stage: false,
            stage_started: None,
            stage_release: None,
        }
    }
}

#[async_trait::async_trait]
impl crate::ActivationCheckpointPort for CheckpointPortProbe {
    async fn restore(&self) -> Result<Option<AgentCheckpoint>, String> {
        self.restored.fetch_add(1, BoundaryOrdering::SeqCst);
        if self.fail_restore {
            return Err("missing exact starting checkpoint".into());
        }
        Ok(self.initial.lock().unwrap().take())
    }
    async fn stage(&self, checkpoint: &AgentCheckpoint) -> Result<(), String> {
        self.staged
            .lock()
            .unwrap()
            .push(serde_json::to_value(checkpoint).unwrap());
        if let Some(started) = &self.stage_started {
            started.notify_one();
        }
        if let Some(release) = &self.stage_release {
            release.notified().await;
        }
        if self.fail_stage {
            Err("injected durable candidate failure".into())
        } else {
            Ok(())
        }
    }
    fn maximum_checkpoint_bytes(&self) -> usize {
        self.limit
    }
}

fn activation_checkpoint_config() -> AgentConfig {
    AgentConfig {
        id: AgentId::new("node-conversation"),
        ..Default::default()
    }
}

fn activation_checkpoint_start() -> AgentCheckpoint {
    let mut session = SessionMemory::new();
    session.append(MessageRole::User, "original request", 2);
    session.append(MessageRole::Assistant, "original answer", 2);
    AgentCheckpoint {
        version: 7,
        agent_id: "node-conversation".into(),
        checkpoint_time: 1,
        session_messages: session.messages().to_vec(),
        cumulative_token_usage: TokenUsageStats::new(30, 10).with_reasoning(3),
        cumulative_token_usage_known: true,
        behavior_state: None,
    }
}

fn activation_checkpoint_control() -> AgentRunControl {
    AgentRunControl::new(crate::AgentRunId::new("one-exact-activation"))
}

#[tokio::test]
async fn activation_checkpoint_stages_real_native_messages_once_and_preserves_starting_usage() {
    let mut initial = activation_checkpoint_start();
    initial.session_messages[1]
        .tool_calls
        .push(axocoatl_memory::StoredToolCall {
            id: "retained-call".into(),
            name: "effect".into(),
            arguments_json: r#"{"earlier":true}"#.into(),
            provider_metadata: axocoatl_core::ProviderMetadata::from([(
                "native.signature".into(),
                "retained-exactly".into(),
            )]),
        });
    initial.session_messages.push(StoredMessage {
        content_parts: None,
        role: MessageRole::Tool,
        token_count: 1,
        content: r#"{"prior_result":true}"#.into(),
        timestamp: 1,
        name: Some("effect".into()),
        tool_calls: vec![],
        tool_call_id: Some("retained-call".into()),
    });
    let initial_messages = serde_json::to_value(&initial.session_messages).unwrap();
    let port = Arc::new(CheckpointPortProbe::new(Some(initial)));
    let provider = boundary_provider(1);
    let tool = Arc::new(BoundaryCountingTool::new(
        axocoatl_llm::ConcurrencyPolicy::Exclusive,
    ));
    let mut executor = ToolExecutor::new();
    executor.register_builtin("effect", tool.clone());
    let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
        .with_tool_executor(Arc::new(executor))
        .with_activation_checkpoint_port(port.clone());
    behavior
        .on_start(&activation_checkpoint_config())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(behavior.session.messages()).unwrap(),
        initial_messages
    );
    assert!(behavior.durable_memory_read_only);
    let outcome = behavior
        .execute_controlled(
            AgentInput::text("new request"),
            boundary_control(Arc::new(BoundaryProbe::default())),
        )
        .await
        .unwrap();
    assert_eq!(outcome.output().content, "done");
    assert_eq!(tool.executions.load(BoundaryOrdering::SeqCst), 1);
    {
    let staged = port.staged.lock().unwrap();
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0]["agent_id"], "node-conversation");
    assert_eq!(
        staged[0]["session_messages"],
        serde_json::to_value(behavior.session.messages()).unwrap()
    );
    assert_eq!(
        staged[0]["cumulative_token_usage"],
        serde_json::to_value(TokenUsageStats::new(50, 14).with_reasoning(3)).unwrap()
    );
    assert_eq!(staged[0]["cumulative_token_usage_known"], true);
    }
    let messages = behavior.session.messages();
    assert_eq!(
        messages[1].tool_calls[0].provider_metadata["native.signature"],
        "retained-exactly"
    );
    let assistant = messages
        .iter()
        .rev()
        .find(|message| !message.tool_calls.is_empty())
        .unwrap();
    assert_eq!(assistant.tool_calls[0].name, "effect");
    assert_eq!(assistant.tool_calls[0].arguments_json, r#"{"index":0}"#);
    assert!(messages
        .iter()
        .any(|message| message.role == MessageRole::Tool
            && message.tool_call_id.as_deref() == Some(assistant.tool_calls[0].id.as_str())));
    behavior.on_stop().await.unwrap();
    assert_eq!(port.staged.lock().unwrap().len(), 1);
    assert!(behavior
        .execute_controlled(AgentInput::text("repeat"), activation_checkpoint_control())
        .await
        .is_err());
    assert_eq!(provider.calls.load(BoundaryOrdering::SeqCst), 2);
}

#[tokio::test]
async fn activation_checkpoint_refuses_mixed_legacy_store_in_both_builder_orders() {
    for port_first in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(CheckpointStore::new(
            root.path(),
            axocoatl_memory::CheckpointPolicy::Manual,
        ));
        let port = Arc::new(CheckpointPortProbe::new(None));
        let behavior =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("unused", 1, 1)), simple_counter());
        let mut behavior = if port_first {
            behavior
                .with_activation_checkpoint_port(port.clone())
                .with_checkpoint_store(store)
        } else {
            behavior
                .with_checkpoint_store(store)
                .with_activation_checkpoint_port(port.clone())
        };
        assert!(behavior
            .on_start(&activation_checkpoint_config())
            .await
            .is_err());
        assert_eq!(port.restored.load(BoundaryOrdering::SeqCst), 0);
        assert!(port.staged.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn activation_checkpoint_startup_refuses_foreign_private_missing_and_oversized_state() {
    for fault in ["foreign", "private", "missing", "size", "zero", "version"] {
        let mut initial = activation_checkpoint_start();
        if fault == "foreign" {
            initial.agent_id = "another-conversation".into();
        }
        if fault == "private" {
            initial.behavior_state = Some("coordinator state".into());
        }
        if fault == "version" {
            initial.version = u64::MAX;
        }
        let mut probe = CheckpointPortProbe::new(Some(initial));
        probe.fail_restore = fault == "missing";
        if fault == "size" {
            probe.limit = 1;
        }
        if fault == "zero" {
            probe.limit = 0;
        }
        let port = Arc::new(probe);
        let provider = boundary_provider(0);
        let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
            .with_activation_checkpoint_port(port.clone());
        assert!(
            behavior
                .on_start(&activation_checkpoint_config())
                .await
                .is_err(),
            "{fault}"
        );
        assert!(
            behavior
                .execute_controlled(
                    AgentInput::text("must not run"),
                    activation_checkpoint_control()
                )
                .await
                .is_err(),
            "{fault}"
        );
        assert_eq!(provider.calls.load(BoundaryOrdering::SeqCst), 0, "{fault}");
        assert!(port.staged.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn activation_checkpoint_refuses_plain_and_request_local_execution_and_skips_idle_work() {
    let port = Arc::new(CheckpointPortProbe::new(None));
    let provider = boundary_provider(0);
    let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
        .with_activation_checkpoint_port(port.clone());
    behavior
        .on_start(&activation_checkpoint_config())
        .await
        .unwrap();
    assert!(behavior.session.messages().is_empty());
    assert!(behavior.execute(AgentInput::text("plain")).await.is_err());
    for mode in [
        ConversationMode::Stateless,
        ConversationMode::SuppliedHistory,
    ] {
        let mut input = AgentInput::text("request local");
        input.conversation_mode = mode;
        assert!(behavior
            .execute_controlled(input, activation_checkpoint_control())
            .await
            .is_err());
    }
    assert!(behavior.on_consolidate().await.unwrap().skipped);
    assert_eq!(provider.calls.load(BoundaryOrdering::SeqCst), 0);
    assert!(port.staged.lock().unwrap().is_empty());
}

#[tokio::test]
async fn activation_checkpoint_staging_failure_or_output_overflow_prevents_success() {
    for overflow in [false, true] {
        let mut probe = CheckpointPortProbe::new(None);
        probe.fail_stage = !overflow;
        if overflow {
            probe.limit = 256;
        }
        let port = Arc::new(probe);
        let mut behavior = DefaultAgentBehavior::new(
            Arc::new(MockLlm::new(&"x".repeat(512), 10, 2)),
            simple_counter(),
        )
        .with_activation_checkpoint_port(port.clone());
        behavior
            .on_start(&activation_checkpoint_config())
            .await
            .unwrap();
        let control = activation_checkpoint_control();
        let result = behavior
            .execute_controlled(AgentInput::text("work"), control.clone())
            .await;
        assert!(result.is_err());
        assert!(control.execution_boundary_failure().is_some());
        assert!(control.is_cancelled());
        assert_eq!(port.staged.lock().unwrap().len(), usize::from(!overflow));
        behavior.on_stop().await.unwrap();
        assert_eq!(port.staged.lock().unwrap().len(), usize::from(!overflow));
    }
}

#[tokio::test]
async fn activation_checkpoint_pre_dispatch_stop_stages_once_without_provider_work() {
    let port = Arc::new(CheckpointPortProbe::new(
        Some(activation_checkpoint_start()),
    ));
    let provider = boundary_provider(0);
    let mut behavior = DefaultAgentBehavior::new(provider.clone(), simple_counter())
        .with_activation_checkpoint_port(port.clone());
    behavior
        .on_start(&activation_checkpoint_config())
        .await
        .unwrap();
    let control = activation_checkpoint_control();
    control.cancel();
    let outcome = behavior
        .execute_controlled(AgentInput::text("stopped request"), control)
        .await
        .unwrap();
    assert!(outcome.is_cancelled());
    assert_eq!(provider.calls.load(BoundaryOrdering::SeqCst), 0);
    let staged = port.staged.lock().unwrap();
    assert_eq!(staged.len(), 1);
    assert_eq!(
        staged[0]["session_messages"],
        serde_json::to_value(behavior.session.messages()).unwrap()
    );
    assert_eq!(staged[0]["cumulative_token_usage_known"], true);
}

#[tokio::test]
async fn activation_checkpoint_stop_during_staging_cannot_return_completed() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mut probe = CheckpointPortProbe::new(None);
    probe.stage_started = Some(started.clone());
    probe.stage_release = Some(release.clone());
    let port = Arc::new(probe);
    let mut behavior = DefaultAgentBehavior::new(
        Arc::new(MockLlm::new("finished provider response", 10, 2)),
        simple_counter(),
    )
    .with_activation_checkpoint_port(port.clone());
    behavior
        .on_start(&activation_checkpoint_config())
        .await
        .unwrap();
    let control = activation_checkpoint_control();
    let running_control = control.clone();
    let running = tokio::spawn(async move {
        behavior
            .execute_controlled(AgentInput::text("work"), running_control)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    control.cancel();
    release.notify_one();
    let outcome = running.await.unwrap().unwrap();
    assert!(outcome.is_cancelled());
    assert_eq!(outcome.output().content, "finished provider response");
    assert_eq!(port.staged.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn activation_checkpoint_unknown_provider_failure_stages_only_complete_starting_prefix() {
    let initial = activation_checkpoint_start();
    let messages = serde_json::to_value(&initial.session_messages).unwrap();
    let port = Arc::new(CheckpointPortProbe::new(Some(initial)));
    let mut behavior =
        DefaultAgentBehavior::new(Arc::new(CheckpointFailureProvider), simple_counter())
            .with_activation_checkpoint_port(port.clone());
    behavior
        .on_start(&activation_checkpoint_config())
        .await
        .unwrap();
    assert!(behavior
        .execute_controlled(
            AgentInput::text("failed new request"),
            activation_checkpoint_control()
        )
        .await
        .is_err());
    let staged = port.staged.lock().unwrap();
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0]["session_messages"], messages);
    assert_eq!(staged[0]["cumulative_token_usage_known"], false);
    assert_eq!(
        staged[0]["cumulative_token_usage"],
        serde_json::to_value(TokenUsageStats::new(30, 10).with_reasoning(3)).unwrap()
    );
}

struct CheckpointFailureProvider;
#[async_trait::async_trait]
impl LlmProvider for CheckpointFailureProvider {
    fn provider_id(&self) -> &str {
        "checkpoint-failure"
    }
    fn model_id(&self) -> &str {
        "checkpoint-failure"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }
    async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
        Err(ProviderError::Network(
            "response lost after dispatch".into(),
        ))
    }
    async fn chat_stream(
        &self,
        _: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        Err(ProviderError::Network(
            "response lost after dispatch".into(),
        ))
    }
}
