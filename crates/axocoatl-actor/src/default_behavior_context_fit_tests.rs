// Context fitting and budget wrap-up, from the 1.1.0 live eval on a local
// model (qwen3-coder at 32,768 tokens with a 4,096-token answer).

/// A quarter token per character, counting tool calls like the real
/// counter, so a test can size a conversation exactly.
fn quarter_counter() -> Arc<dyn TokenCounter> {
    struct QuarterCounter;
    impl TokenCounter for QuarterCounter {
        fn count_text(&self, text: &str) -> usize {
            text.chars().count() / 4
        }
        fn count_messages(&self, messages: &[ChatMessage]) -> usize {
            3 + messages
                .iter()
                .map(|message| {
                    let calls = if message.tool_calls.is_empty() {
                        0
                    } else {
                        self.count_text(&serde_json::to_string(&message.tool_calls).unwrap())
                    };
                    4 + self.count_text(message.text_content().unwrap_or("")) + calls
                })
                .sum::<usize>()
        }
        fn count_tool_definition(&self, value: &serde_json::Value) -> usize {
            self.count_text(&value.to_string())
        }
    }
    Arc::new(QuarterCounter)
}

/// A Session host's budget as the provider port reports it: every call
/// reserves `reservation` tokens and one invocation, every tool call one more.
#[derive(Default)]
struct FakeGrant {
    allowance: axocoatl_llm::ProviderAllowance,
    reservation: u64,
}

/// A tool loop: each round the Agent writes `text_chars` of its own text
/// (which masking never shortens) and echoes `arg_chars`, numbered so no two
/// rounds repeat. It answers when no tools are offered, or after `rounds`
/// rounds.
struct LongLoopLlm {
    rounds: usize,
    text_chars: usize,
    arg_chars: usize,
    context: usize,
    /// Reported usage per call, when the test needs the guard to see some.
    usage: Option<TokenUsageStats>,
    grant: Option<std::sync::Mutex<FakeGrant>>,
    calls: std::sync::atomic::AtomicUsize,
    captured: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
}

impl LongLoopLlm {
    fn new(rounds: usize, text_chars: usize, arg_chars: usize, context: usize) -> Self {
        Self {
            rounds,
            text_chars,
            arg_chars,
            context,
            usage: None,
            grant: None,
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for LongLoopLlm {
    fn provider_id(&self) -> &str {
        "long-loop"
    }
    fn model_id(&self) -> &str {
        "long-loop-model"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            max_context_tokens: self.context,
            max_output_tokens: if self.context > 0 { 4_096 } else { 0 },
            ..Default::default()
        }
    }
    fn execution_bounds(&self, _: &ChatRequest) -> Option<axocoatl_llm::ProviderExecutionBounds> {
        let grant = self.grant.as_ref()?.lock().unwrap();
        Some(axocoatl_llm::ProviderExecutionBounds {
            token_limit: grant.reservation,
            cost_microunits: 0,
            response_bytes: 1 << 20,
        })
    }
    fn remaining_allowance(&self) -> Option<axocoatl_llm::ProviderAllowance> {
        Some(self.grant.as_ref()?.lock().unwrap().allowance)
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
        if let Some(grant) = &self.grant {
            // Admission, as the host does it.
            let mut grant = grant.lock().unwrap();
            let reservation = grant.reservation;
            let allowance = &mut grant.allowance;
            if allowance.tokens.is_some_and(|left| left < reservation)
                || allowance.invocations == Some(0)
            {
                return Err(ProviderError::BudgetExhausted {
                    provider: "long-loop".into(),
                    message: "The Session budget for this Agent is used up: 1,000 of its \
                              10,000 tokens remain and the next model call needs 2,000."
                        .into(),
                });
            }
            allowance.tokens = allowance.tokens.map(|left| left - reservation);
            let invocations = if answers { 1 } else { 2 };
            allowance.invocations = allowance
                .invocations
                .map(|left| left.saturating_sub(invocations));
        }
        self.captured.lock().unwrap().push(request);
        let n = self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut events = Vec::new();
        if answers || n >= self.rounds {
            events.push(Ok(StreamEvent::TextDelta {
                delta: "final answer".to_string(),
            }));
        } else {
            events.push(Ok(StreamEvent::TextDelta {
                delta: format!("{n} {}", prose(self.text_chars)),
            }));
            events.push(Ok(StreamEvent::ToolCallDelta {
                index: Some(0),
                id: format!("call_{n}"),
                name: Some("echo".to_string()),
                args_delta: serde_json::json!({"text": numbered(n, prose(self.arg_chars))})
                    .to_string(),
            }));
        }
        if let Some(usage) = &self.usage {
            events.push(Ok(StreamEvent::Usage(usage.clone())));
        }
        events.push(Ok(StreamEvent::Done {
            finish_reason: if answers || n >= self.rounds {
                FinishReason::Stop
            } else {
                FinishReason::ToolUse
            },
        }));
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

/// `chars` characters of ordinary words (a tokenizer's cheap case).
fn prose(chars: usize) -> String {
    "the file reads ".repeat(chars / 15 + 1)[..chars].to_string()
}

/// `text` with its start replaced by the round number, keeping its length,
/// so each round's call differs from the last as distinct work does.
fn numbered(round: usize, text: String) -> String {
    let number = format!("{round:04}");
    if text.len() < number.len() {
        return text;
    }
    format!("{number}{}", &text[number.len()..])
}

fn echo_executor() -> Arc<axocoatl_tools::ToolExecutor> {
    let mut executor = axocoatl_tools::ToolExecutor::new();
    executor.register_builtin("echo", Arc::new(axocoatl_tools::EchoTool));
    Arc::new(executor)
}

fn answer_limit(max_tokens: usize) -> AgentConfig {
    AgentConfig {
        sampling: axocoatl_core::SamplingConfig {
            max_tokens: Some(max_tokens),
            ..Default::default()
        },
        ..AgentConfig::default()
    }
}

fn dropped_note(request: &ChatRequest) -> Option<String> {
    request.messages.iter().find_map(|message| {
        message
            .text_content()
            .filter(|text| text.contains("earlier tool rounds in this task were removed"))
            .map(str::to_string)
    })
}

/// 1.1.0 kept only the last two rounds once a loop outgrew the context and,
/// rebuilding from the whole history, did so again on every later request:
/// the Agent re-read the same files until its budget ran out.
#[tokio::test]
async fn a_long_loop_in_a_small_context_keeps_the_recent_rounds_that_fit() {
    let provider = Arc::new(LongLoopLlm::new(90, 1_600, 2_400, 32_768));
    let captured = provider.captured.clone();
    let mut behavior = DefaultAgentBehavior::new(provider, quarter_counter())
        .with_tool_round_limit(128)
        .with_tool_executor(echo_executor())
        .with_stale_tool_result_masking(3, []);
    behavior.on_start(&answer_limit(4_096)).await.unwrap();

    let output = behavior.execute(AgentInput::text("fix it")).await.unwrap();
    assert_eq!(output.content, "final answer");

    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 91);
    let counter = quarter_counter();
    let first = requests
        .iter()
        .position(|request| dropped_note(request).is_some())
        .expect("90 rounds outgrow a 32k context");
    assert!(first > 40, "the context is used before anything is left out");
    let mut changes = 0;
    let mut previous = None;
    for request in &requests[first..] {
        assert!(counter.count_messages(&request.messages) + 4_096 <= 32_768);
        let kept = request
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Tool)
            .count();
        assert!(kept >= 10, "only {kept} recent rounds were kept");
        let note = dropped_note(request);
        if note != previous {
            changes += 1;
            previous = note;
        }
    }
    // Rounds are left out when the context overflows, not on every request;
    // between those the request prefix stays the same.
    assert!(changes <= 3, "rounds were left out on {changes} requests");
    assert!(requests.len() - first > 3 * changes);
}

/// Arm B's lead in the eval: request 34 carried 22,808 prompt tokens and the
/// next, with that round's call and result added, about 24,379 — mostly the
/// Agent's own text, which masking cannot shorten. 1.1.0 measured that plus
/// the 4,096-token answer against 85% of the window and left 32 of the 34
/// rounds out; a 32,768-token context holds it with room to spare.
#[tokio::test]
async fn the_eval_s_request_fits_a_32k_context_without_leaving_rounds_out() {
    for target in [22_808, 24_379] {
        let provider = Arc::new(LongLoopLlm::new(0, 0, 0, 32_768));
        let mut behavior = DefaultAgentBehavior::new(provider, quarter_counter())
            .with_tool_executor(echo_executor())
            .with_stale_tool_result_masking(3, []);
        behavior.on_start(&answer_limit(4_096)).await.unwrap();
        let seed = |behavior: &mut DefaultAgentBehavior, pad: usize| {
            behavior.session = SessionMemory::new();
            let task = format!("task {}", "p".repeat(pad * 4));
            behavior.session.append(MessageRole::User, task, 0);
            for index in 0..34 {
                let call = ToolCall {
                    id: format!("call_{index}"),
                    name: "echo".into(),
                    arguments: serde_json::json!({"text": "lib/manifest.js"}),
                    provider_metadata: Default::default(),
                };
                behavior
                    .session
                    .append_assistant_tool_calls("r".repeat(2_400), &[call], 0);
                behavior.session.append_tool_result(
                    "echo",
                    format!("call_{index}"),
                    r#"{"text":"ok"}"#,
                    0,
                );
            }
        };
        seed(&mut behavior, 0);
        let (request, _) = behavior.uncompressed_request_from_session(None, None);
        let pad = target - behavior.prompt_estimate(&request);
        seed(&mut behavior, pad);
        let (request, _) = behavior.uncompressed_request_from_session(None, None);
        assert_eq!(behavior.prompt_estimate(&request), target);
        let old_rule = (32_768.0 * axocoatl_token::COMPRESSION_TRIGGER_PCT) as usize;
        if target == 24_379 {
            assert!(target + 4_096 > old_rule, "1.1.0 left rounds out here");
        }

        let request = behavior.build_request_from_session(None, None, 0, 0).unwrap();
        assert_eq!(dropped_note(&request), None, "{target}");
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|message| message.role == MessageRole::Tool)
                .count(),
            34
        );
        behavior.ensure_request_fits_context(&request).unwrap();
    }
}

/// The fit is measured the way the provider counts: once a complete call
/// shows its tokenizer counts more than the local count, the same request
/// needs more room.
#[tokio::test]
async fn the_fit_follows_the_provider_s_own_count() {
    let provider = Arc::new(LongLoopLlm::new(0, 0, 0, 32_768));
    let behavior = DefaultAgentBehavior::new(provider, quarter_counter());
    let capabilities = ProviderCapabilities {
        max_context_tokens: 32_768,
        ..Default::default()
    };
    assert_eq!(DefaultAgentBehavior::context_fit_limit(&capabilities), 31_744);
    assert_eq!(behavior.counted_prompt_budget(&capabilities, 4_096), 27_648);
    // A small prompt is mostly template overhead and teaches nothing.
    behavior.observe_prompt_scale(500, 700, true);
    assert_eq!(behavior.counted_prompt_budget(&capabilities, 4_096), 27_648);
    behavior.observe_prompt_scale(20_000, 23_000, false);
    assert_eq!(behavior.counted_prompt_budget(&capabilities, 4_096), 27_648);
    behavior.observe_prompt_scale(20_000, 23_000, true);
    assert_eq!(behavior.counted_prompt_budget(&capabilities, 4_096), 24_041);
    // A provider that counts fewer (or caches) never loosens the fit.
    behavior.observe_prompt_scale(20_000, 12_000, true);
    assert_eq!(behavior.counted_prompt_budget(&capabilities, 4_096), 27_648);
}

/// 1.1.0 ran into its own token guard mid-loop and ended the activation
/// with no answer ("Token budget exceeded: used 619018, budget 600000").
#[tokio::test]
async fn a_nearly_spent_token_guard_ends_with_an_answer() {
    let mut provider = LongLoopLlm::new(usize::MAX, 40, 40, 0);
    provider.usage = Some(TokenUsageStats::new(500, 20));
    let provider = Arc::new(provider);
    let captured = provider.captured.clone();
    let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
        .with_tool_round_limit(128)
        .with_tool_executor(echo_executor());
    let mut config = test_config_with_budget(6_000);
    config.sampling.max_tokens = Some(200);
    behavior.on_start(&config).await.unwrap();

    let output = behavior
        .execute(AgentInput::text("keep going"))
        .await
        .expect("the last request asks for the answer instead of failing");
    assert_eq!(output.content, "final answer");
    let requests = captured.lock().unwrap();
    let (last, earlier) = requests.split_last().unwrap();
    assert!(earlier.len() >= 3 && earlier.iter().all(|request| !request.tools.is_empty()));
    assert!(last.tools.is_empty(), "the final request offers no tools");
    let note = last.messages.last().unwrap().text_content().unwrap();
    assert!(note.contains("tools are no longer available"), "{note}");
    assert!(note.contains("of its 6,000 tokens"), "{note}");
    assert!(behavior.tracker.as_ref().unwrap().total_used() <= 6_000);
}

/// The Session grant's tokens ran out mid-loop in the eval and the failure
/// read "LLM provider error: Invalid request for ollama: provider admission
/// failed: ...". Seen coming, the last affordable call asks for the answer.
#[tokio::test]
async fn a_nearly_spent_session_budget_ends_with_an_answer() {
    let mut provider = LongLoopLlm::new(usize::MAX, 40, 40, 0);
    provider.grant = Some(std::sync::Mutex::new(FakeGrant {
        allowance: axocoatl_llm::ProviderAllowance {
            tokens: Some(10_000),
            ..Default::default()
        },
        reservation: 2_000,
    }));
    let provider = Arc::new(provider);
    let captured = provider.captured.clone();
    let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
        .with_tool_round_limit(128)
        .with_tool_executor(echo_executor());
    behavior.on_start(&AgentConfig::default()).await.unwrap();

    let output = behavior.execute(AgentInput::text("keep going")).await.unwrap();
    assert_eq!(output.content, "final answer");
    {
        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 5, "four tool rounds, then the answer");
        assert!(requests[..4].iter().all(|request| !request.tools.is_empty()));
        let note = requests[4].messages.last().unwrap().text_content().unwrap();
        assert!(requests[4].tools.is_empty());
        assert!(
            note.contains(
                "the Session budget has 2,000 tokens left and each model call reserves 2,000"
            ),
            "{note}"
        );
    }

    // Too few invocations for a tool round: the first request answers.
    let mut provider = LongLoopLlm::new(usize::MAX, 40, 40, 0);
    provider.grant = Some(std::sync::Mutex::new(FakeGrant {
        allowance: axocoatl_llm::ProviderAllowance {
            invocations: Some(2),
            ..Default::default()
        },
        reservation: 1,
    }));
    let provider = Arc::new(provider);
    let captured = provider.captured.clone();
    let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
        .with_tool_executor(echo_executor());
    behavior.on_start(&AgentConfig::default()).await.unwrap();
    let output = behavior.execute(AgentInput::text("keep going")).await.unwrap();
    assert_eq!(output.content, "final answer");
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty());
    assert!(requests[0]
        .messages
        .last()
        .unwrap()
        .text_content()
        .unwrap()
        .contains("allows only 2 more model or tool call(s)"));
}

/// When the Session budget cannot admit even the final call, the failure
/// names the limit in the host's words, not as a provider fault.
#[tokio::test]
async fn an_exhausted_session_budget_fails_in_plain_words() {
    let mut provider = LongLoopLlm::new(usize::MAX, 40, 40, 0);
    provider.grant = Some(std::sync::Mutex::new(FakeGrant {
        allowance: axocoatl_llm::ProviderAllowance {
            tokens: Some(1_000),
            ..Default::default()
        },
        reservation: 2_000,
    }));
    let mut behavior = DefaultAgentBehavior::new(Arc::new(provider), simple_counter())
        .with_tool_executor(echo_executor());
    behavior.on_start(&AgentConfig::default()).await.unwrap();

    let error = behavior
        .execute(AgentInput::text("keep going"))
        .await
        .unwrap_err();
    assert!(matches!(error, AgentError::BudgetExhausted(_)));
    let text = error.to_string();
    assert!(
        text.starts_with("The Session budget for this Agent is used up: "),
        "{text}"
    );
    assert!(!text.contains("LLM provider error") && !text.contains("Invalid request"));
}
/// Seed turn 1 of a Session as the eval's lead left it: the person's request,
/// `rounds` tool rounds that each read `output_chars` of a file, and the
/// final answer.
fn seed_completed_turn(
    behavior: &mut DefaultAgentBehavior,
    request: &str,
    answer: &str,
    rounds: usize,
    output_chars: usize,
) {
    behavior.session.append(MessageRole::User, request, 0);
    for index in 0..rounds {
        let id = format!("{request}-{index}");
        let call = ToolCall {
            id: id.clone(),
            name: "echo".into(),
            arguments: serde_json::json!({"text": format!("src/file_{index}.rs")}),
            provider_metadata: Default::default(),
        };
        behavior.session.append_assistant_tool_calls("", &[call], 0);
        behavior
            .session
            .append_tool_result("echo", id, prose(output_chars), 0);
    }
    behavior.session.append(MessageRole::Assistant, answer, 0);
}

/// N3 from the 1.1.0 live eval: turn 1 left the lead 33,510 tokens of
/// history on a 32,768-token model. At the start of turn 2 the daemon logged
/// "Compacted session context tokens_before=33510 tokens_after=5860" and the
/// lead's first request held only its instructions and the new request — no
/// trace of turn 1's request or answer. A smaller turn 1 was carried whole,
/// so whether an Agent remembered depended on how much work it had done.
#[tokio::test]
async fn a_large_first_turn_is_remembered_at_the_start_of_the_next() {
    let provider = Arc::new(LongLoopLlm::new(0, 0, 0, 32_768));
    let captured = provider.captured.clone();
    let mut behavior = DefaultAgentBehavior::new(provider, quarter_counter())
        .with_tool_executor(echo_executor())
        .with_stale_tool_result_masking(3, []);
    behavior.on_start(&answer_limit(4_096)).await.unwrap();
    seed_completed_turn(
        &mut behavior,
        "TURN_ONE_REQUEST fix the parser",
        "TURN_ONE_ANSWER the parser is fixed in src/parser.rs",
        39,
        3_300,
    );
    let history = quarter_counter().count_messages(&behavior.session.as_chat_messages());
    assert!((33_000..34_000).contains(&history), "{history}");

    let output = behavior
        .execute(AgentInput::text("TURN_TWO_REQUEST now add tests"))
        .await
        .unwrap();
    assert_eq!(output.content, "final answer");

    let requests = captured.lock().unwrap();
    let first = &requests[0];
    let has = |role: MessageRole, needle: &str| {
        first.messages.iter().any(|message| {
            message.role == role
                && message
                    .text_content()
                    .is_some_and(|text| text.contains(needle))
        })
    };
    assert!(
        has(MessageRole::User, "TURN_ONE_REQUEST"),
        "turn 1's request"
    );
    assert!(
        has(MessageRole::Assistant, "TURN_ONE_ANSWER"),
        "turn 1's answer"
    );
    assert!(has(MessageRole::User, "TURN_TWO_REQUEST"));
    behavior.ensure_request_fits_context(first).unwrap();
    // The Agent's own memory (what the next turn and a restart start from)
    // was compacted, and keeps both too.
    let session = behavior.session.as_chat_messages();
    assert!(quarter_counter().count_messages(&session) < history / 2);
    assert!(session
        .iter()
        .any(|message| message.role == MessageRole::User
            && message.text_content() == Some("TURN_ONE_REQUEST fix the parser")));
    assert!(session
        .iter()
        .any(|message| message.role == MessageRole::Assistant
            && message.text_content().is_some_and(
                |text| text.ends_with("TURN_ONE_ANSWER the parser is fixed in src/parser.rs")
            )));
}
