use std::{pin::Pin, sync::Arc};

mod native;
mod text_calls;
pub use native::{
    check_native_ollama_server, check_shown_model, describe_native_ollama_error,
    inspect_native_ollama_model, native_ollama_error_hint, observe_native_ollama_context,
    observe_native_ollama_context_within, ollama_version_accepted, ollama_version_note,
    ollama_version_refusal, ollama_version_unaudited, show_native_ollama_model,
    validate_native_ollama_endpoint, NativeOllamaConfig, NativeOllamaContextObservation,
    NativeOllamaProvider, NativeOllamaServerCheck, OllamaCloudMode, OllamaServedVersions,
    ShownModel, AUDITED_OLLAMA_VERSION, CONTRACT_STOP_PROBE_PROMPT, DENIED_OLLAMA_VERSIONS,
    OBSERVATION_SCHEMA_VERSION,
};

/// How long Ollama may take to start an answer, loading the model first,
/// unless `providers.ollama.first_token_timeout_secs` sets it.
pub use axocoatl_llm::transport::LOCAL_FIRST_TOKEN_TIMEOUT;
use reqwest::header::CONTENT_TYPE;
use tokio_stream::{Stream, StreamExt};

use text_calls::{TextCalls, TextHold};

use axocoatl_core::{MessageContent, MessageRole, TokenUsageStats};
use axocoatl_llm::{
    provider_tool_metadata,
    transport::{
        bounded_redacted, direct_http_client, network_error, read_error_text, read_json,
        validated_endpoint, SseDecoder, StreamDeadlines, StreamTimeouts, STREAM_TOTAL_TIMEOUT,
    },
    validate_chat_response, validate_provider_request, validate_required_stream_tool_call_ids,
    validate_required_tool_call_id, validate_response_tool_call, validate_stream_terminal,
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};

/// Split a `MessageContent` into Ollama's native shape: a `content` string
/// plus an `images` array of base64-encoded blobs. Images arrive on the
/// generic `ContentPart::Image { url }` as `data:image/...;base64,XXX`
/// data URIs — we strip the header and pass the bytes.
fn ollama_split_content(content: &MessageContent) -> (String, Vec<String>) {
    let mut text = String::new();
    let mut images: Vec<String> = Vec::new();
    match content {
        MessageContent::Text(s) => text.push_str(s),
        MessageContent::Parts(parts) => {
            for p in parts {
                match p {
                    axocoatl_core::ContentPart::Text(s) => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(s);
                    }
                    axocoatl_core::ContentPart::Image { url, .. } => {
                        if let Some(idx) = url.find("base64,") {
                            images.push(url[idx + "base64,".len()..].to_string());
                        }
                        // Non-base64 image URLs are skipped — Ollama's chat
                        // API accepts only inline base64 in `images`.
                    }
                }
            }
        }
    }
    (text, images)
}

/// Convert Axocoatl chat messages into the OpenAI-compatible `messages` array
/// Ollama's `/v1/chat/completions` endpoint expects. Shared by `chat` and
/// `chat_stream` so the two paths can't drift. Crucially this carries the
/// assistant's `tool_calls` and each tool result's `tool_call_id` through, so a
/// multi-turn tool round-trip replays as a well-formed conversation.
fn ollama_messages(messages: &[axocoatl_core::ChatMessage]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|m| {
            let role = match m.role {
                MessageRole::System => "system",
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::Tool => "tool",
            };
            let (content, images) = ollama_split_content(&m.content);
            let mut msg = serde_json::json!({ "role": role, "content": content });
            if !images.is_empty() {
                msg["images"] = serde_json::json!(images);
            }
            if matches!(m.role, MessageRole::Assistant) && !m.tool_calls.is_empty() {
                msg["tool_calls"] = serde_json::Value::Array(
                    m.tool_calls
                        .iter()
                        .map(|tc| {
                            serde_json::json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    // OpenAI schema: arguments is a JSON string.
                                    "arguments": serde_json::to_string(&tc.arguments)
                                        .unwrap_or_else(|_| "{}".to_string()),
                                }
                            })
                        })
                        .collect(),
                );
            }
            if matches!(m.role, MessageRole::Tool) {
                if let Some(id) = m.tool_call_id.as_ref().or(m.name.as_ref()) {
                    msg["tool_call_id"] = serde_json::json!(id);
                }
            }
            msg
        })
        .collect()
}

/// Convert tool definitions into the OpenAI-compatible `tools` array that
/// Ollama's `/v1/chat/completions` endpoint expects.
fn tools_json(tools: &[axocoatl_llm::ToolDefinition]) -> serde_json::Value {
    serde_json::Value::Array(
        tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect(),
    )
}

fn usage_event(chunk: &serde_json::Value) -> Option<StreamEvent> {
    chunk
        .get("usage")
        .filter(|usage| !usage.is_null())
        .map(|usage| {
            StreamEvent::Usage(TokenUsageStats {
                input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0) as usize,
                output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0) as usize,
                reasoning_tokens: None,
            })
        })
}

/// Return the first reasoning fragment used by Ollama or a compatible server.
/// Reasoning is provider metadata, never assistant content: callers may expose
/// it as a reasoning event or use its presence for terminal validation, but
/// must not promote it into the answer that can drive product behavior.
fn reasoning_text(value: &serde_json::Value) -> Result<Option<&str>, String> {
    for field in ["reasoning", "reasoning_content", "thinking"] {
        let Some(raw) = value.get(field) else {
            continue;
        };
        if raw.is_null() {
            continue;
        }
        let text = raw
            .as_str()
            .ok_or_else(|| format!("Ollama response field {field} was not a string"))?;
        if !text.is_empty() {
            return Ok(Some(text));
        }
    }
    Ok(None)
}

fn reasoning_only_terminal_message(finish_reason: &FinishReason) -> &'static str {
    match finish_reason {
        FinishReason::MaxTokens => {
            "Ollama exhausted the completion limit in reasoning without final content; increase the completion limit or choose a model that can finish within it"
        }
        FinishReason::ContentFilter => {
            "Ollama filtered a reasoning-only response and returned no final content"
        }
        _ => "Ollama returned reasoning without final content",
    }
}

fn normalize_ollama_nonstream_finish(
    native_reason: Option<&str>,
    structured_tool_calls: usize,
    recovered_tool_calls: usize,
) -> Result<FinishReason, ProviderError> {
    let invalid = || ProviderError::ApiError {
        provider: "ollama".to_string(),
        status: 200,
        message: "Ollama returned tool calls under an incompatible or missing finish reason"
            .to_string(),
    };
    if structured_tool_calls > 0 {
        return if native_reason == Some("tool_calls") {
            Ok(FinishReason::ToolUse)
        } else {
            Err(invalid())
        };
    }
    if recovered_tool_calls > 0 {
        return if matches!(native_reason, Some("stop" | "tool_calls")) {
            Ok(FinishReason::ToolUse)
        } else {
            Err(invalid())
        };
    }
    match native_reason {
        Some("stop") => Ok(FinishReason::Stop),
        Some("length") => Ok(FinishReason::MaxTokens),
        Some("content_filter") => Ok(FinishReason::ContentFilter),
        Some(other) => Err(ProviderError::ApiError {
            provider: "ollama".to_string(),
            status: 200,
            message: format!("Ollama returned unsupported finish reason {other}"),
        }),
        None => Err(ProviderError::ApiError {
            provider: "ollama".to_string(),
            status: 200,
            message: "Ollama response omitted its finish reason".to_string(),
        }),
    }
}

/// The calls written as text that end a finished answer, for the
/// OpenAI-compatible finish reason it ended with: read after `stop` (or a
/// `tool_calls` that brought none), and only reported after `length`.
fn compat_text_calls(
    content: &str,
    request: &ChatRequest,
    finish_reason: Option<&str>,
) -> TextCalls {
    match finish_reason {
        Some("stop" | "tool_calls") => {
            text_calls::read_text_calls(content, request, "ollama", true)
        }
        Some("length") => text_calls::read_text_calls(content, request, "ollama", false),
        _ => TextCalls::Answer,
    }
}

/// The refusal of an answer that calls an offered tool as text that cannot
/// run: the turn needs attention instead of ending having run nothing.
fn text_call_refusal(why: String, finish_reason: &str, request: &ChatRequest) -> ProviderError {
    ProviderError::RefusedResponse {
        provider: "ollama".to_string(),
        message: format!(
            "{why}. Nothing from this response ran; Ollama ended it with finish_reason \
             \"{finish_reason}\" and {} offered",
            text_calls::count_of(request.tools.len(), "tool")
        ),
    }
}

fn parse_structured_tool_calls(
    message: &serde_json::Value,
    tools: &[axocoatl_llm::ToolDefinition],
) -> Result<Vec<axocoatl_llm::ToolCall>, ProviderError> {
    let Some(calls) = message["tool_calls"].as_array() else {
        return Ok(Vec::new());
    };
    let calls: Vec<axocoatl_llm::ToolCall> = calls
        .iter()
        .map(|call| {
            let id = call["id"].as_str().unwrap_or("").to_string();
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            let args =
                call["function"]["arguments"]
                    .as_str()
                    .ok_or_else(|| ProviderError::ApiError {
                        provider: "ollama".to_string(),
                        status: 200,
                        message: "provider returned malformed tool-call arguments".to_string(),
                    })?;
            let arguments = serde_json::from_str(args).map_err(|_| ProviderError::ApiError {
                provider: "ollama".to_string(),
                status: 200,
                message: "provider returned malformed tool-call arguments".to_string(),
            })?;
            validate_required_tool_call_id("ollama", &id)?;
            validate_response_tool_call("ollama", &name, &arguments, tools)?;
            Ok(axocoatl_llm::ToolCall {
                id,
                name,
                arguments,
                provider_metadata: provider_tool_metadata("ollama"),
            })
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;
    let mut ids = std::collections::HashSet::with_capacity(calls.len());
    if calls.iter().any(|call| !ids.insert(call.id.as_str())) {
        return Err(ProviderError::ApiError {
            provider: "ollama".to_string(),
            status: 200,
            message: "provider returned duplicate tool-call ids".to_string(),
        });
    }
    Ok(calls)
}

/// The answer text a compatible stream received and how much of it is shown.
#[derive(Default)]
struct StreamedText {
    content: String,
    hold: TextHold,
}

impl StreamedText {
    fn lock(text: &std::sync::Mutex<Self>) -> std::sync::MutexGuard<'_, Self> {
        text.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// Add `delta` and return the text that can be shown now: all of it
    /// when `free` (no text is read as calls).
    fn push(&mut self, delta: &str, free: bool) -> Option<String> {
        self.content.push_str(delta);
        if free {
            self.rest()
        } else {
            self.hold.release(&self.content)
        }
    }

    fn release_to(&mut self, end: usize) -> Option<String> {
        self.hold.release_to(&self.content, end)
    }

    /// Everything not shown yet.
    fn rest(&mut self) -> Option<String> {
        self.release_to(self.content.len())
    }
}

/// Ollama provider using its OpenAI-compatible chat completions endpoint.
/// Compatible implementations must honor the Ollama request/response contract.
pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    /// How long an answer may take to start and to pause
    /// ([`StreamTimeouts::LOCAL`] unless the configuration sets them).
    stream_timeouts: StreamTimeouts,
}

impl OllamaProvider {
    /// Create a provider for a local Ollama instance (default: http://localhost:11434).
    pub fn new(model: impl Into<String>) -> Self {
        Self::with_base_url("http://localhost:11434", model)
    }

    /// Create with a custom base URL (for remote Ollama or a compatible implementation).
    /// Ollama is on this computer or its network, so no proxy is used.
    pub fn with_base_url(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: direct_http_client(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            stream_timeouts: StreamTimeouts::LOCAL,
        }
    }

    /// Wait for each answer as `timeouts` say (`providers.ollama`'s
    /// `first_token_timeout_secs` and `idle_timeout_secs`).
    pub fn with_stream_timeouts(mut self, timeouts: StreamTimeouts) -> Self {
        self.stream_timeouts = timeouts;
        self
    }

    fn endpoint(&self) -> Result<String, ProviderError> {
        validated_endpoint(&self.base_url, "v1/chat/completions", self.provider_id())
    }

    /// Build the OpenAI-compatible request body shared by the buffered and
    /// streaming chat paths.
    fn build_request_body(&self, request: &ChatRequest, stream: bool) -> serde_json::Value {
        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);
        let mut body = serde_json::json!({
            "model": model_for_call,
            "messages": ollama_messages(&request.messages),
        });

        // Short schema-bound control calls can explicitly suppress thinking.
        // Ordinary Session, Automation, and tool turns omit this option and
        // preserve the model's normal reasoning behavior.
        if request
            .provider_options
            .as_ref()
            .and_then(|options| options.get("reasoning_effort"))
            .and_then(serde_json::Value::as_str)
            == Some("none")
        {
            body["reasoning_effort"] = serde_json::json!("none");
        }

        if stream {
            body["stream"] = serde_json::json!(true);
            body["stream_options"] = serde_json::json!({ "include_usage": true });
        }
        if let Some(max) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }
        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(top_p) = request.top_p {
            body["top_p"] = serde_json::json!(top_p);
        }
        if !request.stop_sequences.is_empty() {
            body["stop"] = serde_json::json!(&request.stop_sequences);
        }
        if request.response_format == Some(axocoatl_core::ResponseFormat::Json) {
            // This provider targets Ollama's OpenAI-compatible endpoint. Its
            // JSON-mode contract is OpenAI's `response_format` object, not the
            // native `/api/chat` endpoint's top-level `format: "json"` field.
            body["response_format"] = serde_json::json!({ "type": "json_object" });
        }
        if !request.tools.is_empty() {
            body["tools"] = tools_json(&request.tools);
        }

        body
    }
}

#[async_trait::async_trait]
impl LlmProvider for OllamaProvider {
    fn provider_id(&self) -> &str {
        "ollama"
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true, // Sent on every request; honoured by tool-capable models
            structured_output: true,
            vision: true,
            reasoning: true,
            embeddings: false,
            // Ollama model names and Modelfile limits are operator-defined.
            max_context_tokens: 0,
            max_output_tokens: 0,
        }
    }

    fn model_constraints_known(&self, _request: &ChatRequest) -> bool {
        false
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        validate_provider_request(&request, self.provider_id())?;
        let body = self.build_request_body(&request, false);
        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);

        let response = self
            .client
            .post(self.endpoint()?)
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .timeout(self.stream_timeouts.buffered())
            .send()
            .await
            .map_err(|error| network_error(&error, &[]))?;

        let status = response.status();
        if status == 429 {
            let retry_after_secs = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok());
            return Err(ProviderError::RateLimited {
                provider: "ollama".to_string(),
                retry_after_secs,
            });
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ProviderError::AuthError {
                provider: "ollama".to_string(),
            });
        }
        if status.as_u16() == 404 {
            return Err(ProviderError::ModelNotFound {
                provider: "ollama".to_string(),
                model: model_for_call.to_string(),
            });
        }
        if !status.is_success() {
            let err_text = read_error_text(response, &[]).await;
            return Err(ProviderError::ApiError {
                provider: "ollama".to_string(),
                status: status.as_u16(),
                message: err_text,
            });
        }

        let resp_body: serde_json::Value = read_json(response, "ollama").await?;

        let choices = resp_body["choices"]
            .as_array()
            .ok_or_else(|| ProviderError::ApiError {
                provider: "ollama".to_string(),
                status: 200,
                message: "Ollama response omitted choices".to_string(),
            })?;
        if choices.len() != 1 || choices[0]["index"].as_u64() != Some(0) {
            return Err(ProviderError::ApiError {
                provider: "ollama".to_string(),
                status: 200,
                message: "Ollama response must contain exactly choice index 0".to_string(),
            });
        }
        let choice = &choices[0];

        let mut content = choice["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let saw_reasoning = reasoning_text(&choice["message"])
            .map_err(|message| ProviderError::ApiError {
                provider: "ollama".to_string(),
                status: 200,
                message,
            })?
            .is_some();

        // Extract tool calls from OpenAI-compatible response
        let mut tool_calls = parse_structured_tool_calls(&choice["message"], &request.tools)?;
        let structured_tool_call_count = tool_calls.len();

        // Calls a model wrote as text that end the answer, read as the
        // native provider reads them (`text_calls`). With a structured call,
        // markup in the text stays text.
        if tool_calls.is_empty() {
            let finish = choice["finish_reason"].as_str();
            match compat_text_calls(&content, &request, finish) {
                TextCalls::Calls { answer_end, calls } => {
                    content.truncate(answer_end);
                    tool_calls = calls;
                }
                TextCalls::Unreadable(why) => {
                    return Err(text_call_refusal(why, finish.unwrap_or_default(), &request))
                }
                TextCalls::Answer => {}
            }
        }

        let recovered_tool_call_count = tool_calls.len().saturating_sub(structured_tool_call_count);
        let finish_reason = normalize_ollama_nonstream_finish(
            choice["finish_reason"].as_str(),
            structured_tool_call_count,
            recovered_tool_call_count,
        )?;
        if saw_reasoning && content.trim().is_empty() && tool_calls.is_empty() {
            return Err(ProviderError::ApiError {
                provider: "ollama".to_string(),
                status: 200,
                message: reasoning_only_terminal_message(&finish_reason).to_string(),
            });
        }

        let normalized = ChatResponse {
            content,
            tool_calls,
            finish_reason,
            usage: TokenUsageStats {
                input_tokens: resp_body["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as usize,
                output_tokens: resp_body["usage"]["completion_tokens"]
                    .as_u64()
                    .unwrap_or(0) as usize,
                reasoning_tokens: None,
            },
            model: resp_body["model"]
                .as_str()
                .unwrap_or(model_for_call)
                .to_string(),
            provider: "ollama".to_string(),
        };
        validate_chat_response("ollama", &normalized)?;
        Ok(normalized)
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        validate_provider_request(&request, self.provider_id())?;
        let body = self.build_request_body(&request, true);
        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);

        let mut deadlines = StreamDeadlines::start(self.stream_timeouts);
        let response = deadlines
            .headers(
                self.client
                    .post(self.endpoint()?)
                    .header(CONTENT_TYPE, "application/json")
                    .json(&body)
                    .send(),
                self.provider_id(),
            )
            .await?
            .map_err(|error| network_error(&error, &[]))?;

        let status = response.status();
        if status == 429 {
            let retry_after_secs = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok());
            return Err(ProviderError::RateLimited {
                provider: "ollama".to_string(),
                retry_after_secs,
            });
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ProviderError::AuthError {
                provider: "ollama".to_string(),
            });
        }
        if status.as_u16() == 404 {
            return Err(ProviderError::ModelNotFound {
                provider: "ollama".to_string(),
                model: model_for_call.to_string(),
            });
        }
        if !status.is_success() {
            let err_text = read_error_text(response, &[]).await;
            return Err(ProviderError::ApiError {
                provider: "ollama".to_string(),
                status: status.as_u16(),
                message: err_text,
            });
        }

        // OpenAI-compatible SSE: each event is `data: {json}` or `data: [DONE]`.
        let mut byte_stream = response.bytes_stream();

        // Calls written as text are read from the finished answer, as the
        // native provider reads them.
        let reads_text_calls = text_calls::reads_text_calls(&request);
        // Accumulated assistant text and how much of it is shown: a call the
        // model writes as text is held off-screen until the answer ends and
        // shows whether it is one ([`TextHold`]). Shared with the error path
        // below, which shows what was held when the response fails.
        let streamed = Arc::new(std::sync::Mutex::new(StreamedText::default()));
        let text = streamed.clone();

        let stream = async_stream::try_stream! {
            let mut decoder = SseDecoder::provider_default();
            let mut free = !reads_text_calls;
            let mut saw_struct_tool_call = false;
            let mut saw_reasoning = false;
            let mut pending_finish: Option<(FinishReason, String)> = None;
            let mut saw_sentinel = false;
            let mut structured_tool_call_ids = std::collections::BTreeMap::<usize, String>::new();
            let mut recovered_tool_call_count = 0usize;
            let total_deadline = tokio::time::Instant::now() + STREAM_TOTAL_TIMEOUT;

            'response: loop {
                let next = deadlines
                    .next(&mut byte_stream, total_deadline, "ollama")
                    .await?;
                let reached_eof = next.is_none();
                let events = match next {
                    Some(chunk) => {
                        let chunk = chunk.map_err(|error| {
                            ProviderError::Stream(bounded_redacted(&error.to_string(), 8 * 1024, &[]))
                        })?;
                        decoder.push(&chunk)?
                    }
                    None => decoder.finish()?,
                };

                for event in events {
                    if event.data.trim() == "[DONE]" {
                        saw_sentinel = true;
                        break 'response;
                    }

                    let parsed: serde_json::Value = serde_json::from_str(&event.data)
                        .map_err(|error| ProviderError::Stream(format!("invalid Ollama SSE JSON: {error}")))?;

                    // OpenAI-compatible servers emit exact usage in a final
                    // empty-choice chunk when `stream_options.include_usage`
                    // is requested. It must not depend on a choice-level
                    // finish_reason being present in the same frame.
                    if let Some(usage) = usage_event(&parsed) {
                        yield usage;
                    }

                    let choices = parsed["choices"].as_array().ok_or_else(|| {
                        ProviderError::Stream("Ollama stream frame omitted choices".to_string())
                    })?;
                    if choices.len() > 1
                        || choices.first().is_some_and(|choice| choice["index"].as_u64() != Some(0))
                    {
                        Err(ProviderError::Stream(
                            "Ollama stream returned multiple alternatives or a nonzero choice index"
                                .to_string(),
                        ))?;
                    }
                    if choices.is_empty()
                        && parsed
                            .get("usage")
                            .is_none_or(serde_json::Value::is_null)
                    {
                        Err(ProviderError::Stream(
                            "Ollama stream returned an empty non-usage frame".to_string(),
                        ))?;
                    }
                    for choice in choices {
                            if let Some(reasoning) = reasoning_text(&choice["delta"])
                                .map_err(ProviderError::Stream)?
                            {
                                saw_reasoning = true;
                                yield StreamEvent::ReasoningDelta {
                                    delta: reasoning.to_string(),
                                };
                            }

                            // Text content deltas, shown as they can be; the
                            // finished answer is read for calls written as text.
                            if let Some(content) = choice["delta"]["content"].as_str() {
                                if !content.is_empty() {
                                    let delta = StreamedText::lock(&text).push(content, free);
                                    if let Some(delta) = delta {
                                        yield StreamEvent::TextDelta { delta };
                                    }
                                }
                            }

                            // Structured tool call deltas (the usual path). OpenAI-
                            // compatible streams send the id once and key later
                            // argument fragments by `index`.
                            if let Some(tool_calls) = choice["delta"]["tool_calls"].as_array() {
                                saw_struct_tool_call = true;
                                // With a structured call no text is read as a
                                // call: what was held is shown before it.
                                if !free {
                                    free = true;
                                    let delta = StreamedText::lock(&text).rest();
                                    if let Some(delta) = delta {
                                        yield StreamEvent::TextDelta { delta };
                                    }
                                }
                                for tc in tool_calls {
                                    let index = tc["index"].as_u64().map(|i| i as usize);
                                    let required_index = index.ok_or_else(|| {
                                        ProviderError::Stream(
                                            "Ollama streamed a structured tool call without an index".to_string(),
                                        )
                                    })?;
                                    let id = tc["id"].as_str().unwrap_or("").to_string();
                                    let known_id = structured_tool_call_ids
                                        .entry(required_index)
                                        .or_default();
                                    if !id.is_empty() {
                                        if !known_id.is_empty() && known_id != &id {
                                            Err(ProviderError::Stream(format!(
                                                "Ollama changed a tool-call id for index {required_index}"
                                            )))?;
                                        }
                                        *known_id = id.clone();
                                    }
                                    let name = tc["function"]["name"].as_str().map(String::from);
                                    let args_delta = tc["function"]["arguments"]
                                        .as_str()
                                        .unwrap_or("")
                                        .to_string();
                                    yield StreamEvent::ToolCallDelta {
                                        index,
                                        id: id.clone(),
                                        name,
                                        args_delta,
                                    };
                                    yield StreamEvent::ToolCallMetadata {
                                        index,
                                        id,
                                        metadata: provider_tool_metadata("ollama"),
                                    };
                                }
                            }

                            // Finish reason. The answer is read for calls
                            // written as text once the stream ends.
                            if let Some(reason) = choice["finish_reason"].as_str() {
                                let finish = match reason {
                                    "stop" => FinishReason::Stop,
                                    "tool_calls" => FinishReason::ToolUse,
                                    "length" => FinishReason::MaxTokens,
                                    "content_filter" => FinishReason::ContentFilter,
                                    other => Err(ProviderError::Stream(format!(
                                        "Ollama returned unsupported finish reason {other}"
                                    )))?,
                                };
                                pending_finish = Some((finish, reason.to_string()));
                            }
                    }
                }

                if reached_eof {
                    break;
                }
            }

            let (mut finish_reason, native_reason) = pending_finish.ok_or_else(|| {
                let terminal = if saw_sentinel { "terminal sentinel" } else { "connection close" };
                ProviderError::Stream(format!("Ollama stream reached {terminal} without a finish reason"))
            })?;
            let (read, delta, blank) = {
                let mut streamed = StreamedText::lock(&text);
                let read = if free {
                    TextCalls::Answer
                } else {
                    compat_text_calls(&streamed.content, &request, Some(native_reason.as_str()))
                };
                let answer_end = match &read {
                    TextCalls::Calls { answer_end, .. } => *answer_end,
                    _ => streamed.content.len(),
                };
                let delta = streamed.release_to(answer_end);
                (read, delta, streamed.content.trim().is_empty())
            };
            if let Some(delta) = delta {
                yield StreamEvent::TextDelta { delta };
            }
            match read {
                TextCalls::Unreadable(why) => {
                    Err(text_call_refusal(why, &native_reason, &request))?;
                }
                TextCalls::Calls { calls, .. } => {
                    recovered_tool_call_count = calls.len();
                    for (i, call) in calls.iter().enumerate() {
                        yield StreamEvent::ToolCallDelta {
                            index: Some(i),
                            id: call.id.clone(),
                            name: Some(call.name.clone()),
                            args_delta: call.arguments.to_string(),
                        };
                        yield StreamEvent::ToolCallMetadata {
                            index: Some(i),
                            id: call.id.clone(),
                            metadata: call.provider_metadata.clone(),
                        };
                    }
                    finish_reason = FinishReason::ToolUse;
                }
                TextCalls::Answer => {}
            }
            let tool_call_count = if saw_struct_tool_call {
                validate_required_stream_tool_call_ids(
                    "Ollama",
                    &finish_reason,
                    structured_tool_call_ids.values().map(String::as_str),
                )?;
                None
            } else {
                Some(recovered_tool_call_count)
            };
            if let Some(tool_call_count) = tool_call_count {
                validate_stream_terminal("Ollama", &finish_reason, tool_call_count)?;
            }
            if saw_reasoning
                && blank
                && !saw_struct_tool_call
                && recovered_tool_call_count == 0
            {
                Err(ProviderError::Stream(
                    reasoning_only_terminal_message(&finish_reason).to_string(),
                ))?;
            }
            yield StreamEvent::Done { finish_reason };
        };

        // A response that fails shows the text held so far first, so the
        // abandoned output is not lost; nothing in it runs.
        let stream = async_stream::stream! {
            let mut stream = Box::pin(stream);
            while let Some(item) = stream.next().await {
                if item.is_err() {
                    let held = StreamedText::lock(&streamed).rest();
                    if let Some(delta) = held {
                        yield Ok(StreamEvent::TextDelta { delta });
                    }
                    yield item;
                    return;
                }
                yield item;
            }
        };
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_request() -> ChatRequest {
        let mut request = ChatRequest::simple("lookup");
        request.tools = vec![axocoatl_llm::ToolDefinition {
            name: "lookup".to_string(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
            concurrency: Default::default(),
        }];
        request
    }

    fn recovered_tool_call_flood(count: usize) -> String {
        r#"<tool_call>{"name":"lookup","arguments":{}}</tool_call>"#.repeat(count)
    }

    fn assert_openai_json_mode(body: &serde_json::Value) {
        assert_eq!(
            body.get("response_format"),
            Some(&serde_json::json!({ "type": "json_object" }))
        );
        assert!(
            body.get("format").is_none(),
            "the native Ollama `format` field is invalid on /v1/chat/completions"
        );
    }

    fn assert_reasoning_disabled(body: &serde_json::Value) {
        assert_eq!(
            body.get("reasoning_effort"),
            Some(&serde_json::json!("none")),
            "an explicitly non-reasoning call must use the endpoint's hard thinking switch"
        );
    }

    fn disable_reasoning(request: &mut ChatRequest) {
        request.provider_options = Some(serde_json::json!({"reasoning_effort": "none"}));
    }

    #[test]
    fn default_base_url() {
        let provider = OllamaProvider::new("llama3");
        assert_eq!(
            provider.endpoint().unwrap(),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(provider.model_id(), "llama3");
        assert_eq!(provider.provider_id(), "ollama");
    }

    #[test]
    fn custom_base_url() {
        let provider = OllamaProvider::with_base_url("http://gpu-server:11434", "mistral");
        assert_eq!(
            provider.endpoint().unwrap(),
            "http://gpu-server:11434/v1/chat/completions"
        );
    }

    #[test]
    fn trailing_slash_stripped() {
        let provider = OllamaProvider::with_base_url("http://localhost:11434/", "llama3");
        assert_eq!(
            provider.endpoint().unwrap(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn capabilities_local_model() {
        let provider = OllamaProvider::new("llama3");
        let caps = provider.capabilities();
        assert!(caps.vision);
        assert!(caps.tool_calling);
        assert!(caps.reasoning);
        assert_eq!(caps.max_context_tokens, 0);
        assert!(!provider.model_constraints_known(&ChatRequest::simple("test")));
    }

    #[test]
    fn buffered_json_mode_request_uses_openai_compatible_shape() {
        let provider = OllamaProvider::new("llama3");
        let mut request = ChatRequest::simple("Return JSON");
        request.response_format = Some(axocoatl_core::ResponseFormat::Json);
        disable_reasoning(&mut request);

        let body = provider.build_request_body(&request, false);

        assert_openai_json_mode(&body);
        assert_reasoning_disabled(&body);
        assert!(body.get("stream").is_none());
    }

    #[test]
    fn streaming_json_mode_request_uses_openai_compatible_shape() {
        let provider = OllamaProvider::new("llama3");
        let mut request = ChatRequest::simple("Return JSON");
        request.response_format = Some(axocoatl_core::ResponseFormat::Json);
        disable_reasoning(&mut request);

        let body = provider.build_request_body(&request, true);

        assert_openai_json_mode(&body);
        assert_reasoning_disabled(&body);
        assert_eq!(body.get("stream"), Some(&serde_json::json!(true)));
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn ordinary_request_preserves_the_models_reasoning_default() {
        let provider = OllamaProvider::new("qwen3:8b");
        let body = provider.build_request_body(&ChatRequest::simple("solve this"), false);

        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn empty_choice_usage_tail_is_not_discarded() {
        let tail = serde_json::json!({
            "choices": [],
            "usage": { "prompt_tokens": 21, "completion_tokens": 8 }
        });
        assert!(matches!(
            usage_event(&tail),
            Some(StreamEvent::Usage(TokenUsageStats {
                input_tokens: 21,
                output_tokens: 8,
                ..
            }))
        ));
    }

    #[tokio::test]
    async fn nonstream_reasoning_is_never_promoted_to_final_content() {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let response = serde_json::json!({
            "choices": [{
                "index": 0,
                "message": {
                    "reasoning": "private chain of thought",
                    "content": "FINAL"
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 7, "completion_tokens": 11},
            "model": "qwen3:8b"
        });
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;

        let provider = OllamaProvider::with_base_url(server.uri(), "qwen3:8b");
        let response = provider.chat(ChatRequest::simple("plan")).await.unwrap();
        assert_eq!(response.content, "FINAL");
        assert_eq!(response.usage.input_tokens, 7);
        assert_eq!(response.usage.output_tokens, 11);
    }

    #[tokio::test]
    async fn nonstream_reasoning_only_response_fails_for_supported_aliases() {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for field in ["reasoning", "reasoning_content", "thinking"] {
            let server = MockServer::start().await;
            let mut message = serde_json::json!({"content": ""});
            message[field] = serde_json::json!("private chain of thought");
            let response = serde_json::json!({
                "choices": [{
                    "index": 0,
                    "message": message,
                    "finish_reason": "length"
                }],
                "usage": {"prompt_tokens": 7, "completion_tokens": 500},
                "model": "qwen3:8b"
            });
            Mock::given(wiremock::matchers::method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(response))
                .mount(&server)
                .await;

            let provider = OllamaProvider::with_base_url(server.uri(), "qwen3:8b");
            assert!(matches!(
                provider.chat(ChatRequest::simple("plan")).await,
                Err(ProviderError::ApiError { message, .. })
                    if message.contains("reasoning without final content")
            ));
        }
    }

    #[tokio::test]
    async fn stream_reasoning_aliases_remain_reasoning_and_final_content_remains_text() {
        use tokio_stream::StreamExt;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"hidden-a\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"hidden-b\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"thinking\":\"hidden-c\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"FINAL\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":11}}\n\n",
            "data: [DONE]\n\n"
        );
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;

        let provider = OllamaProvider::with_base_url(server.uri(), "qwen3:8b");
        let events = provider
            .chat_stream(ChatRequest::simple("plan"))
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let mut reasoning = String::new();
        let mut content = String::new();
        let mut saw_usage = false;
        let mut saw_done = false;
        for event in events {
            match event.unwrap() {
                StreamEvent::ReasoningDelta { delta } => reasoning.push_str(&delta),
                StreamEvent::TextDelta { delta } => content.push_str(&delta),
                StreamEvent::Usage(usage) => {
                    saw_usage = usage.input_tokens == 7 && usage.output_tokens == 11;
                }
                StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                } => saw_done = true,
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(reasoning, "hidden-ahidden-bhidden-c");
        assert_eq!(content, "FINAL");
        assert!(saw_usage);
        assert!(saw_done);
    }

    #[tokio::test]
    async fn stream_reasoning_only_length_keeps_usage_but_never_completes_blank() {
        use tokio_stream::StreamExt;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"hidden\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":500}}\n\n",
            "data: [DONE]\n\n"
        );
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;

        let provider = OllamaProvider::with_base_url(server.uri(), "qwen3:8b");
        let events = provider
            .chat_stream(ChatRequest::simple("plan"))
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            Ok(StreamEvent::Usage(TokenUsageStats {
                input_tokens: 7,
                output_tokens: 500,
                ..
            }))
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            Err(ProviderError::Stream(message))
                if message.contains("reasoning without final content")
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            Ok(StreamEvent::TextDelta { .. } | StreamEvent::Done { .. })
        )));
    }

    #[test]
    fn nonstream_tool_calls_require_a_compatible_terminal() {
        assert!(normalize_ollama_nonstream_finish(Some("stop"), 1, 0).is_err());
        assert!(normalize_ollama_nonstream_finish(Some("length"), 0, 1).is_err());
        assert_eq!(
            normalize_ollama_nonstream_finish(Some("stop"), 0, 1).unwrap(),
            FinishReason::ToolUse
        );
        assert_eq!(
            normalize_ollama_nonstream_finish(Some("tool_calls"), 1, 0).unwrap(),
            FinishReason::ToolUse
        );
    }

    #[tokio::test]
    async fn nonstream_json_call_floods_are_refused_before_any_call_is_returned() {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let response = serde_json::json!({
            "choices": [{
                "index": 0,
                "message": {
                    "content": recovered_tool_call_flood(129)
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1},
            "model": "local-model"
        });
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;

        let provider = OllamaProvider::with_base_url(server.uri(), "local-model");
        assert!(matches!(
            provider.chat(lookup_request()).await,
            Err(ProviderError::RefusedResponse { message, .. })
                if message.contains("lookup") && message.contains("JSON")
        ));
    }

    #[tokio::test]
    async fn stream_json_call_floods_emit_zero_actionable_events() {
        use tokio_stream::StreamExt;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let frame = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "content": recovered_tool_call_flood(129)
                },
                "finish_reason": "stop"
            }]
        });
        let sse = format!("data: {frame}\n\ndata: [DONE]\n\n");
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;

        let provider = OllamaProvider::with_base_url(server.uri(), "local-model");
        let events = provider
            .chat_stream(lookup_request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            Err(ProviderError::RefusedResponse { message, .. }) if message.contains("JSON")
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            Ok(StreamEvent::ToolCallDelta { .. }
                | StreamEvent::ToolCallMetadata { .. }
                | StreamEvent::Done { .. })
        )));
    }

    #[tokio::test]
    async fn stream_multiple_choice_alternatives_never_merge_into_parallel_calls() {
        use tokio_stream::StreamExt;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"choices\":[",
            "{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{}\"}}]},\"finish_reason\":null},",
            "{\"index\":1,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_b\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;

        let provider = OllamaProvider::with_base_url(server.uri(), "local-model");
        let mut request = ChatRequest::simple("lookup");
        request.tools = vec![axocoatl_llm::ToolDefinition {
            name: "lookup".to_string(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
            concurrency: Default::default(),
        }];
        let events = provider
            .chat_stream(request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            Err(ProviderError::Stream(message)) if message.contains("multiple alternatives")
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            Ok(StreamEvent::ToolCallDelta { .. } | StreamEvent::Done { .. })
        )));
    }

    #[tokio::test]
    async fn stream_text_tool_call_under_content_filter_is_never_actionable() {
        use tokio_stream::StreamExt;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"<tool_call>{\\\"name\\\":\\\"lookup\\\",\\\"arguments\\\":{}}</tool_call>\"},\"finish_reason\":\"content_filter\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;

        let provider = OllamaProvider::with_base_url(server.uri(), "local-model");
        let mut request = ChatRequest::simple("lookup");
        request.tools = vec![axocoatl_llm::ToolDefinition {
            name: "lookup".to_string(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
            concurrency: Default::default(),
        }];
        let events = provider
            .chat_stream(request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        // Nothing is read as a call from a filtered answer; it ends filtered.
        assert!(!events.iter().any(|event| matches!(
            event,
            Ok(StreamEvent::ToolCallDelta { .. } | StreamEvent::ToolCallMetadata { .. })
        )));
        assert!(matches!(
            events.last(),
            Some(Ok(StreamEvent::Done {
                finish_reason: FinishReason::ContentFilter
            }))
        ));
    }

    #[test]
    fn nonstream_tool_calls_fail_closed_on_malformed_or_undeclared_arguments() {
        let tools = vec![axocoatl_llm::ToolDefinition {
            name: "lookup".to_string(),
            description: String::new(),
            parameters: serde_json::json!({ "type": "object" }),
            concurrency: Default::default(),
        }];
        for (name, arguments) in [
            ("lookup", "{"),
            ("lookup", "[]"),
            ("not/declared", "{}"),
            ("", "{}"),
        ] {
            let message = serde_json::json!({
                "tool_calls": [{
                    "id": "call",
                    "function": { "name": name, "arguments": arguments }
                }]
            });
            assert!(parse_structured_tool_calls(&message, &tools).is_err());
        }
        // A well-formed call to an undeclared tool is returned for the caller
        // to answer with a tool error; it never runs.
        let undeclared = serde_json::json!({
            "tool_calls": [{
                "id": "call",
                "function": { "name": "not_declared", "arguments": "{}" }
            }]
        });
        let calls = parse_structured_tool_calls(&undeclared, &tools).unwrap();
        assert_eq!(calls[0].name, "not_declared");
        let missing_id = serde_json::json!({
            "tool_calls": [{
                "id": "",
                "function": { "name": "lookup", "arguments": "{}" }
            }]
        });
        assert!(matches!(
            parse_structured_tool_calls(&missing_id, &tools),
            Err(ProviderError::ApiError { message, .. }) if message.contains("empty id")
        ));
    }

    #[test]
    fn messages_encode_assistant_tool_calls_and_tool_result() {
        use axocoatl_core::{ChatMessage, ToolCall};

        let msgs = vec![
            ChatMessage::user("weather?"),
            ChatMessage::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    arguments: serde_json::json!({ "location": "NYC" }),
                    provider_metadata: Default::default(),
                }],
            ),
            ChatMessage::tool_result("{\"temp\":72}", "get_weather", "call_1"),
        ];
        let out = ollama_messages(&msgs);

        // Assistant turn carries OpenAI-compatible tool_calls.
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(out[1]["tool_calls"][0]["type"], "function");
        assert_eq!(out[1]["tool_calls"][0]["function"]["name"], "get_weather");
        let args = out[1]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(args).unwrap()["location"],
            "NYC"
        );

        // Tool result correlates via tool_call_id.
        assert_eq!(out[2]["role"], "tool");
        assert_eq!(out[2]["tool_call_id"], "call_1");
    }

    fn coder_request() -> ChatRequest {
        let mut request = ChatRequest::simple("Change the heading colour");
        for (name, properties, required) in [
            (
                "edit_file",
                serde_json::json!({"path":{"type":"string"},"old":{"type":"string"},
                    "new":{"type":"string"}}),
                serde_json::json!(["path", "old", "new"]),
            ),
            (
                "bash",
                serde_json::json!({"command":{"type":"string"}}),
                serde_json::json!(["command"]),
            ),
        ] {
            request.tools.push(axocoatl_llm::ToolDefinition {
                name: name.to_string(),
                description: String::new(),
                parameters: serde_json::json!({"type":"object","properties":properties,
                    "required":required}),
                concurrency: Default::default(),
            });
        }
        request
    }

    /// What qwen3-coder writes when Ollama does not lift its call.
    const COMPAT_TEXT_CALL: &str = concat!(
        "I'll update the heading.\n",
        "<function=edit_file>\n",
        "<parameter=path>\nindex.html\n</parameter>\n",
        "<parameter=old>\n  h1 { color: #fff; }\n</parameter>\n",
        "<parameter=new>\n  h1 { color: #9c27b0; font-weight: bold; }\n</parameter>\n",
        "</function>\n</tool_call>",
    );

    async fn compat_answer(content: &str) -> Result<ChatResponse, ProviderError> {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let response = serde_json::json!({
            "choices": [{"index": 0, "message": {"content": content}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1},
            "model": "local-model"
        });
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;
        OllamaProvider::with_base_url(server.uri(), "local-model")
            .chat(coder_request())
            .await
    }

    async fn compat_stream(pieces: &[&str]) -> Vec<Result<StreamEvent, ProviderError>> {
        use tokio_stream::StreamExt;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let mut sse = String::new();
        for piece in pieces {
            let frame = serde_json::json!({"choices":[{"index":0,"delta":{"content":piece},
                "finish_reason":null}]});
            sse.push_str(&format!("data: {frame}\n\n"));
        }
        sse.push_str(
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
        );
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;
        OllamaProvider::with_base_url(server.uri(), "local-model")
            .chat_stream(coder_request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await
    }

    #[tokio::test]
    async fn a_qwen_call_that_ends_the_answer_is_recovered_without_its_markup() {
        let response = compat_answer(COMPAT_TEXT_CALL).await.unwrap();
        assert_eq!(response.finish_reason, FinishReason::ToolUse);
        assert_eq!(response.content, "I'll update the heading.\n");
        assert_eq!(response.tool_calls.len(), 1);
        let call = &response.tool_calls[0];
        assert_eq!(call.name, "edit_file");
        assert!(call.id.starts_with("call_text_"), "{}", call.id);
        assert_eq!(call.arguments["path"], "index.html");
        // Inner indentation is kept (edit_file matches `old` exactly); only
        // the format's own wrapping newlines go.
        assert_eq!(call.arguments["old"], "  h1 { color: #fff; }");
        assert_eq!(
            call.arguments["new"],
            "  h1 { color: #9c27b0; font-weight: bold; }"
        );
    }

    /// A call quoted anywhere in an answer (fenced, inline, mid-sentence, in
    /// another call's value, as JSON in prose, to a tool not offered) never
    /// runs: the text stays the answer.
    #[tokio::test]
    async fn a_quoted_call_is_never_run() {
        for content in [
            "A call looks like this:\n```\n<function=bash>\n<parameter=command>\nrm -rf build\n\
             </parameter>\n</function>\n```\nThat is all.",
            "Example:\n~~~xml\n<function=bash>\n<parameter=command>\nrm -rf build\n</parameter>\n\
             </function>\n",
            "Example:\n<pre>\n<function=bash>\n<parameter=command>\nrm -rf build\n</parameter>\n\
             </function>",
            "Use <function=edit_file> when you need to change a file.",
            "The Hermes format is <tool_call>{\"name\":\"bash\",\"arguments\":{\"command\":\
             \"rm -rf build\"}}</tool_call> and that's it.",
            r#"Run {"name":"bash","arguments":{"command":"rm -rf build"}} to clean."#,
            "```json\n{\"name\":\"bash\",\"arguments\":{\"command\":\"rm -rf build\"}}\n```",
            "<function=rm_rf>\n<parameter=path>\n/\n</parameter>\n</function>",
            "Just a normal assistant reply with no tool calls at all.",
        ] {
            let response = compat_answer(content).await.unwrap();
            assert!(response.tool_calls.is_empty(), "{content}");
            assert_eq!(response.finish_reason, FinishReason::Stop, "{content}");
            assert_eq!(response.content, content);
        }
        // A call quoted in another call's value is part of that value.
        let command = "echo '<function=bash>rm -rf build</function>' >> docs/format.md";
        let nested = format!(
            "I'll document it.\n<function=bash>\n<parameter=command>\n{command}\n</parameter>\n\
             </function>"
        );
        let response = compat_answer(&nested).await.unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].arguments["command"], command);
        assert_eq!(response.content, "I'll document it.\n");
    }

    /// An answer that calls an offered tool in a way that cannot run is
    /// refused, so the turn does not end having run nothing.
    #[tokio::test]
    async fn a_call_that_cannot_run_is_refused_not_answered() {
        let flood =
            "<function=bash>\n<parameter=command>\nls\n</parameter>\n</function>\n".repeat(129);
        for (content, says) in [
            (
                "<function=bash>\n<parameter=command>\nls\n</parameter>\n</function>\n\
                 Then I will read the output.",
                "text follows the call markup",
            ),
            (
                "<function=bash>\n<parameter=command>\nls\n</function>",
                "not well formed",
            ),
            (
                r#"{"name":"bash","arguments":{"command":"ls"}}"#,
                "as JSON",
            ),
            ("I'll list it.\n{\"bash\": {\"command\": \"ls\"}}", "as JSON"),
            (
                "<tool_call>\n{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"}}\n</tool_call>",
                "written as JSON",
            ),
            (
                "<function=edit_file>\n<parameter=path>\na\n</parameter>\n</function>",
                "misses its required parameter old",
            ),
            (flood.as_str(), "more than 128 calls"),
            (
                "Listing.<function=bash>\n<parameter=command>\nls\n</parameter>\n</function>",
                "does not begin a line",
            ),
            (
                "Listing. <tool_call>{\"name\": \"bash\", \"arguments\": \"{\\\"command\\\": \
                 \\\"ls\\\"}\"}</tool_call>",
                "as JSON",
            ),
        ] {
            match compat_answer(content).await {
                Err(ProviderError::RefusedResponse { message, .. }) => {
                    assert!(message.contains(says), "{content}: {message}");
                    assert!(message.contains("Nothing from this response ran"), "{message}");
                    assert!(
                        message.ends_with("finish_reason \"stop\" and 2 tools offered"),
                        "{message}"
                    );
                }
                other => panic!("{content}: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_streamed_call_is_held_off_screen_and_recovered() {
        let split = COMPAT_TEXT_CALL.find("ction=").unwrap();
        let events = compat_stream(&[&COMPAT_TEXT_CALL[..split], &COMPAT_TEXT_CALL[split..]]).await;
        let mut text = String::new();
        for event in &events {
            if let Ok(StreamEvent::TextDelta { delta }) = event {
                assert!(!delta.contains('<'), "{delta:?}");
                text.push_str(delta);
            }
        }
        assert_eq!(text, "I'll update the heading.\n");
        assert!(events.iter().any(|event| matches!(
            event,
            Ok(StreamEvent::ToolCallDelta { name: Some(name), .. }) if name == "edit_file"
        )));
        assert!(matches!(
            events.last(),
            Some(Ok(StreamEvent::Done {
                finish_reason: FinishReason::ToolUse
            }))
        ));
    }

    /// A `<tool_call>` that ends the prose, streamed token by token with
    /// its newline at the start of the next token, never shows.
    #[tokio::test]
    async fn an_opener_streamed_token_by_token_never_shows() {
        let call = &COMPAT_TEXT_CALL[COMPAT_TEXT_CALL.find("<function=").unwrap()..];
        let opened = format!("\n\n{call}");
        let events = compat_stream(&[
            "I'll update",
            " the heading.",
            " <",
            "tool",
            "_call",
            ">",
            &opened,
        ])
        .await;
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                Ok(StreamEvent::TextDelta { delta }) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "I'll update the heading. ");
        assert!(matches!(
            events.last(),
            Some(Ok(StreamEvent::Done {
                finish_reason: FinishReason::ToolUse
            }))
        ));
    }

    /// Text held as a possible call is shown when the response fails, so
    /// the abandoned output is not lost; nothing in it runs.
    #[tokio::test]
    async fn held_text_is_shown_when_the_stream_fails() {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let frame = serde_json::json!({"choices":[{"index":0,"delta":{
            "content":"Plan.\n<function=bash>\n"},"finish_reason":null}]});
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(format!("data: {frame}\n\ndata: {{not json\n\n")),
            )
            .mount(&server)
            .await;
        let events = OllamaProvider::with_base_url(server.uri(), "local-model")
            .chat_stream(coder_request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                Ok(StreamEvent::TextDelta { delta }) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Plan.\n<function=bash>\n");
        assert!(matches!(
            events.last(),
            Some(Err(ProviderError::Stream(message))) if message.contains("invalid Ollama SSE JSON")
        ));
    }

    #[tokio::test]
    async fn a_streamed_call_that_cannot_run_is_shown_and_refused() {
        let content =
            "Listing.\n<function=bash>\n<parameter=command>\nls\n</parameter>\n</function>\nDone.";
        let events = compat_stream(&[content]).await;
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                Ok(StreamEvent::TextDelta { delta }) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, content);
        assert!(matches!(
            events.last(),
            Some(Err(ProviderError::RefusedResponse { message, .. })) if message.contains("bash")
        ));
        assert!(!events.iter().any(|event| matches!(
            event,
            Ok(StreamEvent::ToolCallDelta { .. } | StreamEvent::Done { .. })
        )));
    }

    #[test]
    fn request_body_forwards_max_tokens_and_stop_sequences() {
        let provider = OllamaProvider::new("llama3");
        let mut request = ChatRequest::simple("hello");
        request.max_tokens = Some(321);
        request.stop_sequences = vec!["END".to_string(), "STOP".to_string()];
        let body = provider.build_request_body(&request, false);
        assert_eq!(body["max_tokens"], 321);
        assert_eq!(body["stop"], serde_json::json!(["END", "STOP"]));
    }
}
