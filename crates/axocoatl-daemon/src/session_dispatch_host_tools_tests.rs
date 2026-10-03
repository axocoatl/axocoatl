//! Host-invocation tools through real native actor admission: offered only
//! when listed, bound to the exact admitted call, declined with a reason
//! when the daemon cannot run them, and settled like any other tool.
use super::*;
use crate::session_dispatch::{HostInvocationContext, HostInvocationTool};

/// Calls `tool` once with `arguments`, then answers with text.
struct HostToolProvider {
    tool: &'static str,
    arguments: serde_json::Value,
    expect_offered: bool,
    call: bool,
    expect_result: &'static str,
    calls: AtomicUsize,
    results: Mutex<Vec<String>>,
}

impl HostToolProvider {
    fn new(
        tool: &'static str,
        expect_offered: bool,
        expect_result: &'static str,
    ) -> Arc<Self> {
        Arc::new(Self {
            tool,
            arguments: serde_json::json!({"url": "http://localhost:8765/"}),
            expect_offered,
            call: expect_offered,
            expect_result,
            calls: AtomicUsize::new(0),
            results: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl LlmProvider for HostToolProvider {
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
        let round = self.calls.fetch_add(1, Ordering::SeqCst);
        let offered = request.tools.iter().any(|tool| tool.name == self.tool);
        assert_eq!(offered, self.expect_offered, "offered tools: {:?}", request.tools.iter().map(|tool| &tool.name).collect::<Vec<_>>());
        let call = self.call && round == 0;
        let mut events = if call {
            vec![Ok(StreamEvent::ToolCallDelta {
                index: Some(0),
                id: "host-tool-call".into(),
                name: Some(self.tool.into()),
                args_delta: self.arguments.to_string(),
            })]
        } else {
            if self.call {
                let result = request
                    .messages
                    .iter()
                    .find(|message| message.tool_call_id.as_deref() == Some("host-tool-call"))
                    .and_then(|message| message.text_content())
                    .expect("the acknowledged host tool result")
                    .to_owned();
                assert!(result.contains(self.expect_result), "{result}");
                self.results.lock().unwrap().push(result);
            }
            vec![Ok(StreamEvent::TextDelta {
                delta: "done".into(),
            })]
        };
        events.push(Ok(StreamEvent::Usage(TokenUsageStats::new(10, 2))));
        events.push(Ok(StreamEvent::Done {
            finish_reason: if call {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
        }));
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

#[derive(Default)]
struct FakeHostTool {
    refusal: Option<String>,
    bound: Mutex<Vec<HostInvocationContext>>,
    executed: AtomicUsize,
}

struct BoundFake {
    owner: Arc<FakeHostTool>,
    context: HostInvocationContext,
}

#[async_trait]
impl axocoatl_tools::BuiltinTool for BoundFake {
    fn description(&self) -> &str {
        "fake browser"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, axocoatl_tools::ToolError> {
        self.owner.executed.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::json!({
            "invocation": self.context.invocation_id.as_str(),
            "agent": self.context.agent,
            "url": arguments["url"],
        }))
    }
}

struct FakeRegistration(Arc<FakeHostTool>);

impl HostInvocationTool for FakeRegistration {
    fn name(&self) -> &'static str {
        "browser"
    }
    fn definition(&self) -> Arc<dyn axocoatl_tools::BuiltinTool> {
        Arc::new(axocoatl_tools::BrowserTool::definition())
    }
    fn refusal(&self, _: &ExecutionProfile) -> Option<String> {
        self.0.refusal.clone()
    }
    fn bind(&self, context: HostInvocationContext) -> Arc<dyn axocoatl_tools::BuiltinTool> {
        self.0.bound.lock().unwrap().push(context.clone());
        Arc::new(BoundFake {
            owner: self.0.clone(),
            context,
        })
    }
}

fn intents(controller: &SessionDispatchController) -> Vec<InvocationId> {
    let state = controller.lock().unwrap();
    state
        .canonical
        .records()
        .unwrap()
        .iter()
        .filter_map(|record| match &record.event {
            TurnContractEvent::RecordIntent { invocation_id, .. } => Some(invocation_id.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_listed_host_tool_is_offered_and_bound_to_the_exact_call() {
    let fixture = input_fixture_with_tools(false, &["browser"]);
    let fake = Arc::new(FakeHostTool::default());
    fixture
        .controller
        .register_host_invocation_tool(Arc::new(FakeRegistration(fake.clone())))
        .unwrap();
    start_input(&fixture.controller, &fixture.parent);
    let mut resources = input_resources(&fixture.parent, InputProvider::new("unused", false, false));
    let provider = HostToolProvider::new("browser", true, "\"agent\":\"parent\"");
    resources.provider = provider.clone();
    let result = fixture
        .controller
        .prepare_autonomous_activation(fixture.parent.input.activation.clone(), resources)
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(result.accepted, "{:?}", result.failure);
    assert_eq!(fake.executed.load(Ordering::SeqCst), 1);
    let bound = fake.bound.lock().unwrap().clone();
    assert_eq!(bound.len(), 1);
    let intents = intents(&fixture.controller);
    assert_eq!(intents.len(), 1);
    assert_eq!(bound[0].invocation_id, intents[0]);
    assert_eq!(bound[0].activation, fixture.parent.input.activation);
    assert_eq!(bound[0].session_id, "input-session");
    assert!(!bound[0].read_only);
    assert!(bound[0].checkout.is_none());
    assert!(provider.results.lock().unwrap()[0].contains(intents[0].as_str()));
    let state = fixture.controller.lock().unwrap();
    assert!(state.canonical.records().unwrap().iter().any(|record| matches!(
        &record.event,
        TurnContractEvent::RecordOutcome { outcome: InvocationOutcome::Succeeded, invocation_id, .. }
            if *invocation_id == intents[0]
    )));
}

#[tokio::test]
async fn an_unlisted_host_tool_is_not_offered() {
    let fixture = input_fixture_with_tools(false, &["effect"]);
    let fake = Arc::new(FakeHostTool::default());
    fixture
        .controller
        .register_host_invocation_tool(Arc::new(FakeRegistration(fake.clone())))
        .unwrap();
    start_input(&fixture.controller, &fixture.parent);
    let mut resources = input_resources(&fixture.parent, InputProvider::new("unused", false, false));
    resources.provider = HostToolProvider::new("browser", false, "");
    let result = fixture
        .controller
        .prepare_autonomous_activation(fixture.parent.input.activation.clone(), resources)
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(result.accepted, "{:?}", result.failure);
    assert!(fake.bound.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_listed_host_tool_the_daemon_cannot_run_is_declined_with_its_reason() {
    let fixture = input_fixture_with_tools(false, &["browser"]);
    let fake = Arc::new(FakeHostTool {
        refusal: Some("the browser block is not configured".into()),
        ..Default::default()
    });
    fixture
        .controller
        .register_host_invocation_tool(Arc::new(FakeRegistration(fake.clone())))
        .unwrap();
    // A refused tool is not offered, but a model can still name it.
    start_input(&fixture.controller, &fixture.parent);
    let mut resources = input_resources(&fixture.parent, InputProvider::new("unused", false, false));
    let mut provider = HostToolProvider::new("browser", false, "");
    Arc::get_mut(&mut provider).unwrap().call = true;
    resources.provider = provider;
    let result = fixture
        .controller
        .prepare_autonomous_activation(fixture.parent.input.activation.clone(), resources)
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(result.accepted, "{:?}", result.failure);
    assert!(fake.bound.lock().unwrap().is_empty());
    assert_eq!(fake.executed.load(Ordering::SeqCst), 0);
    assert!(intents(&fixture.controller).is_empty(), "a declined call records nothing");

    // Admission repeats the check and declines with the reason.
    let state = fixture.controller.lock().unwrap();
    let refusal = state
        .host_tool_refusal(&fixture.parent.input.activation, "browser")
        .unwrap();
    assert_eq!(refusal.as_deref(), Some("the browser block is not configured"));
    assert_eq!(
        state
            .host_tool_refusal(&fixture.parent.input.activation, "read_file")
            .unwrap(),
        None
    );
}

#[test]
fn only_host_invocation_names_can_be_registered() {
    struct Named;
    impl HostInvocationTool for Named {
        fn name(&self) -> &'static str {
            "bash"
        }
        fn definition(&self) -> Arc<dyn axocoatl_tools::BuiltinTool> {
            Arc::new(axocoatl_tools::BrowserTool::definition())
        }
        fn refusal(&self, _: &ExecutionProfile) -> Option<String> {
            None
        }
        fn bind(&self, _: HostInvocationContext) -> Arc<dyn axocoatl_tools::BuiltinTool> {
            Arc::new(axocoatl_tools::BrowserTool::definition())
        }
    }
    let fixture = input_fixture();
    assert!(fixture
        .controller
        .register_host_invocation_tool(Arc::new(Named))
        .is_err());
    crate::session_dispatch::validate_repository_tools(&[
        "read_file".into(),
        "browser".into(),
        "browser_check".into(),
    ])
    .unwrap();
    assert!(crate::session_dispatch::validate_repository_tools(&["selenium".into()]).is_err());
}
