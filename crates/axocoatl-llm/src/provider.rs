use std::pin::Pin;

use serde::{Deserialize, Serialize};
use tokio_stream::Stream;

use axocoatl_core::{ChatMessage, MessageContent, TokenUsageStats};

use crate::error::ProviderError;
use crate::tools::{ToolCall, ToolDefinition};

/// The core LLM provider trait — all providers implement this.
///
/// Uses `async_trait` because provider implementations need dynamic dispatch
/// (`Arc<dyn LlmProvider>`) throughout the framework.
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync + 'static {
    /// Provider identifier (e.g., "openai", "anthropic", "ollama").
    fn provider_id(&self) -> &str;

    /// Model identifier being used.
    fn model_id(&self) -> &str;

    /// Capabilities this provider/model supports.
    fn capabilities(&self) -> ProviderCapabilities;

    /// Non-streaming chat completion.
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError>;

    /// Streaming chat completion.
    /// Returns a stream of events — caller consumes until `StreamEvent::Done`.
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>;

    /// Count tokens for a request (for budget pre-checking).
    /// Default implementation uses a rough byte-based approximation.
    fn count_tokens(&self, request: &ChatRequest) -> usize {
        request
            .messages
            .iter()
            .map(|m| match &m.content {
                MessageContent::Text(s) => s.len() / 4,
                MessageContent::Parts(parts) => parts
                    .iter()
                    .map(|p| match p {
                        axocoatl_core::ContentPart::Text(s) => s.len() / 4,
                        axocoatl_core::ContentPart::Image { .. } => 85, // ~85 tokens per image
                    })
                    .sum(),
            })
            .sum()
    }
}

/// What a specific provider+model combination can do.
#[derive(Debug, Clone, Default)]
pub struct ProviderCapabilities {
    pub streaming: bool,
    pub tool_calling: bool,
    pub structured_output: bool,
    pub vision: bool,
    pub reasoning: bool,
    pub embeddings: bool,
    pub max_context_tokens: usize,
    pub max_output_tokens: usize,
}

/// A chat completion request — universal across all providers.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: Option<usize>,
    pub temperature: Option<f32>,
    /// Nucleus sampling cutoff. `None` → provider default.
    pub top_p: Option<f32>,
    /// Requested output format. `Some(Json)` selects the provider's native JSON
    /// mode (or a prompt-enforced fallback where there is none).
    pub response_format: Option<axocoatl_core::ResponseFormat>,
    pub stop_sequences: Vec<String>,
    /// Provider-specific parameters (escape hatch — zero overhead when unused).
    pub provider_options: Option<serde_json::Value>,
    /// Per-call model override. When `Some`, the provider should use this
    /// model id instead of its configured default for this single request.
    /// Used by the Chat tab's `model_override` feature; the provider, base
    /// URL, and credentials stay the same.
    pub model_override: Option<String>,
}

impl ChatRequest {
    /// Create a simple request with a single user message.
    pub fn simple(user_message: impl Into<String>) -> Self {
        Self {
            messages: vec![ChatMessage::user(user_message)],
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            response_format: None,
            stop_sequences: Vec::new(),
            provider_options: None,
            model_override: None,
        }
    }

    /// Create a request with a system prompt and user message.
    pub fn with_system(system: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            messages: vec![ChatMessage::system(system), ChatMessage::user(user)],
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            response_format: None,
            stop_sequences: Vec::new(),
            provider_options: None,
            model_override: None,
        }
    }
}

/// A non-streaming response.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
    pub usage: TokenUsageStats,
    pub model: String,
    pub provider: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FinishReason {
    Stop,
    ToolUse,
    MaxTokens,
    ContentFilter,
    Error,
}

/// Streaming events — all providers normalized to this enum.
#[derive(Debug)]
pub enum StreamEvent {
    /// A chunk of assistant text.
    TextDelta { delta: String },
    /// A chunk of reasoning/thinking text (extended-thinking models).
    ReasoningDelta { delta: String },
    /// A tool call being streamed.
    ToolCallDelta {
        /// Provider stream index for this tool call. OpenAI-compatible APIs
        /// (OpenAI, Mistral, OpenRouter, Ollama) send the `id` only on the
        /// first chunk and stream subsequent argument fragments keyed by
        /// `index`, so accumulation must correlate by index when present.
        index: Option<usize>,
        id: String,
        name: Option<String>,
        args_delta: String,
    },
    /// Final usage statistics (emitted before Done).
    Usage(TokenUsageStats),
    /// Stream complete.
    Done { finish_reason: FinishReason },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_request_simple() {
        let req = ChatRequest::simple("hello");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].text_content(), Some("hello"));
    }

    #[test]
    fn chat_request_with_system() {
        let req = ChatRequest::with_system("You are helpful.", "Hi");
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].text_content(), Some("You are helpful."));
        assert_eq!(req.messages[1].text_content(), Some("Hi"));
    }

    #[test]
    fn provider_capabilities_default() {
        let caps = ProviderCapabilities::default();
        assert!(!caps.streaming);
        assert!(!caps.tool_calling);
        assert_eq!(caps.max_context_tokens, 0);
    }

    #[test]
    fn finish_reason_serde_roundtrip() {
        let reasons = vec![
            FinishReason::Stop,
            FinishReason::ToolUse,
            FinishReason::MaxTokens,
            FinishReason::ContentFilter,
            FinishReason::Error,
        ];
        for reason in reasons {
            let json = serde_json::to_string(&reason).unwrap();
            let back: FinishReason = serde_json::from_str(&json).unwrap();
            assert_eq!(back, reason);
        }
    }

    #[test]
    fn default_count_tokens_approximation() {
        // Test the default trait implementation via a concrete struct
        struct DummyProvider;

        #[async_trait::async_trait]
        impl LlmProvider for DummyProvider {
            fn provider_id(&self) -> &str {
                "dummy"
            }
            fn model_id(&self) -> &str {
                "dummy"
            }
            fn capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities::default()
            }
            async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
                unimplemented!()
            }
            async fn chat_stream(
                &self,
                _: ChatRequest,
            ) -> Result<
                Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
                ProviderError,
            > {
                unimplemented!()
            }
        }

        let provider = DummyProvider;
        let req = ChatRequest::simple("hello world test");
        let count = provider.count_tokens(&req);
        assert!(count > 0);
        // "hello world test" = 16 chars / 4 = 4
        assert_eq!(count, 4);
    }
}
