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
            if let Some(text) = msg.text_content() {
                total += self.count_text(text);
            }
            if let Some(name) = &msg.name {
                total += self.count_text(name);
                total -= 1; // name replaces role
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
