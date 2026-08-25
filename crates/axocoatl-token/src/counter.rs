use crate::error::TokenError;
use axocoatl_core::ChatMessage;

/// Provider-agnostic token counting.
/// Different providers have different tokenizers — this abstraction handles routing.
pub trait TokenCounter: Send + Sync {
    /// Count tokens in a plain text string.
    fn count_text(&self, text: &str) -> usize;

    /// Count tokens for a chat completion request (includes role/formatting overhead).
    fn count_messages(&self, messages: &[ChatMessage]) -> usize;

    /// Count tokens for a tool definition (serialized as JSON).
    fn count_tool_definition(&self, tool_json: &serde_json::Value) -> usize;
}

/// OpenAI / tiktoken-compatible counter.
/// Supports: gpt-4o (o200k_base), gpt-4 (cl100k_base), o1/o3 (o200k_base).
pub struct TiktokenCounter {
    bpe: tiktoken_rs::CoreBPE,
    _model: String,
}

impl TiktokenCounter {
    /// Create counter using o200k_base encoding (GPT-4o, o1, o3).
    pub fn o200k_base() -> Result<Self, TokenError> {
        let bpe = tiktoken_rs::o200k_base().map_err(|e| TokenError::InitFailed(e.to_string()))?;
        Ok(Self {
            bpe,
            _model: "gpt-4o".to_string(),
        })
    }

    /// Create counter using cl100k_base encoding (GPT-4, GPT-3.5).
    pub fn cl100k_base() -> Result<Self, TokenError> {
        let bpe = tiktoken_rs::cl100k_base().map_err(|e| TokenError::InitFailed(e.to_string()))?;
        Ok(Self {
            bpe,
            _model: "gpt-4".to_string(),
        })
    }

    /// Create counter for a specific model name.
    pub fn for_model(model: &str) -> Result<Self, TokenError> {
        let bpe = tiktoken_rs::get_bpe_from_model(model)
            .map_err(|_| TokenError::UnknownModel(model.to_string()))?;
        Ok(Self {
            bpe,
            _model: model.to_string(),
        })
    }
}

impl TokenCounter for TiktokenCounter {
    fn count_text(&self, text: &str) -> usize {
        self.bpe.encode_with_special_tokens(text).len()
    }

    fn count_messages(&self, messages: &[ChatMessage]) -> usize {
        // Each message adds 4 overhead tokens; reply primes +3
        let mut total = 3usize; // reply priming
        for msg in messages {
            total += 4; // per-message overhead
            match &msg.content {
                axocoatl_core::MessageContent::Text(text) => total += self.count_text(text),
                // Image bytes are transport encoding, not language-model tokens.
                // Count text exactly and reserve a conservative visual-token
                // allowance per image; serializing a 10 MiB data URL would
                // falsely reject an otherwise normal vision request.
                axocoatl_core::MessageContent::Parts(parts) => {
                    for part in parts {
                        match part {
                            axocoatl_core::ContentPart::Text(text) => {
                                total += self.count_text(text);
                            }
                            axocoatl_core::ContentPart::Image { .. } => {
                                // Covers common high-detail tiling costs with
                                // headroom without depending on encoded bytes.
                                total += 1_024;
                            }
                        }
                    }
                }
            }
            if let Some(name) = &msg.name {
                total += self.count_text(name);
                total -= 1; // name replaces role
            }
            if !msg.tool_calls.is_empty() {
                // Arguments and opaque provider replay metadata (for example a
                // Gemini thought signature) consume context even when the
                // assistant turn has no textual content.
                let serialized = serde_json::to_string(&msg.tool_calls).unwrap_or_default();
                total += self.count_text(&serialized);
            }
            if let Some(tool_call_id) = &msg.tool_call_id {
                total += self.count_text(tool_call_id);
            }
        }
        total
    }

    fn count_tool_definition(&self, tool_json: &serde_json::Value) -> usize {
        self.count_text(&serde_json::to_string(tool_json).unwrap_or_default())
    }
}

/// Approximate counter for non-OpenAI models (Anthropic, Gemini, Ollama).
/// Uses cl100k_base as approximation — within ~5% for most English text.
pub struct ApproximateCounter(TiktokenCounter);

impl ApproximateCounter {
    pub fn new() -> Result<Self, TokenError> {
        Ok(Self(TiktokenCounter::cl100k_base()?))
    }
}

impl TokenCounter for ApproximateCounter {
    fn count_text(&self, text: &str) -> usize {
        self.0.count_text(text)
    }

    fn count_messages(&self, messages: &[ChatMessage]) -> usize {
        self.0.count_messages(messages)
    }

    fn count_tool_definition(&self, tool_json: &serde_json::Value) -> usize {
        self.0.count_tool_definition(tool_json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn o200k_base_counts_known_string() {
        let counter = TiktokenCounter::o200k_base().unwrap();
        // "hello world" is a well-known test case
        let count = counter.count_text("hello world");
        assert!(count > 0);
        assert!(count <= 3); // typically 2 tokens
    }

    #[test]
    fn cl100k_base_counts_known_string() {
        let counter = TiktokenCounter::cl100k_base().unwrap();
        let count = counter.count_text("hello world");
        assert!(count > 0);
        assert!(count <= 3);
    }

    #[test]
    fn empty_string_is_zero_tokens() {
        let counter = TiktokenCounter::o200k_base().unwrap();
        assert_eq!(counter.count_text(""), 0);
    }

    #[test]
    fn count_messages_includes_overhead() {
        let counter = TiktokenCounter::o200k_base().unwrap();
        let messages = vec![ChatMessage::user("hi")];
        let count = counter.count_messages(&messages);
        // "hi" ~1 token + 4 overhead + 3 reply priming = ~8
        assert!(count >= 7);
    }

    #[test]
    fn count_messages_empty() {
        let counter = TiktokenCounter::o200k_base().unwrap();
        let count = counter.count_messages(&[]);
        assert_eq!(count, 3); // just reply priming
    }

    #[test]
    fn count_messages_includes_tool_arguments_ids_and_provider_metadata() {
        let counter = TiktokenCounter::o200k_base().unwrap();
        let plain = ChatMessage::assistant("");
        let mut metadata = axocoatl_core::ProviderMetadata::new();
        metadata.insert(
            "gemini.thought_signature".to_string(),
            "opaque-signature-with-real-token-cost".repeat(8),
        );
        let with_tool_call = ChatMessage::assistant_with_tool_calls(
            "",
            vec![axocoatl_core::ToolCall {
                id: "provider-call-id".to_string(),
                name: "repo_read".to_string(),
                arguments: serde_json::json!({
                    "path": "/a/long/provider-visible/path",
                    "line": 42
                }),
                provider_metadata: metadata,
            }],
        );

        assert!(counter.count_messages(&[with_tool_call]) > counter.count_messages(&[plain]) + 20);

        let tool_result = ChatMessage::tool_result("ok", "repo_read", "provider-call-id");
        assert!(
            counter.count_messages(&[tool_result])
                > counter.count_messages(&[ChatMessage::tool("ok")])
        );
    }

    #[test]
    fn multimodal_count_ignores_base64_transport_size_but_counts_extracted_text() {
        let counter = TiktokenCounter::o200k_base().unwrap();
        let image = ChatMessage {
            role: axocoatl_core::MessageRole::User,
            content: axocoatl_core::MessageContent::Parts(vec![
                axocoatl_core::ContentPart::Text("inspect this image".to_string()),
                axocoatl_core::ContentPart::Image {
                    url: format!("data:image/png;base64,{}", "A".repeat(8 * 1024 * 1024)),
                    detail: axocoatl_core::ImageDetail::Auto,
                },
            ]),
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        };
        let image_tokens = counter.count_messages(&[image]);
        assert!(image_tokens > 1_000 && image_tokens < 2_000);

        let extracted_text = ChatMessage {
            role: axocoatl_core::MessageRole::User,
            content: axocoatl_core::MessageContent::Parts(vec![axocoatl_core::ContentPart::Text(
                "word ".repeat(300_000),
            )]),
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        };
        assert!(counter.count_messages(&[extracted_text]) > 200_000);
    }

    #[test]
    fn count_tool_definition() {
        let counter = TiktokenCounter::o200k_base().unwrap();
        let tool = serde_json::json!({
            "name": "get_weather",
            "description": "Get the current weather for a location",
            "parameters": {
                "type": "object",
                "properties": {
                    "location": { "type": "string" }
                }
            }
        });
        let count = counter.count_tool_definition(&tool);
        assert!(count > 10);
    }

    #[test]
    fn approximate_counter_works() {
        let counter = ApproximateCounter::new().unwrap();
        let count = counter.count_text("hello world");
        assert!(count > 0);
    }

    #[test]
    fn unknown_model_returns_error() {
        let result = TiktokenCounter::for_model("nonexistent-model-xyz");
        assert!(result.is_err());
    }
}
