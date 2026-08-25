use std::pin::Pin;
use std::sync::OnceLock;

use axocoatl_token::{ApproximateCounter, TokenCounter};
use serde::{Deserialize, Serialize};
use tokio_stream::Stream;

use axocoatl_core::{ChatMessage, ProviderMetadata, TokenUsageStats};

use crate::error::ProviderError;
use crate::tools::{ToolCall, ToolDefinition};

/// Conservative tool-name contract accepted by every provider Axocoatl ships.
/// Keeping this validation at the shared provider boundary prevents one bad
/// configured tool from turning into a remote, provider-specific 400 response.
const MAX_PROVIDER_TOOL_NAME_BYTES: usize = 64;
const MAX_PROVIDER_TOOLS: usize = 128;
const MAX_PROVIDER_STOP_SEQUENCES: usize = 4;

/// Provider identity retained on native tool calls so fallback routing cannot
/// switch protocols in the middle of the follow-up exchange.
pub const TOOL_METADATA_PROVIDER_ID: &str = "axocoatl.provider_id";
/// Reserved fallback-route fields. These are durable protocol state, not tool
/// arguments: a native tool exchange must stay on the exact selected backend
/// and model until the user's current turn finishes.
pub const TOOL_METADATA_ROUTE_SLOT: &str = "axocoatl.route.slot";
pub const TOOL_METADATA_ROUTE_PROVIDER: &str = "axocoatl.route.provider";
pub const TOOL_METADATA_ROUTE_MODEL: &str = "axocoatl.route.model";

pub fn provider_tool_metadata(provider: &str) -> ProviderMetadata {
    ProviderMetadata::from([(TOOL_METADATA_PROVIDER_ID.to_string(), provider.to_string())])
}

/// Validate request fields whose portable provider contract is narrower than
/// their in-memory Rust representation.
pub fn validate_provider_request(
    request: &ChatRequest,
    provider: &str,
) -> Result<(), ProviderError> {
    if request.max_tokens == Some(0) {
        return Err(ProviderError::InvalidRequest {
            provider: provider.to_string(),
            message: "max_tokens must be greater than zero".to_string(),
        });
    }

    if request.stop_sequences.len() > MAX_PROVIDER_STOP_SEQUENCES {
        return Err(ProviderError::InvalidRequest {
            provider: provider.to_string(),
            message: format!(
                "{} stop sequences exceed the portable {MAX_PROVIDER_STOP_SEQUENCES}-sequence limit",
                request.stop_sequences.len()
            ),
        });
    }

    for (field, value) in [
        ("temperature", request.temperature),
        ("top_p", request.top_p),
    ] {
        if value.is_some_and(|value| !value.is_finite()) {
            return Err(ProviderError::InvalidRequest {
                provider: provider.to_string(),
                message: format!("{field} must be finite"),
            });
        }
    }
    if request.temperature.is_some_and(|value| value < 0.0) {
        return Err(ProviderError::InvalidRequest {
            provider: provider.to_string(),
            message: "temperature must be greater than or equal to zero".to_string(),
        });
    }
    if request
        .top_p
        .is_some_and(|value| !(0.0..=1.0).contains(&value))
    {
        return Err(ProviderError::InvalidRequest {
            provider: provider.to_string(),
            message: "top_p must be between zero and one".to_string(),
        });
    }

    if request.tools.len() > MAX_PROVIDER_TOOLS {
        return Err(ProviderError::InvalidRequest {
            provider: provider.to_string(),
            message: format!(
                "{} tool definitions exceed the portable {MAX_PROVIDER_TOOLS}-tool limit",
                request.tools.len()
            ),
        });
    }

    let mut names = std::collections::HashSet::with_capacity(request.tools.len());
    for (index, tool) in request.tools.iter().enumerate() {
        let valid_name = !tool.name.is_empty()
            && tool.name.len() <= MAX_PROVIDER_TOOL_NAME_BYTES
            && tool
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
        if !valid_name {
            return Err(ProviderError::InvalidRequest {
                provider: provider.to_string(),
                message: format!(
                    "tool definition {index} has a name outside the portable 1-64 byte [A-Za-z0-9_-] contract"
                ),
            });
        }
        if !names.insert(tool.name.as_str()) {
            return Err(ProviderError::InvalidRequest {
                provider: provider.to_string(),
                message: format!("tool definition {index} duplicates an earlier tool name"),
            });
        }
        if !tool.parameters.is_object() {
            return Err(ProviderError::InvalidRequest {
                provider: provider.to_string(),
                message: format!("tool definition {index} parameters must be a JSON Schema object"),
            });
        }
    }

    Ok(())
}

/// Validate one normalized tool call before a non-streaming response becomes
/// actionable. Provider output is untrusted: malformed arguments, empty names,
/// and calls to functions that were not advertised must fail closed just like
/// the actor's streaming accumulator.
pub fn validate_response_tool_call(
    provider: &str,
    name: &str,
    arguments: &serde_json::Value,
    tools: &[ToolDefinition],
) -> Result<(), ProviderError> {
    if name.is_empty() {
        return Err(ProviderError::ApiError {
            provider: provider.to_string(),
            status: 200,
            message: "provider returned a tool call with an empty name".to_string(),
        });
    }
    if !tools.iter().any(|tool| tool.name == name) {
        return Err(ProviderError::ApiError {
            provider: provider.to_string(),
            status: 200,
            message: "provider returned a tool call that was not declared in the request"
                .to_string(),
        });
    }
    if !arguments.is_object() {
        return Err(ProviderError::ApiError {
            provider: provider.to_string(),
            status: 200,
            message: "provider returned malformed or non-object tool-call arguments".to_string(),
        });
    }
    Ok(())
}

/// OpenAI-compatible and Anthropic protocols require a non-empty native call
/// id so the following tool-result message can be correlated safely. Gemini is
/// intentionally excluded because its function-call protocol has no call id.
pub fn validate_required_tool_call_id(provider: &str, id: &str) -> Result<(), ProviderError> {
    if id.is_empty() {
        return Err(ProviderError::ApiError {
            provider: provider.to_string(),
            status: 200,
            message: "provider returned a tool call with an empty id".to_string(),
        });
    }
    Ok(())
}

/// Validate the terminal state of a normalized non-streaming response before
/// any returned tool call can become actionable. A provider that reports a
/// tool terminal without calls (or complete calls under a non-tool terminal)
/// has returned an internally inconsistent response and must fail closed.
pub fn validate_chat_response(
    provider: &str,
    response: &ChatResponse,
) -> Result<(), ProviderError> {
    if response.tool_calls.len() > MAX_PROVIDER_TOOLS {
        return Err(ProviderError::ApiError {
            provider: provider.to_string(),
            status: 200,
            message: format!(
                "provider returned {} tool calls, exceeding the portable {MAX_PROVIDER_TOOLS}-call limit",
                response.tool_calls.len()
            ),
        });
    }

    match (
        matches!(&response.finish_reason, FinishReason::ToolUse),
        response.tool_calls.is_empty(),
    ) {
        (true, true) => Err(ProviderError::ApiError {
            provider: provider.to_string(),
            status: 200,
            message: "provider reported a tool-use terminal without a tool call".to_string(),
        }),
        (true, false) | (false, true) => Ok(()),
        (false, false) => Err(ProviderError::ApiError {
            provider: provider.to_string(),
            status: 200,
            message: "provider returned tool calls under a non-tool terminal".to_string(),
        }),
    }
}

/// Streaming counterpart of [`validate_chat_response`]. Adapters call this
/// immediately before `Done`, after they have observed the full response but
/// before an actor can dispatch any accumulated tool call.
pub fn validate_stream_terminal(
    provider: &str,
    finish_reason: &FinishReason,
    tool_call_count: usize,
) -> Result<(), ProviderError> {
    if tool_call_count > MAX_PROVIDER_TOOLS {
        return Err(ProviderError::Stream(format!(
            "{provider} returned {tool_call_count} tool calls, exceeding the portable {MAX_PROVIDER_TOOLS}-call limit"
        )));
    }
    let tool_terminal = matches!(finish_reason, FinishReason::ToolUse);
    match (tool_terminal, tool_call_count == 0) {
        (true, true) => Err(ProviderError::Stream(format!(
            "{provider} reported a tool-use terminal without a tool call"
        ))),
        (true, false) | (false, true) => Ok(()),
        (false, false) => Err(ProviderError::Stream(format!(
            "{provider} returned tool calls under a non-tool terminal"
        ))),
    }
}

/// Validate the terminal state and native ids for an OpenAI-compatible stream.
/// Id-less follow-up calls are valid for Gemini, so Gemini calls
/// [`validate_stream_terminal`] directly instead.
pub fn validate_required_stream_tool_call_ids<'a>(
    provider: &str,
    finish_reason: &FinishReason,
    ids: impl IntoIterator<Item = &'a str>,
) -> Result<(), ProviderError> {
    let mut count = 0usize;
    let mut unique = std::collections::HashSet::new();
    for id in ids {
        count = count.saturating_add(1);
        if id.is_empty() {
            return Err(ProviderError::Stream(format!(
                "{provider} stream returned a tool call with an empty id"
            )));
        }
        if !unique.insert(id) {
            return Err(ProviderError::Stream(format!(
                "{provider} stream returned duplicate tool-call ids"
            )));
        }
    }
    validate_stream_terminal(provider, finish_reason, count)
}

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

    /// Capabilities for the exact model selected by a request override. Most
    /// providers have a uniform family contract and inherit this default;
    /// adapters with model-dependent features override it.
    fn capabilities_for(&self, _request: &ChatRequest) -> ProviderCapabilities {
        self.capabilities()
    }

    /// Whether the boolean/numeric capability facts are authoritative for the
    /// request's exact model id. Open-ended model namespaces and compatible
    /// endpoints return false so wrappers do not turn heuristics into local
    /// rejection. Protocol-level validation still runs via `validate_request`.
    fn model_constraints_known(&self, _request: &ChatRequest) -> bool {
        true
    }

    /// Local request preflight for this exact provider/model route. Wrappers
    /// call it before selecting a fallback so provider-specific sampling and
    /// protocol constraints fail before any remote request is sent.
    fn validate_request(&self, request: &ChatRequest) -> Result<(), ProviderError> {
        validate_provider_request(request, self.provider_id())
    }

    /// Non-streaming chat completion.
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError>;

    /// Streaming chat completion.
    /// Returns a stream of events — caller consumes until `StreamEvent::Done`.
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>;

    /// Count tokens for a request (for budget pre-checking).
    /// Default implementation counts complete message/tool serialization with
    /// a shared provider-agnostic tokenizer. Provider-native implementations
    /// may override it with their exact tokenizer.
    fn count_tokens(&self, request: &ChatRequest) -> usize {
        static COUNTER: OnceLock<Option<ApproximateCounter>> = OnceLock::new();
        let Some(counter) = COUNTER.get_or_init(|| ApproximateCounter::new().ok()) else {
            // Static tokenizer initialization should not fail, but retain a
            // bounded, allocation-free fallback rather than panicking at the
            // provider boundary.
            let message_bytes = serde_json::to_vec(&request.messages)
                .map(|value| value.len())
                .unwrap_or_default();
            let tool_bytes = serde_json::to_vec(&request.tools)
                .map(|value| value.len())
                .unwrap_or_default();
            return message_bytes.saturating_add(tool_bytes).saturating_add(3) / 4;
        };

        let mut total = counter.count_messages(&request.messages);
        for tool in &request.tools {
            let provider_visible = serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            });
            total = total.saturating_add(counter.count_tool_definition(&provider_visible));
        }
        total
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
    /// Used by per-request callers, including the retained lightweight-chat
    /// API; the provider, base URL, and credentials stay the same.
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

/// Estimate generated response tokens when a provider omits usage metadata.
///
/// The estimate includes assistant text plus structured tool-call identity,
/// name, and arguments. Keeping this at the shared provider boundary prevents
/// direct provider callers from treating a paid response with missing usage as
/// an exact zero and keeps their fallback aligned with the Agent tool loop.
pub fn estimate_response_output_tokens(
    counter: &dyn TokenCounter,
    response: &ChatResponse,
) -> usize {
    let mut assistant = ChatMessage::assistant(&response.content);
    assistant.tool_calls = response.tool_calls.clone();
    let reply_priming = counter.count_messages(&[]);
    let structured = counter
        .count_messages(&[assistant])
        .saturating_sub(reply_priming);
    // Some custom TokenCounter implementations historically counted only
    // message text. Keep an explicit structured lower bound as well.
    let explicit =
        response
            .tool_calls
            .iter()
            .fold(counter.count_text(&response.content), |tokens, call| {
                let provider_output = serde_json::json!({
                    "id": &call.id,
                    "name": &call.name,
                    "arguments": &call.arguments,
                });
                tokens.saturating_add(counter.count_text(&provider_output.to_string()))
            });
    structured.max(explicit)
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
    /// Exact backend route selected by a provider wrapper for this stream.
    /// The actor merges it into every tool call produced by the response,
    /// including calls recovered from model-authored text, so a later native
    /// tool-response round cannot switch provider protocols.
    ProviderRoute { metadata: ProviderMetadata },
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
    /// Opaque metadata for a streamed tool call. Kept separate from argument
    /// deltas so existing provider streams remain simple and metadata is never
    /// exposed to the tool executor as model-authored arguments.
    ToolCallMetadata {
        index: Option<usize>,
        id: String,
        metadata: ProviderMetadata,
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
        let mut req = ChatRequest::simple("hello world test");
        let message_count = provider.count_tokens(&req);
        assert!(message_count > 0);
        req.tools.push(ToolDefinition {
            name: "lookup".to_string(),
            description: "Look up one value".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"key": {"type": "string"}}
            }),
            concurrency: Default::default(),
        });
        assert!(provider.count_tokens(&req) > message_count);
    }

    #[test]
    fn missing_usage_estimate_counts_prose_and_structured_tool_output() {
        let counter = ApproximateCounter::new().unwrap();
        let mut prose = response(FinishReason::Stop, 0);
        prose.content = "I inspected the repository.".to_string();
        assert!(estimate_response_output_tokens(&counter, &prose) > 0);

        let tool_only = response(FinishReason::ToolUse, 1);
        assert!(estimate_response_output_tokens(&counter, &tool_only) > 0);
        assert!(
            estimate_response_output_tokens(&counter, &tool_only)
                > counter.count_text(&tool_only.content)
        );
    }

    #[test]
    fn provider_request_rejects_incompatible_or_duplicate_tool_names() {
        let definition = |name: &str| ToolDefinition {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
            concurrency: Default::default(),
        };

        let mut request = ChatRequest::simple("hello");
        request.tools = vec![definition("server.tool")];
        assert!(matches!(
            validate_provider_request(&request, "test"),
            Err(ProviderError::InvalidRequest { .. })
        ));

        request.tools = vec![definition("read_file"), definition("read_file")];
        assert!(matches!(
            validate_provider_request(&request, "test"),
            Err(ProviderError::InvalidRequest { .. })
        ));
    }

    #[test]
    fn provider_request_accepts_qualified_portable_tool_name() {
        let mut request = ChatRequest::simple("hello");
        request.tools = vec![ToolDefinition {
            name: "mcp__filesystem__read_file".to_string(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
            concurrency: Default::default(),
        }];
        validate_provider_request(&request, "test").unwrap();
    }

    #[test]
    fn provider_request_rejects_nonobject_tool_schema() {
        let mut request = ChatRequest::simple("hello");
        request.tools = vec![ToolDefinition {
            name: "lookup".to_string(),
            description: String::new(),
            parameters: serde_json::Value::Null,
            concurrency: Default::default(),
        }];
        assert!(matches!(
            validate_provider_request(&request, "test"),
            Err(ProviderError::InvalidRequest { message, .. })
                if message.contains("JSON Schema object")
        ));
    }

    fn response(finish_reason: FinishReason, tool_call_count: usize) -> ChatResponse {
        ChatResponse {
            content: String::new(),
            tool_calls: (0..tool_call_count)
                .map(|index| ToolCall {
                    id: format!("call_{index}"),
                    name: "lookup".to_string(),
                    arguments: serde_json::json!({}),
                    provider_metadata: Default::default(),
                })
                .collect(),
            finish_reason,
            usage: TokenUsageStats::default(),
            model: "test".to_string(),
            provider: "test".to_string(),
        }
    }

    #[test]
    fn normalized_response_terminal_must_match_tool_calls() {
        validate_chat_response("test", &response(FinishReason::Stop, 0)).unwrap();
        validate_chat_response("test", &response(FinishReason::ToolUse, 1)).unwrap();
        assert!(validate_chat_response("test", &response(FinishReason::ToolUse, 0)).is_err());
        assert!(validate_chat_response("test", &response(FinishReason::Stop, 1)).is_err());

        validate_stream_terminal("test", &FinishReason::Stop, 0).unwrap();
        validate_stream_terminal("test", &FinishReason::ToolUse, 1).unwrap();
        assert!(validate_stream_terminal("test", &FinishReason::ToolUse, 0).is_err());
        assert!(validate_stream_terminal("test", &FinishReason::Stop, 1).is_err());

        validate_required_stream_tool_call_ids("test", &FinishReason::ToolUse, ["call_1"]).unwrap();
        assert!(
            validate_required_stream_tool_call_ids("test", &FinishReason::ToolUse, [""],).is_err()
        );
        assert!(validate_required_stream_tool_call_ids(
            "test",
            &FinishReason::ToolUse,
            ["call_1", "call_1"],
        )
        .is_err());
    }

    #[test]
    fn required_native_tool_id_rejects_empty_value() {
        validate_required_tool_call_id("test", "call_1").unwrap();
        assert!(matches!(
            validate_required_tool_call_id("test", ""),
            Err(ProviderError::ApiError { message, .. }) if message.contains("empty id")
        ));
    }
}
