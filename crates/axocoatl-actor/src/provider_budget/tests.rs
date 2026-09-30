use super::*;
use axocoatl_core::TokenBudget;
use axocoatl_llm::{FinishReason, ProviderCapabilities, ProviderError, ProviderExecutionBounds};
use std::sync::{Arc, Mutex};

struct Counter;
impl TokenCounter for Counter {
    fn count_text(&self, text: &str) -> usize {
        text.len() / 4 + 1
    }
    fn count_messages(&self, messages: &[ChatMessage]) -> usize {
        messages
            .iter()
            .map(|message| {
                message
                    .text_content()
                    .map_or(1, |text| self.count_text(text))
            })
            .sum()
    }
    fn count_tool_definition(&self, value: &serde_json::Value) -> usize {
        self.count_text(&value.to_string())
    }
}

struct ContextProbe {
    context: usize,
    output: usize,
    whole_call_capacity: Option<u64>,
    observed: Mutex<Vec<ChatRequest>>,
    dispatched: Mutex<usize>,
}
impl ContextProbe {
    fn new(context: usize, output: usize) -> Self {
        Self {
            context,
            output,
            whole_call_capacity: None,
            observed: Mutex::new(Vec::new()),
            dispatched: Mutex::new(0),
        }
    }
}
#[async_trait::async_trait]
impl LlmProvider for ContextProbe {
    fn provider_id(&self) -> &str {
        "context-order-fixture"
    }
    fn model_id(&self) -> &str {
        "model"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            max_context_tokens: self.context,
            max_output_tokens: self.output,
            ..Default::default()
        }
    }
    fn count_tokens(&self, request: &ChatRequest) -> usize {
        Counter.count_messages(&request.messages)
    }
    fn execution_bounds(&self, request: &ChatRequest) -> Option<ProviderExecutionBounds> {
        // This fixture preserves the native context + prediction / JSON-pass
        // algebra. It is not a replacement for the daemon's real durable gate.
        let passes = if request.response_format == Some(axocoatl_core::ResponseFormat::Json) {
            2
        } else {
            1
        };
        Some(ProviderExecutionBounds {
            token_limit: (self.context as u64 + request.max_tokens.unwrap_or(self.output) as u64)
                * passes,
            cost_microunits: 0,
            response_bytes: 1024 * 1024,
        })
    }
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        self.observed.lock().unwrap().push(request.clone());
        if self
            .whole_call_capacity
            .is_some_and(|capacity| self.execution_bounds(&request).unwrap().token_limit > capacity)
        {
            return Err(ProviderError::InvalidRequest {
                provider: self.provider_id().into(),
                message: "whole-call context/prediction reservation exceeds capacity".into(),
            });
        }
        *self.dispatched.lock().unwrap() += 1;
        Ok(ChatResponse {
            content: "complete".into(),
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: TokenUsageStats::new(1, 1),
            model: self.model_id().into(),
            provider: self.provider_id().into(),
        })
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<
        std::pin::Pin<
            Box<
                dyn tokio_stream::Stream<Item = Result<axocoatl_llm::StreamEvent, ProviderError>>
                    + Send,
            >,
        >,
        ProviderError,
    > {
        Err(ProviderError::InvalidRequest {
            provider: self.provider_id().into(),
            message: "Coordinator fixture supports non-streaming calls only".into(),
        })
    }
}
fn make_tracker(
    per_call: usize,
    per_execution: usize,
    overflow_policy: OverflowPolicy,
) -> TokenTracker {
    TokenTracker::new(
        TokenBudget {
            per_call,
            per_execution,
            overflow_policy,
        },
        Arc::new(Counter),
    )
}

#[tokio::test]
async fn coordinator_uses_existing_abort_allowance_before_context_projection() {
    let provider = ContextProbe::new(1000, 900);
    let tracker = make_tracker(900, 900, OverflowPolicy::Abort);
    let request = ChatRequest::simple("p".repeat(500));
    let estimated = provider.count_tokens(&request);
    assert!(estimated + provider.output > provider.context);
    let result = chat(&provider, &Counter, Some(&tracker), None, request, 0, None)
        .await
        .unwrap();
    assert!(matches!(result, ControlledChat::Response(_)));
    let captured = provider.observed.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].max_tokens, Some(900 - estimated));
    assert_eq!(*provider.dispatched.lock().unwrap(), 1);
}

#[tokio::test]
async fn coordinator_compressible_history_is_projected_before_rejecting_spend() {
    let provider = ContextProbe::new(1000, 900);
    let tracker = make_tracker(900, 900, OverflowPolicy::Abort);
    let mut request = ChatRequest::simple("CURRENT_EXACT");
    request.messages = vec![
        ChatMessage::system("required system"),
        ChatMessage::user("older ".repeat(900)),
        ChatMessage::assistant("previous answer"),
        ChatMessage::user("CURRENT_EXACT"),
    ];
    let original = format!("{request:?}");
    assert!(provider.count_tokens(&request) > 900);
    chat(
        &provider,
        &Counter,
        Some(&tracker),
        None,
        request.clone(),
        3,
        None,
    )
    .await
    .unwrap();
    let captured = provider.observed.lock().unwrap();
    let projected = &captured[0];
    assert_eq!(projected.messages.len(), 2);
    assert_eq!(
        projected.messages[0].text_content(),
        Some("required system")
    );
    assert_eq!(projected.messages[1].text_content(), Some("CURRENT_EXACT"));
    assert_eq!(
        projected.max_tokens,
        Some(900 - provider.count_tokens(projected))
    );
    assert_eq!(format!("{request:?}"), original);
}

#[tokio::test]
async fn coordinator_recomputes_remaining_execution_allowance_and_preserves_other_modes() {
    let provider = ContextProbe::new(1000, 900);
    let tracker = make_tracker(1000, 1000, OverflowPolicy::Abort);
    tracker.record_usage(150, 50).unwrap();
    let request = ChatRequest::simple("input ".repeat(40));
    let estimated = provider.count_tokens(&request);
    chat(&provider, &Counter, Some(&tracker), None, request, 0, None)
        .await
        .unwrap();
    assert_eq!(
        provider.observed.lock().unwrap()[0].max_tokens,
        Some(800 - estimated)
    );

    for mode in 0..3 {
        let provider = ContextProbe::new(2000, 900);
        let budget = make_tracker(
            1000,
            1000,
            if mode == 1 {
                OverflowPolicy::Warn
            } else {
                OverflowPolicy::Abort
            },
        );
        let mut request = ChatRequest::simple("preserved input");
        if mode == 0 {
            request.max_tokens = Some(12);
        }
        let budget = if mode == 2 { None } else { Some(&budget) };
        chat(&provider, &Counter, budget, None, request, 0, None)
            .await
            .unwrap();
        assert_eq!(
            provider.observed.lock().unwrap()[0].max_tokens,
            if mode == 0 { Some(12) } else { None }
        );
    }
}

#[tokio::test]
async fn zero_allowance_and_protected_oversize_refuse_before_provider_effects() {
    for input in ["tiny".into(), "huge ".repeat(2000)] {
        let provider = ContextProbe::new(1000, 900);
        let budget = make_tracker(0, 0, OverflowPolicy::Abort);
        let result = chat(
            &provider,
            &Counter,
            Some(&budget),
            None,
            ChatRequest::simple(input),
            0,
            None,
        )
        .await;
        assert!(result.is_err());
        assert!(provider.observed.lock().unwrap().is_empty());
        assert_eq!(*provider.dispatched.lock().unwrap(), 0);
    }
}

#[tokio::test]
async fn estimate_projection_never_shrinks_native_style_whole_call_reservation() {
    for json in [false, true] {
        let mut provider = ContextProbe::new(4096, 3000);
        provider.whole_call_capacity = Some(3000);
        let budget = make_tracker(3000, 3000, OverflowPolicy::Abort);
        let mut request = ChatRequest::simple("small input");
        if json {
            request.response_format = Some(axocoatl_core::ResponseFormat::Json);
        }
        let estimated = provider.count_tokens(&request);
        let result = chat(&provider, &Counter, Some(&budget), None, request, 0, None).await;
        assert!(
            matches!(result,Err(AgentError::Provider(ref error)) if error.contains("whole-call"))
        );
        let observed = provider.observed.lock().unwrap();
        assert_eq!(observed[0].max_tokens, Some(3000 - estimated));
        let actual_bound = provider.execution_bounds(&observed[0]).unwrap();
        assert_eq!(
            actual_bound.token_limit,
            (4096 + 3000 - estimated) as u64 * if json { 2 } else { 1 }
        );
        assert!(actual_bound.token_limit > 3000);
        assert_eq!(*provider.dispatched.lock().unwrap(), 0);
        assert_eq!(budget.total_used(), 0);
    }
}
