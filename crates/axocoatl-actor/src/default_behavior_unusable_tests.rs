// Included in default_behavior::tests. A completed response the model's own
// output makes unusable (no answer and no tool call that can run) is asked
// for once more within the activation, with a note, as a separately
// budgeted call; a second one fails the activation saying so.

/// What the Ollama providers refuse an empty gpt-oss response with.
fn no_answer() -> ProviderError {
    ProviderError::UnusableResponse {
        provider: "ollama".into(),
        message: "the model's response held no answer and no tool call (it wrote only \
                  reasoning). Nothing from this response ran; Ollama ended it with done_reason \
                  \"stop\" and 1 tool offered"
            .into(),
    }
}

/// A completed, refused response: its reasoning, its usage, the refusal.
fn unusable(error: fn() -> ProviderError) -> ScriptedEvents {
    vec![
        Ok(StreamEvent::ReasoningDelta {
            delta: "Continue.".to_string(),
        }),
        Ok(StreamEvent::UsageObservation(
            axocoatl_core::MeasuredTokenUsage::known(TokenUsageStats::new(372, 24)),
        )),
        Err(error()),
    ]
}

fn answered(text: &str) -> ScriptedEvents {
    vec![
        Ok(StreamEvent::TextDelta {
            delta: text.to_string(),
        }),
        Ok(StreamEvent::Usage(TokenUsageStats::new(400, 12))),
        Ok(StreamEvent::Done {
            finish_reason: FinishReason::Stop,
        }),
    ]
}

/// A behavior whose model answers `script(n)` to its n-th call, with
/// `lookup` declared, and what it asked and streamed.
async fn unusable_run(
    script: impl Fn(usize) -> ScriptedEvents + Send + Sync + 'static,
    budget: Option<usize>,
) -> (
    Result<AgentOutput, AgentError>,
    Vec<ChatRequest>,
    Vec<crate::behavior::AgentStreamChunk>,
    DefaultAgentBehavior,
) {
    let provider = Arc::new(ScriptedStreamLlm::new(script));
    let captured = provider.captured.clone();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
        .with_tool_executor(lookup_but_not_report(executions))
        .with_executor_tool_allowlist(["lookup".to_string()]);
    let mut config = AgentConfig::default();
    if let Some(budget) = budget {
        config.token_budget = Some(TokenBudget {
            per_call: budget,
            per_execution: budget,
            overflow_policy: OverflowPolicy::Abort,
        });
    }
    behavior.on_start(&config).await.unwrap();
    let (sink, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    behavior.set_stream_sink(Some(sink));
    let result = behavior
        .execute(AgentInput::text("review the change"))
        .await;
    let mut chunks = Vec::new();
    while let Ok(chunk) = receiver.try_recv() {
        chunks.push(chunk);
    }
    let requests = captured.lock().unwrap().clone();
    (result, requests, chunks, behavior)
}

fn retries(chunks: &[crate::behavior::AgentStreamChunk]) -> Vec<String> {
    chunks
        .iter()
        .filter_map(|chunk| match chunk {
            crate::behavior::AgentStreamChunk::ProviderRetry { reason } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

/// The 1.5.0 measurement's failed review step: gpt-oss wrote only
/// reasoning. The model is asked once more, with a note that says what its
/// response held and what to do; its answer to that is the activation's.
/// Both calls are charged, the re-ask is recorded, and the note stays out of
/// the conversation the Agent keeps.
#[tokio::test]
async fn an_unusable_response_is_asked_for_once_more_with_a_note() {
    let (result, requests, chunks, behavior) = unusable_run(
        |call| {
            if call == 0 {
                unusable(no_answer)
            } else {
                answered("Approved: the change computes the last page correctly.")
            }
        },
        None,
    )
    .await;
    let output = result.expect("the answer to the note is the activation's answer");
    assert_eq!(
        output.content,
        "Approved: the change computes the last page correctly."
    );
    assert_eq!(output.token_usage, TokenUsageStats::new(400, 12));
    assert_eq!(
        behavior.cumulative_token_usage_snapshot(),
        TokenUsageStats::new(772, 36),
        "the refused response's usage is charged too"
    );
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].messages.len(), requests[0].messages.len() + 1);
    assert_eq!(
        requests[1].messages.last().unwrap().text_content(),
        Some(
            "[Note from the host: your last response could not be used, and nothing from it \
             ran: the model's response held no answer and no tool call (it wrote only \
             reasoning). Reply again now: call one of your tools as a tool call, with every \
             value it needs, or write your final answer.]"
        )
    );
    assert_eq!(requests[1].tools.len(), requests[0].tools.len());
    assert_eq!(
        retries(&chunks),
        ["the model's response could not be used (the model's response held no answer and \
          no tool call (it wrote only reasoning)); asking it once more with a note, as a new, \
          separately reserved call"]
    );
    assert!(
        !behavior
            .session()
            .as_chat_messages()
            .iter()
            .any(|message| message
                .text_content()
                .is_some_and(|text| text.contains("Note from the host"))),
        "the note is not kept"
    );
}

/// A second unusable response fails the activation with the model's own
/// failure, worded as a provider error so it is classed by what it says,
/// and saying it answered the note. Nothing ran; nothing is asked a third
/// time.
#[tokio::test]
async fn a_second_unusable_response_fails_the_activation_saying_so() {
    let (result, requests, chunks, _) = unusable_run(|_| unusable(no_answer), None).await;
    let error = result.expect_err("two unusable responses end the activation");
    assert!(matches!(error, AgentError::UnusableResponse(_)), "{error:?}");
    let text = error.to_string();
    assert!(
        text.starts_with(
            "LLM provider error: ollama returned a response that was refused: the model's \
             response held no answer and no tool call"
        ),
        "{text}"
    );
    assert!(
        text.ends_with(
            "This was its answer when asked once more after a response that could not be used \
             either (the model's response held no answer and no tool call (it wrote only \
             reasoning))"
        ),
        "{text}"
    );
    assert_eq!(requests.len(), 2);
    assert_eq!(retries(&chunks).len(), 1);
}

/// When the budget cannot pay for asking again (the refused response's 396
/// tokens leave too little of 400), the refusal stands, says it, and no
/// second call is made.
#[tokio::test]
async fn an_unusable_response_is_not_asked_for_again_past_the_budget() {
    let (result, requests, chunks, _) =
        unusable_run(|_| unusable(no_answer), Some(400)).await;
    let text = result.expect_err("the budget is spent").to_string();
    assert!(
        text.contains("the model's response held no answer and no tool call")
            && text.contains("The model was not asked again: "),
        "{text}"
    );
    assert_eq!(requests.len(), 1);
    assert!(retries(&chunks).is_empty());
}

/// Any other refused response keeps failing the activation at once: only
/// the model's unusable output is asked for again.
#[tokio::test]
async fn a_refused_response_is_not_asked_for_again() {
    let (result, requests, _, _) = unusable_run(
        |_| {
            unusable(|| ProviderError::RefusedResponse {
                provider: "scripted".into(),
                message: "provider returned malformed or non-object tool-call arguments".into(),
            })
        },
        None,
    )
    .await;
    assert!(
        matches!(result, Err(AgentError::Provider(_))),
        "{result:?}"
    );
    assert_eq!(requests.len(), 1);
}

/// Asking once more is a provider call like any other: when its stream ends
/// before its completion event (the 1.5.0 measurement's r3-rust writer,
/// whose Ollama ran out of GPU memory), it is retried once, and that answer
/// is the activation's. Both retries are recorded.
#[tokio::test]
async fn the_call_that_asks_once_more_is_retried_when_its_stream_ends_early() {
    let (result, requests, chunks, _) = unusable_run(
        |call| match call {
            0 => unusable(no_answer),
            1 => vec![Ok(StreamEvent::TextDelta {
                delta: "Let me".to_string(),
            })],
            _ => answered("Fixed: the last page keeps what is left."),
        },
        None,
    )
    .await;
    let output = result.expect("the early end is retried");
    assert_eq!(output.content, "Fixed: the last page keeps what is left.");
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert!(request
            .messages
            .last()
            .and_then(ChatMessage::text_content)
            .is_some_and(|text| text.starts_with("[Note from the host: your last response")));
    }
    let retries = retries(&chunks);
    assert_eq!(retries.len(), 2, "{retries:?}");
    assert!(retries[0].starts_with("the model's response could not be used"));
}
