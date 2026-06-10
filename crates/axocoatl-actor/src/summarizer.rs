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
use axocoatl_token::Summarizer;

/// Summarizes text and conversations by calling an LLM provider.
pub struct LlmSummarizer {
    provider: Arc<dyn LlmProvider>,
}

impl LlmSummarizer {
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self { provider }
    }

    async fn summarize(&self, system: &str, user: String) -> Result<String, String> {
        let request = ChatRequest::with_system(system, user);
        let response = self
            .provider
            .chat(request)
            .await
            .map_err(|e| format!("summarizer LLM call failed: {e}"))?;
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
                usage: TokenUsageStats::default(),
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
        LlmSummarizer::new(Arc::new(StubLlm {
            content: content.to_string(),
            ok,
        }))
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
}
