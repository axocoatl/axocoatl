//! LLM-backed summarizer for the context-compression pipeline.
//!
//! Implements [`Summarizer`](axocoatl_token::Summarizer) over an
//! [`LlmProvider`]. The 5-stage compression pipeline's LLM stages — microcompact
//! (summarize an oversized tool result) and autocompact (summarize a whole
//! conversation) — call into this so that, under context or budget pressure, old
//! context is *summarized* rather than silently snipped away.

use std::sync::Arc;

use async_trait::async_trait;
use axocoatl_core::ChatMessage;
use axocoatl_llm::{ChatRequest, LlmProvider};
use axocoatl_token::{Summarizer, TokenTracker};

/// Summarizes text and conversations by calling an LLM provider. When a token
/// tracker is supplied, the summarization's own token usage is recorded against
/// the agent's budget — summarization is real spend, not free housekeeping.
pub struct LlmSummarizer {
    provider: Arc<dyn LlmProvider>,
    tracker: Option<TokenTracker>,
    /// The agent's configured model, sent as the per-request override so a shared
    /// OpenAI-compatible provider summarizes with the agent's model instead of the
    /// provider's hardcoded default. `None` falls back to that default.
    model: Option<String>,
}

impl LlmSummarizer {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tracker: Option<TokenTracker>,
        model: Option<String>,
    ) -> Self {
        Self {
            provider,
            tracker,
            model,
        }
    }

    async fn summarize(&self, system: &str, user: String) -> Result<String, String> {
        let mut request = ChatRequest::with_system(system, user);
        request.model_override = self.model.clone();
        let response = self
            .provider
            .chat(request)
            .await
            .map_err(|e| format!("summarizer LLM call failed: {e}"))?;
        // Count the summarization's tokens against the budget (best-effort: an
        // over-budget result is surfaced by the next pre-flight check, not here).
        if let Some(tracker) = &self.tracker {
            let _ = tracker.record_usage(response.usage.input_tokens, response.usage.output_tokens);
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
        let transcript = messages
            .iter()
            .map(|m| format!("{:?}: {}", m.role, m.text_content().unwrap_or("")))
            .collect::<Vec<_>>()
            .join("\n");
        let user = format!("Conversation to summarize:\n{transcript}");
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

    #[async_trait]
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
                per_call: 1000,
                per_execution: 1000,
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
            None,
        );
        s.summarize_tool_result("grep", "lots").await.unwrap();
        assert_eq!(tracker.total_used(), 42);
    }
}
