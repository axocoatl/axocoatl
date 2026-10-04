// Included in default_behavior::tests. A call to a tool the request did not
// declare is answered with a tool error instead of failing the activation, and
// a response refused after its provider completed it keeps its usage.

type ScriptedEvents = Vec<Result<StreamEvent, ProviderError>>;

/// Streams `script(n)` for the n-th provider call and records every request.
struct ScriptedStreamLlm {
    script: Box<dyn Fn(usize) -> ScriptedEvents + Send + Sync>,
    calls: std::sync::atomic::AtomicUsize,
    captured: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
}

impl ScriptedStreamLlm {
    fn new(script: impl Fn(usize) -> ScriptedEvents + Send + Sync + 'static) -> Self {
        Self {
            script: Box::new(script),
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for ScriptedStreamLlm {
    fn provider_id(&self) -> &str {
        "scripted"
    }
    fn model_id(&self) -> &str {
        "scripted-model"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            ..Default::default()
        }
    }
    async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
        unreachable!("the actor streams")
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        self.captured.lock().unwrap().push(request);
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Box::pin(tokio_stream::iter((self.script)(call))))
    }
}

fn report_call(id: &str) -> Result<StreamEvent, ProviderError> {
    Ok(StreamEvent::ToolCallDelta {
        index: Some(0),
        id: id.to_string(),
        name: Some("report".to_string()),
        args_delta: serde_json::json!({"issue": "neighbours only"}).to_string(),
    })
}

/// `lookup` is declared; `report` exists in the executor but is not.
fn lookup_but_not_report(
    executions: Arc<std::sync::atomic::AtomicUsize>,
) -> Arc<axocoatl_tools::ToolExecutor> {
    let mut executor = axocoatl_tools::ToolExecutor::new();
    executor.register_builtin("lookup", Arc::new(ExecutionCounterTool(executions.clone())));
    executor.register_builtin("report", Arc::new(ExecutionCounterTool(executions)));
    Arc::new(executor)
}

#[tokio::test]
async fn an_undeclared_tool_call_is_answered_with_a_tool_error_and_the_model_finishes() {
    let provider = Arc::new(ScriptedStreamLlm::new(|call| {
        if call == 0 {
            vec![
                Ok(StreamEvent::TextDelta {
                    delta: "The comparison only checks neighbours.".to_string(),
                }),
                report_call("report-1"),
                Ok(StreamEvent::Usage(TokenUsageStats::new(11, 3))),
                Ok(StreamEvent::Done {
                    finish_reason: FinishReason::ToolUse,
                }),
            ]
        } else {
            vec![
                Ok(StreamEvent::TextDelta {
                    delta: "Bug: the comparison only checks neighbours.".to_string(),
                }),
                Ok(StreamEvent::Usage(TokenUsageStats::new(17, 5))),
                Ok(StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                }),
            ]
        }
    }));
    let captured = provider.captured.clone();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
        .with_tool_executor(lookup_but_not_report(executions.clone()))
        .with_executor_tool_allowlist(["lookup".to_string()]);
    behavior.on_start(&AgentConfig::default()).await.unwrap();

    let output = behavior
        .execute(AgentInput::text("review lib/manifest.js"))
        .await
        .expect("an undeclared call does not fail the activation");

    assert_eq!(
        output.content,
        "Bug: the comparison only checks neighbours."
    );
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "an undeclared call never runs, even when an executor has that name"
    );
    assert_eq!(output.tool_calls.len(), 1);
    assert_eq!(output.token_usage, TokenUsageStats::new(28, 8));

    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["lookup"]
    );
    let followup = &requests[1];
    let assistant = followup
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Assistant && !message.tool_calls.is_empty())
        .expect("the model's response is kept in its conversation");
    assert_eq!(
        assistant.text_content(),
        Some("The comparison only checks neighbours.")
    );
    assert_eq!(assistant.tool_calls[0].name, "report");
    let result = followup
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Tool)
        .expect("the undeclared call is answered");
    assert_eq!(result.tool_call_id.as_deref(), Some("report-1"));
    assert_eq!(result.name.as_deref(), Some("report"));
    let expected = serde_json::json!({
        "error": "`report` is not an available tool. Available tools: lookup. If you are done, answer without calling a tool."
    });
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(result.text_content().unwrap()).unwrap(),
        expected
    );
}

#[tokio::test]
async fn repeated_undeclared_tool_calls_stop_at_the_tool_round_limit() {
    let provider = Arc::new(ScriptedStreamLlm::new(|call| {
        vec![
            report_call(&format!("report-{call}")),
            Ok(StreamEvent::Done {
                finish_reason: FinishReason::ToolUse,
            }),
        ]
    }));
    let captured = provider.captured.clone();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
        .with_tool_executor(lookup_but_not_report(executions.clone()))
        .with_executor_tool_allowlist(["lookup".to_string()])
        .with_tool_round_limit(2);
    behavior.on_start(&AgentConfig::default()).await.unwrap();

    let error = behavior
        .execute(AgentInput::text("review"))
        .await
        .expect_err("an undeclared call is a tool round like any other");
    assert!(
        matches!(&error, AgentError::ToolRoundLimit { limit: 2, pending } if pending == "report"),
        "{error}"
    );
    assert_eq!(captured.lock().unwrap().len(), 3);
    assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// A response refused after its provider completed it and reported its usage
/// keeps that usage as known, both when the actor refuses it after its
/// completion event and when the provider adapter refuses it.
#[tokio::test]
async fn a_response_refused_after_completion_keeps_its_complete_usage() {
    let usage = TokenUsageStats::new(40, 9);
    for adapter_refused in [false, true] {
        let reported = usage.clone();
        let provider = Arc::new(ScriptedStreamLlm::new(move |_| {
            let observed = Ok(StreamEvent::UsageObservation(
                axocoatl_core::MeasuredTokenUsage::known(reported.clone()),
            ));
            if adapter_refused {
                vec![
                    observed,
                    Err(ProviderError::RefusedResponse {
                        provider: "scripted".into(),
                        message: "provider returned malformed or non-object tool-call arguments"
                            .into(),
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::ToolCallDelta {
                        index: Some(0),
                        id: "bad".to_string(),
                        name: Some("lookup".to_string()),
                        args_delta: "[]".to_string(),
                    }),
                    observed,
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::ToolUse,
                    }),
                ]
            }
        }));
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let port = Arc::new(CheckpointPortProbe::new(None));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
            .with_tool_executor(lookup_but_not_report(executions.clone()))
            .with_activation_checkpoint_port(port.clone());
        behavior
            .on_start(&activation_checkpoint_config())
            .await
            .unwrap();
        let error = behavior
            .execute_controlled(AgentInput::text("work"), activation_checkpoint_control())
            .await
            .expect_err("a malformed call still fails the activation");
        assert!(matches!(error, AgentError::Provider(_)), "{error:?}");
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 0);
        let staged = port.staged.lock().unwrap();
        let checkpoint = serde_json::from_value::<AgentCheckpoint>(staged[0].clone()).unwrap();
        assert_eq!(checkpoint.cumulative_token_usage, usage);
        assert!(
            checkpoint.cumulative_token_usage_known,
            "adapter_refused={adapter_refused}"
        );
    }
}

/// Four identical `bash` rounds trip the loop guard, so the fifth request
/// withholds tools and asks for the final answer; `wrap_up` is its response.
async fn run_to_final_answer_request(
    wrap_up: fn() -> ScriptedEvents,
) -> (
    Result<AgentOutput, AgentError>,
    Vec<ChatRequest>,
    usize,
    Vec<ChatMessage>,
) {
    let provider = Arc::new(ScriptedStreamLlm::new(move |call| {
        if call < 4 {
            vec![
                Ok(StreamEvent::ToolCallDelta {
                    index: Some(0),
                    id: format!("bash-{call}"),
                    name: Some("bash".to_string()),
                    args_delta: serde_json::json!({"command": "cat lib/manifest.js"}).to_string(),
                }),
                Ok(StreamEvent::Done {
                    finish_reason: FinishReason::ToolUse,
                }),
            ]
        } else {
            wrap_up()
        }
    }));
    let captured = provider.captured.clone();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut executor = axocoatl_tools::ToolExecutor::new();
    executor.register_builtin("bash", Arc::new(ExecutionCounterTool(executions.clone())));
    let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
        .with_tool_round_limit(128)
        .with_tool_executor(Arc::new(executor));
    behavior.on_start(&AgentConfig::default()).await.unwrap();
    let result = behavior
        .execute(AgentInput::text("fix the collision check"))
        .await;
    let requests = captured.lock().unwrap().clone();
    let executions = executions.load(std::sync::atomic::Ordering::SeqCst);
    let history = behavior.session().as_chat_messages();
    (result, requests, executions, history)
}

fn stray_bash_call() -> Result<StreamEvent, ProviderError> {
    Ok(StreamEvent::ToolCallDelta {
        index: Some(0),
        id: "stray".to_string(),
        name: Some("bash".to_string()),
        args_delta: serde_json::json!({"command": "echo done"}).to_string(),
    })
}

#[tokio::test]
async fn a_final_answer_with_a_stray_tool_call_is_accepted_without_another_round() {
    let (result, requests, executions, history) = run_to_final_answer_request(|| {
        vec![
            Ok(StreamEvent::TextDelta {
                delta: "Fixed: every pair is compared now.".to_string(),
            }),
            stray_bash_call(),
            Ok(StreamEvent::Done {
                finish_reason: FinishReason::ToolUse,
            }),
        ]
    })
    .await;
    let output = result.expect("the text is the final answer");
    assert_eq!(output.content, "Fixed: every pair is compared now.");
    assert_eq!(requests.len(), 5, "the stray call starts no further round");
    assert!(requests[4].tools.is_empty(), "tools were withheld");
    assert_eq!(executions, 4, "the stray call never runs");
    assert_eq!(output.tool_calls.len(), 4);
    let last = history.last().unwrap();
    assert_eq!(last.role, MessageRole::Assistant);
    assert!(
        last.tool_calls.is_empty(),
        "the dropped call is not recorded"
    );
    assert_eq!(
        last.text_content(),
        Some("Fixed: every pair is compared now.")
    );
}

#[tokio::test]
async fn a_final_answer_request_answered_only_with_a_tool_call_fails_plainly() {
    let (result, requests, executions, _) = run_to_final_answer_request(|| {
        vec![
            stray_bash_call(),
            Ok(StreamEvent::Done {
                finish_reason: FinishReason::ToolUse,
            }),
        ]
    })
    .await;
    let error = result.expect_err("no answer was written");
    let AgentError::ToolFailed { reason, .. } = &error else {
        panic!("unexpected error: {error:?}");
    };
    assert!(
        reason.contains("asked for its final answer")
            && reason.contains("it called a tool instead (bash)"),
        "{reason}"
    );
    assert_eq!(requests.len(), 5, "no extra round");
    assert_eq!(executions, 4);
}
