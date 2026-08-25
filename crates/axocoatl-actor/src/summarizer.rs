//! LLM-backed summarizer for the context-compression pipeline.
//!
//! Implements [`Summarizer`](axocoatl_token::Summarizer) over an
//! [`LlmProvider`]. The 5-stage compression pipeline's LLM stages — microcompact
//! (summarize an oversized tool result) and autocompact (summarize a whole
//! conversation) — call into this so that, under context or budget pressure, old
//! context is *summarized* rather than silently snipped away.

use std::sync::Arc;

use async_trait::async_trait;
use axocoatl_core::{ChatMessage, OverflowPolicy, TokenUsageStats};
use axocoatl_llm::{ChatRequest, LlmProvider};
use axocoatl_token::{BudgetError, Summarizer, TokenCounter, TokenTracker};

const SUMMARY_MAX_OUTPUT_TOKENS: usize = 1_024;

/// Summarizes text and conversations by calling an LLM provider. When a token
/// tracker is supplied, the summarization's own token usage is recorded against
/// the agent's budget — summarization is real spend, not free housekeeping.
pub struct LlmSummarizer {
    provider: Arc<dyn LlmProvider>,
    tracker: Option<TokenTracker>,
    counter: Arc<dyn TokenCounter>,
    usage: std::sync::Mutex<TokenUsageStats>,
    usage_states: Vec<crate::behavior::ExecutionUsageState>,
    /// The agent's configured model, sent as the per-request override so a shared
    /// OpenAI-compatible provider summarizes with the agent's model instead of the
    /// provider's hardcoded default. `None` falls back to that default.
    model: Option<String>,
}

impl LlmSummarizer {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tracker: Option<TokenTracker>,
        counter: Arc<dyn TokenCounter>,
        model: Option<String>,
    ) -> Self {
        Self {
            provider,
            tracker,
            counter,
            usage: std::sync::Mutex::new(TokenUsageStats::default()),
            usage_states: Vec::new(),
            model,
        }
    }

    pub(crate) fn with_usage_state(
        mut self,
        usage_state: crate::behavior::ExecutionUsageState,
    ) -> Self {
        self.usage_states.push(usage_state);
        self
    }

    pub fn usage_snapshot(&self) -> TokenUsageStats {
        self.usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn summarize(&self, system: &str, user: String) -> Result<String, String> {
        let mut request = ChatRequest::with_system(system, user);
        request.model_override = self.model.clone();
        request.max_tokens = Some(SUMMARY_MAX_OUTPUT_TOKENS);
        let estimated_input = self.provider.count_tokens(&request);
        if let Some(tracker) = &self.tracker {
            let requested = estimated_input.saturating_add(SUMMARY_MAX_OUTPUT_TOKENS);
            if let Err(BudgetError::WouldExceedBudget {
                current,
                requested,
                budget,
            }) = tracker.check_headroom(requested)
            {
                match tracker.budget().overflow_policy {
                    OverflowPolicy::Abort => {
                        return Err(format!(
                            "summarizer token budget exceeded before provider call: {current} + {requested} > {budget}"
                        ));
                    }
                    OverflowPolicy::Warn => {
                        tracing::warn!(
                            current,
                            requested,
                            budget,
                            "summarizer would exceed token budget, continuing (warn policy)"
                        );
                    }
                }
            }
        }
        for usage_state in &self.usage_states {
            usage_state.begin_provider_call();
        }
        let mut response = self
            .provider
            .chat(request)
            .await
            .map_err(|e| format!("summarizer LLM call failed: {e}"))?;
        if response.usage.total() == 0 {
            let output = self.counter.count_text(&response.content);
            response.usage = axocoatl_core::TokenUsageStats::new(estimated_input, output);
        }
        self.usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .merge(&response.usage);
        for usage_state in &self.usage_states {
            usage_state.record_provider_response(&response.usage);
        }
        if let Some(tracker) = &self.tracker {
            let reported_total = response.usage.total();
            let tracked_output = response
                .usage
                .output_tokens
                .saturating_add(response.usage.reasoning_tokens.unwrap_or(0));
            let per_call_overrun = reported_total > tracker.budget().per_call;
            let recorded = tracker.record_usage(response.usage.input_tokens, tracked_output);
            match tracker.budget().overflow_policy {
                OverflowPolicy::Abort if per_call_overrun => {
                    return Err(format!(
                        "summarizer provider-reported call usage exceeded token budget: {reported_total} > {}",
                        tracker.budget().per_call
                    ));
                }
                OverflowPolicy::Abort => {
                    if let Err(error) = recorded {
                        return Err(format!(
                            "summarizer provider-reported usage exceeded token budget: {error}"
                        ));
                    }
                }
                OverflowPolicy::Warn => {
                    if per_call_overrun {
                        tracing::warn!(
                            reported_total,
                            budget = tracker.budget().per_call,
                            "summarizer provider-reported call usage exceeded token budget (warn policy)"
                        );
                    }
                    if let Err(error) = recorded {
                        tracing::warn!(error = %error, "summarizer provider-reported usage exceeded execution token budget (warn policy)");
                    }
                }
            }
        }
        let summary = response.content.trim().to_string();
        if summary.is_empty() {
            return Err("summarizer returned an empty summary".to_string());
        }
        Ok(summary)
    }
}

#[async_trait]
impl Summarizer for LlmSummarizer {
    async fn summarize_tool_result(&self, tool_name: &str, result: &str) -> Result<String, String> {
        let system = "You compress tool output for an AI agent's working context. Preserve every \
                      concrete datum (identifiers, numbers, file paths, statuses, error messages); \
                      drop only redundancy and formatting. Return ONLY the summary, no preamble.";
        let user = format!("Tool: {tool_name}\n\nOutput to compress:\n{result}");
        self.summarize(system, user).await
    }

    async fn summarize_conversation(&self, messages: &[ChatMessage]) -> Result<String, String> {
        let system = "You compress a conversation transcript into a compact summary for an AI \
                      agent's working memory. Preserve the goals, constraints, decisions, key \
                      facts, and open questions; write in third person, past tense. Return ONLY \
                      the summary, no preamble.";
        // Preserve the completed provider transaction's structure. A role/text
        // projection loses assistant tool calls, ids, arguments, result
        // correlation, and opaque replay metadata (for example thought
        // signatures), which can make the resulting summary contradict the
        // actual route even though the live suffix itself remains intact.
        let transcript = serde_json::to_string(messages)
            .map_err(|error| format!("failed to serialize conversation for summary: {error}"))?;
        let user = format!("Conversation JSON to summarize:\n{transcript}");
        self.summarize(system, user).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_core::{MessageRole, TokenUsageStats};
    use axocoatl_llm::{
        ChatResponse, FinishReason, ProviderCapabilities, ProviderError, StreamEvent,
    };
    use std::pin::Pin;
    use tokio_stream::Stream;

    /// Provider whose `chat` returns a fixed content (or errors when `ok` is false).
    struct StubLlm {
        content: String,
        ok: bool,
        usage: TokenUsageStats,
    }

    #[async_trait::async_trait]
    impl LlmProvider for StubLlm {
        fn provider_id(&self) -> &str {
            "stub"
        }
        fn model_id(&self) -> &str {
            "stub-model"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        fn count_tokens(&self, _: &ChatRequest) -> usize {
            0
        }
        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            if !self.ok {
                return Err(ProviderError::ApiError {
                    provider: "stub".to_string(),
                    status: 500,
                    message: "boom".to_string(),
                });
            }
            Ok(ChatResponse {
                content: self.content.clone(),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: self.usage.clone(),
                model: "stub-model".to_string(),
                provider: "stub".to_string(),
            })
        }
        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            unimplemented!("summarizer never streams")
        }
    }

    fn summarizer(content: &str, ok: bool) -> LlmSummarizer {
        LlmSummarizer::new(
            Arc::new(StubLlm {
                content: content.to_string(),
                ok,
                usage: TokenUsageStats::default(),
            }),
            None,
            Arc::new(axocoatl_token::ApproximateCounter::new().unwrap()),
            None,
        )
    }

    #[tokio::test]
    async fn summarize_tool_result_returns_trimmed_summary() {
        let s = summarizer("  a compact summary  ", true);
        let out = s
            .summarize_tool_result("grep", "lots of lines")
            .await
            .unwrap();
        assert_eq!(out, "a compact summary");
    }

    #[tokio::test]
    async fn summarize_conversation_returns_summary() {
        let s = summarizer("the gist", true);
        let msgs = vec![
            ChatMessage::user("what is 2+2?"),
            ChatMessage::assistant("4"),
        ];
        assert_eq!(s.summarize_conversation(&msgs).await.unwrap(), "the gist");
    }

    struct CapturingSummaryLlm {
        request: Arc<std::sync::Mutex<Option<ChatRequest>>>,
    }

    #[async_trait]
    impl LlmProvider for CapturingSummaryLlm {
        fn provider_id(&self) -> &str {
            "capture-summary"
        }

        fn model_id(&self) -> &str {
            "capture-summary-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
            *self.request.lock().unwrap() = Some(request);
            Ok(ChatResponse {
                content: "summary".to_string(),
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
                usage: TokenUsageStats::default(),
                model: "capture-summary-model".to_string(),
                provider: "capture-summary".to_string(),
            })
        }

        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            unreachable!("summarizer never streams")
        }
    }

    #[tokio::test]
    async fn conversation_summary_input_retains_tool_arguments_ids_and_provider_metadata() {
        let captured = Arc::new(std::sync::Mutex::new(None));
        let summarizer = LlmSummarizer::new(
            Arc::new(CapturingSummaryLlm {
                request: captured.clone(),
            }),
            None,
            Arc::new(axocoatl_token::ApproximateCounter::new().unwrap()),
            None,
        );
        let call = axocoatl_core::ToolCall {
            id: "call-structured".to_string(),
            name: "repo_read".to_string(),
            arguments: serde_json::json!({"path": "/structured"}),
            provider_metadata: axocoatl_core::ProviderMetadata::from([(
                "gemini.thought_signature".to_string(),
                "opaque-structured-signature".to_string(),
            )]),
        };
        let messages = vec![
            ChatMessage::user("inspect"),
            ChatMessage::assistant_with_tool_calls("", vec![call.clone()]),
            ChatMessage::tool_result("ok", &call.name, &call.id),
        ];

        summarizer.summarize_conversation(&messages).await.unwrap();

        let request = captured.lock().unwrap();
        let input = request
            .as_ref()
            .unwrap()
            .messages
            .last()
            .unwrap()
            .text_content()
            .unwrap();
        assert!(input.contains("call-structured"));
        assert!(input.contains("/structured"));
        assert!(input.contains("opaque-structured-signature"));
    }

    #[tokio::test]
    async fn abort_budget_with_one_token_left_skips_summarizer_provider_call() {
        use axocoatl_core::TokenBudget;

        let captured = Arc::new(std::sync::Mutex::new(None));
        let tracker = TokenTracker::new(
            TokenBudget {
                per_call: 10,
                per_execution: 10,
                overflow_policy: OverflowPolicy::Abort,
            },
            Arc::new(axocoatl_token::ApproximateCounter::new().unwrap()),
        );
        tracker.record_usage(9, 0).unwrap();
        let summarizer = LlmSummarizer::new(
            Arc::new(CapturingSummaryLlm {
                request: captured.clone(),
            }),
            Some(tracker),
            Arc::new(axocoatl_token::ApproximateCounter::new().unwrap()),
            None,
        );

        let error = summarizer
            .summarize_tool_result("repo_read", "large result")
            .await
            .unwrap_err();

        assert!(error.contains("before provider call"));
        assert!(captured.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn empty_response_is_error() {
        let s = summarizer("   ", true);
        assert!(s.summarize_tool_result("t", "x").await.is_err());
    }

    #[tokio::test]
    async fn provider_error_propagates() {
        let s = summarizer("ignored", false);
        let err = s
            .summarize_conversation(&[ChatMessage {
                role: MessageRole::User,
                content: axocoatl_core::MessageContent::Text("hi".to_string()),
                name: None,
                tool_calls: vec![],
                tool_call_id: None,
            }])
            .await
            .unwrap_err();
        assert!(err.contains("summarizer LLM call failed"));
    }

    #[tokio::test]
    async fn records_summarization_usage_against_tracker() {
        use axocoatl_core::TokenBudget;
        use axocoatl_token::{TokenCounter, TokenTracker};

        struct ZeroCounter;
        impl TokenCounter for ZeroCounter {
            fn count_text(&self, _: &str) -> usize {
                0
            }
            fn count_messages(&self, _: &[ChatMessage]) -> usize {
                0
            }
            fn count_tool_definition(&self, _: &serde_json::Value) -> usize {
                0
            }
        }

        let tracker = TokenTracker::new(
            TokenBudget {
                per_call: 10_000,
                per_execution: 10_000,
                overflow_policy: Default::default(),
            },
            Arc::new(ZeroCounter),
        );
        let s = LlmSummarizer::new(
            Arc::new(StubLlm {
                content: "summary".to_string(),
                ok: true,
                usage: TokenUsageStats::new(30, 12),
            }),
            Some(tracker.clone()),
            Arc::new(ZeroCounter),
            None,
        );
        s.summarize_tool_result("grep", "lots").await.unwrap();
        assert_eq!(tracker.total_used(), 42);
    }

    #[tokio::test]
    async fn provider_reported_summarizer_overrun_fails_current_compaction_call() {
        use axocoatl_core::TokenBudget;

        struct ZeroCounter;
        impl axocoatl_token::TokenCounter for ZeroCounter {
            fn count_text(&self, _: &str) -> usize {
                0
            }
            fn count_messages(&self, _: &[ChatMessage]) -> usize {
                0
            }
            fn count_tool_definition(&self, _: &serde_json::Value) -> usize {
                0
            }
        }

        let tracker = TokenTracker::new(
            TokenBudget {
                per_call: SUMMARY_MAX_OUTPUT_TOKENS,
                per_execution: SUMMARY_MAX_OUTPUT_TOKENS,
                overflow_policy: OverflowPolicy::Abort,
            },
            Arc::new(ZeroCounter),
        );
        let summarizer = LlmSummarizer::new(
            Arc::new(StubLlm {
                content: "summary".to_string(),
                ok: true,
                usage: TokenUsageStats::new(900, 200),
            }),
            Some(tracker.clone()),
            Arc::new(ZeroCounter),
            None,
        );

        let error = summarizer
            .summarize_tool_result("repo_read", "large result")
            .await
            .unwrap_err();
        assert!(error.contains("provider-reported call usage exceeded"));
        assert_eq!(tracker.total_used(), 1_100);
    }

    #[tokio::test]
    async fn hostile_reasoning_usage_cannot_bypass_summarizer_abort_budget() {
        use axocoatl_core::TokenBudget;

        struct ZeroCounter;
        impl axocoatl_token::TokenCounter for ZeroCounter {
            fn count_text(&self, _: &str) -> usize {
                0
            }
            fn count_messages(&self, _: &[ChatMessage]) -> usize {
                0
            }
            fn count_tool_definition(&self, _: &serde_json::Value) -> usize {
                0
            }
        }

        let tracker = TokenTracker::new(
            TokenBudget {
                per_call: SUMMARY_MAX_OUTPUT_TOKENS,
                per_execution: SUMMARY_MAX_OUTPUT_TOKENS,
                overflow_policy: OverflowPolicy::Abort,
            },
            Arc::new(ZeroCounter),
        );
        let summarizer = LlmSummarizer::new(
            Arc::new(StubLlm {
                content: "summary".to_string(),
                ok: true,
                usage: TokenUsageStats::new(0, 0).with_reasoning(usize::MAX),
            }),
            Some(tracker.clone()),
            Arc::new(ZeroCounter),
            None,
        );

        let error = summarizer
            .summarize_tool_result("repo_read", "large result")
            .await
            .unwrap_err();
        assert!(error.contains("provider-reported call usage exceeded"));
        assert_eq!(tracker.total_used(), usize::MAX);
    }
}
