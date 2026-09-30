// A tool loop that stopped making progress ends with an answer. In the
// 1.1.0 eval a solo Agent restated its finished answer through 18 rounds of
// `bash` `echo "✅ …"` (about 540,000 tokens, no change) until the budget
// wrap-up stopped it.

/// Answers every tool call with a fixed result for its tool, as a sandbox
/// would: the same command gets the same output.
struct FixedResultTool(serde_json::Value);

#[async_trait::async_trait]
impl axocoatl_tools::BuiltinTool for FixedResultTool {
    fn description(&self) -> &str {
        "fixed result"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _arguments: serde_json::Value,
    ) -> Result<serde_json::Value, axocoatl_tools::ToolError> {
        Ok(self.0.clone())
    }
}

fn repository_executor() -> Arc<axocoatl_tools::ToolExecutor> {
    let mut executor = axocoatl_tools::ToolExecutor::new();
    executor.register_builtin(
        "bash",
        Arc::new(FixedResultTool(
            serde_json::json!({"stdout": "ok\n", "stderr": "", "exit_code": 0}),
        )),
    );
    executor.register_builtin(
        "edit_file",
        Arc::new(FixedResultTool(serde_json::json!({"replaced": 1}))),
    );
    Arc::new(executor)
}

/// Calls the scripted tools in order while tools are offered, then answers.
struct ScriptedLoopLlm {
    script: Vec<(&'static str, serde_json::Value)>,
    calls: std::sync::atomic::AtomicUsize,
    captured: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
}

impl ScriptedLoopLlm {
    fn new(script: Vec<(&'static str, serde_json::Value)>) -> Self {
        Self {
            script,
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for ScriptedLoopLlm {
    fn provider_id(&self) -> &str {
        "scripted-loop"
    }
    fn model_id(&self) -> &str {
        "scripted-loop-model"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            ..Default::default()
        }
    }
    async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
        unimplemented!("the loop test streams")
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        let answers = request.tools.is_empty();
        self.captured.lock().unwrap().push(request);
        let n = self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let call = self.script.get(n).filter(|_| !answers);
        let mut events = Vec::new();
        match call {
            Some((name, arguments)) => events.push(Ok(StreamEvent::ToolCallDelta {
                index: Some(0),
                id: format!("call_{n}"),
                name: Some((*name).to_string()),
                args_delta: arguments.to_string(),
            })),
            None => events.push(Ok(StreamEvent::TextDelta {
                delta: "final answer".to_string(),
            })),
        }
        events.push(Ok(StreamEvent::Done {
            finish_reason: if call.is_some() {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
        }));
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

async fn run_script(
    script: Vec<(&'static str, serde_json::Value)>,
) -> (AgentOutput, Vec<ChatRequest>) {
    let provider = Arc::new(ScriptedLoopLlm::new(script));
    let captured = provider.captured.clone();
    let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
        .with_tool_round_limit(128)
        .with_tool_executor(repository_executor());
    behavior.on_start(&AgentConfig::default()).await.unwrap();
    let output = behavior
        .execute(AgentInput::text("fix the collision check"))
        .await
        .expect("the loop ends with an answer");
    let requests = captured.lock().unwrap().clone();
    (output, requests)
}

fn host_note(request: &ChatRequest) -> Option<String> {
    request
        .messages
        .last()
        .and_then(ChatMessage::text_content)
        .filter(|text| text.starts_with("[Note from the host:"))
        .map(str::to_string)
}

#[tokio::test]
async fn an_agent_restating_its_answer_with_echo_is_asked_for_it() {
    let mut script = vec![(
        "bash",
        serde_json::json!({"command": "cd /workspace/repo && npm test"}),
    )];
    for index in 0..18 {
        script.push((
            "bash",
            serde_json::json!({
                "command": format!(
                    "cd /workspace/repo && echo \"✅ Core contract violation fixed ({index})\""
                )
            }),
        ));
    }
    let (output, requests) = run_script(script).await;

    assert_eq!(output.content, "final answer");
    // The test run, three echo rounds, then the request for the answer.
    assert_eq!(requests.len(), 5, "wrap-up within four repeats");
    assert!(requests[..4].iter().all(|request| !request.tools.is_empty()));
    let last = requests.last().unwrap();
    assert!(last.tools.is_empty(), "the final request offers no tools");
    let note = host_note(last).unwrap();
    assert!(
        note.contains("your last 3 tool rounds only printed text and changed nothing"),
        "{note}"
    );
    assert!(note.contains("tools are no longer available"), "{note}");
    assert_eq!(output.tool_calls.len(), 4);
}

#[tokio::test]
async fn an_agent_repeating_the_same_call_for_the_same_result_is_asked_for_its_answer() {
    let read = ("bash", serde_json::json!({"command": "cat lib/manifest.js"}));
    let (output, requests) = run_script(vec![read.clone(); 12]).await;
    assert_eq!(output.content, "final answer");
    assert_eq!(requests.len(), 5, "four identical rounds, then the answer");
    let note = host_note(requests.last().unwrap()).unwrap();
    assert!(
        note.contains("repeated the same calls and got the same results"),
        "{note}"
    );
}

/// Editing and re-running the same test is progress, however often the test
/// command and its output repeat.
#[tokio::test]
async fn an_edit_test_loop_is_never_cut_short() {
    let mut script = Vec::new();
    for index in 0..12 {
        script.push((
            "edit_file",
            serde_json::json!({"path": "lib/manifest.js", "old": "a", "new": format!("b{index}")}),
        ));
        script.push(("bash", serde_json::json!({"command": "npm test"})));
    }
    let rounds = script.len();
    let (output, requests) = run_script(script).await;
    assert_eq!(output.content, "final answer");
    assert_eq!(requests.len(), rounds + 1);
    assert!(requests.iter().all(|request| !request.tools.is_empty()));
    assert!(requests.iter().all(|request| host_note(request).is_none()));
}
