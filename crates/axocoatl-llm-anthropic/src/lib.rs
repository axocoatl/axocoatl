use std::pin::Pin;

use reqwest::header::CONTENT_TYPE;
use tokio_stream::Stream;

use axocoatl_core::{MessageContent, MessageRole, ProviderMetadata, TokenUsageStats};
use axocoatl_llm::{
    provider_tool_metadata,
    transport::{
        bounded_redacted, http_client, network_error, next_stream_item, read_error_text, read_json,
        SseDecoder, RESPONSE_TIMEOUT, STREAM_IDLE_TIMEOUT, STREAM_TOTAL_TIMEOUT,
    },
    validate_chat_response, validate_provider_request, validate_response_tool_call,
    validate_stream_terminal, ChatRequest, ChatResponse, FinishReason, LlmProvider,
    ProviderCapabilities, ProviderError, StreamEvent, ToolCall, ToolDefinition,
};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Exact native assistant content blocks for one Anthropic tool-use step.
/// Thinking-enabled Claude models require these blocks (including signatures
/// and redacted thinking) to be replayed unmodified before tool results.
const ANTHROPIC_REPLAY_BLOCKS: &str = "anthropic.assistant_content_blocks";

fn anthropic_replay_metadata(
    blocks: &[serde_json::Value],
) -> Result<ProviderMetadata, ProviderError> {
    let mut metadata = provider_tool_metadata("anthropic");
    let serialized = serde_json::to_string(blocks)?;
    if serialized.len() > axocoatl_llm::transport::MAX_RESPONSE_BODY_BYTES {
        return Err(ProviderError::Stream(
            "Anthropic replay metadata exceeded the response safety limit".to_string(),
        ));
    }
    metadata.insert(ANTHROPIC_REPLAY_BLOCKS.to_string(), serialized);
    Ok(metadata)
}

fn replay_blocks_for_message(
    tool_calls: &[ToolCall],
) -> Result<Option<Vec<serde_json::Value>>, ProviderError> {
    let mut encoded: Option<&str> = None;
    for call in tool_calls {
        if let Some(candidate) = call.provider_metadata.get(ANTHROPIC_REPLAY_BLOCKS) {
            if encoded.is_some_and(|existing| existing != candidate) {
                return Err(ProviderError::InvalidRequest {
                    provider: "anthropic".to_string(),
                    message: "assistant tool calls contain conflicting Anthropic replay blocks"
                        .to_string(),
                });
            }
            encoded = Some(candidate);
        }
    }
    let Some(encoded) = encoded else {
        return Ok(None);
    };
    if encoded.len() > axocoatl_llm::transport::MAX_RESPONSE_BODY_BYTES {
        return Err(ProviderError::InvalidRequest {
            provider: "anthropic".to_string(),
            message: "Anthropic replay blocks exceed the response safety limit".to_string(),
        });
    }
    let blocks: Vec<serde_json::Value> =
        serde_json::from_str(encoded).map_err(|_| ProviderError::InvalidRequest {
            provider: "anthropic".to_string(),
            message: "Anthropic replay blocks are not valid bounded JSON".to_string(),
        })?;
    let native_calls: Vec<&serde_json::Value> = blocks
        .iter()
        .filter(|block| block["type"] == "tool_use")
        .collect();
    let matches_calls = native_calls.len() == tool_calls.len()
        && native_calls.iter().zip(tool_calls).all(|(block, call)| {
            block["id"].as_str() == Some(call.id.as_str())
                && block["name"].as_str() == Some(call.name.as_str())
                && block.get("input") == Some(&call.arguments)
        });
    if !matches_calls {
        return Err(ProviderError::InvalidRequest {
            provider: "anthropic".to_string(),
            message: "Anthropic replay blocks do not match their assistant tool calls".to_string(),
        });
    }
    Ok(Some(blocks))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeBlockKind {
    Text,
    Thinking,
    RedactedThinking,
    ToolUse,
}

impl NativeBlockKind {
    fn from_block(block: &serde_json::Value) -> Result<Self, ProviderError> {
        match block["type"].as_str() {
            Some("text") => Ok(Self::Text),
            Some("thinking") => Ok(Self::Thinking),
            Some("redacted_thinking") => Ok(Self::RedactedThinking),
            Some("tool_use") => Ok(Self::ToolUse),
            _ => Err(ProviderError::Stream(
                "Anthropic started an unsupported or malformed content block".to_string(),
            )),
        }
    }
}

fn start_native_block(
    blocks: &mut std::collections::BTreeMap<usize, serde_json::Value>,
    open_blocks: &mut std::collections::BTreeMap<usize, NativeBlockKind>,
    index: usize,
    block: &serde_json::Value,
) -> Result<NativeBlockKind, ProviderError> {
    if blocks.contains_key(&index) || open_blocks.contains_key(&index) {
        return Err(ProviderError::Stream(
            "Anthropic repeated a content-block index".to_string(),
        ));
    }
    let kind = NativeBlockKind::from_block(block)?;
    blocks.insert(index, block.clone());
    open_blocks.insert(index, kind);
    Ok(kind)
}

fn apply_native_block_delta(
    blocks: &mut std::collections::BTreeMap<usize, serde_json::Value>,
    partial_tool_inputs: &mut std::collections::BTreeMap<usize, String>,
    open_blocks: &std::collections::BTreeMap<usize, NativeBlockKind>,
    index: usize,
    delta: &serde_json::Value,
) -> Result<(), ProviderError> {
    let kind = open_blocks.get(&index).copied().ok_or_else(|| {
        ProviderError::Stream(
            "Anthropic sent a delta for an unknown or closed content block".to_string(),
        )
    })?;
    let block = blocks.get_mut(&index).ok_or_else(|| {
        ProviderError::Stream("Anthropic sent a delta before its content block start".to_string())
    })?;
    let append_string =
        |block: &mut serde_json::Value, field: &str, value: &str| match block.get_mut(field) {
            Some(serde_json::Value::String(existing)) => existing.push_str(value),
            _ => block[field] = serde_json::json!(value),
        };
    match (kind, delta["type"].as_str()) {
        (NativeBlockKind::Text, Some("text_delta")) => {
            let text = delta["text"].as_str().ok_or_else(|| {
                ProviderError::Stream("Anthropic text delta omitted its text".to_string())
            })?;
            append_string(block, "text", text);
        }
        (NativeBlockKind::Thinking, Some("thinking_delta")) => {
            let thinking = delta["thinking"].as_str().ok_or_else(|| {
                ProviderError::Stream(
                    "Anthropic thinking delta omitted its thinking text".to_string(),
                )
            })?;
            append_string(block, "thinking", thinking);
        }
        (NativeBlockKind::Thinking, Some("signature_delta")) => {
            let signature = delta["signature"].as_str().ok_or_else(|| {
                ProviderError::Stream("Anthropic signature delta omitted its signature".to_string())
            })?;
            append_string(block, "signature", signature);
        }
        (NativeBlockKind::ToolUse, Some("input_json_delta")) => {
            let json = delta["partial_json"].as_str().ok_or_else(|| {
                ProviderError::Stream(
                    "Anthropic tool-input delta omitted its partial JSON".to_string(),
                )
            })?;
            partial_tool_inputs
                .get_mut(&index)
                .ok_or_else(|| {
                    ProviderError::Stream(
                        "Anthropic sent tool input for a non-tool content block".to_string(),
                    )
                })?
                .push_str(json);
        }
        (NativeBlockKind::Text, Some("citations_delta")) => {
            let citation = delta.get("citation").ok_or_else(|| {
                ProviderError::Stream("Anthropic citation delta omitted its citation".to_string())
            })?;
            if !block["citations"].is_array() {
                block["citations"] = serde_json::json!([]);
            }
            block["citations"]
                .as_array_mut()
                .expect("initialized as array")
                .push(citation.clone());
        }
        _ => {
            return Err(ProviderError::Stream(
                "Anthropic sent an unsupported delta for the open content-block type".to_string(),
            ));
        }
    }
    Ok(())
}

fn finish_native_block(
    blocks: &mut std::collections::BTreeMap<usize, serde_json::Value>,
    partial_tool_inputs: &mut std::collections::BTreeMap<usize, String>,
    open_blocks: &mut std::collections::BTreeMap<usize, NativeBlockKind>,
    index: usize,
) -> Result<(), ProviderError> {
    let kind = open_blocks.remove(&index).ok_or_else(|| {
        ProviderError::Stream(
            "Anthropic stopped an unknown or already-closed content block".to_string(),
        )
    })?;
    let block = blocks.get_mut(&index).ok_or_else(|| {
        ProviderError::Stream("Anthropic stopped an unknown content block".to_string())
    })?;
    if matches!(kind, NativeBlockKind::ToolUse) {
        let partial = partial_tool_inputs.remove(&index).ok_or_else(|| {
            ProviderError::Stream("Anthropic tool block lost its partial-input state".to_string())
        })?;
        if !partial.is_empty() {
            let input: serde_json::Value = serde_json::from_str(&partial).map_err(|_| {
                ProviderError::Stream(
                    "Anthropic completed a tool call with malformed input JSON".to_string(),
                )
            })?;
            if !input.is_object() {
                return Err(ProviderError::Stream(
                    "Anthropic completed a tool call with non-object input".to_string(),
                ));
            }
            block["input"] = input;
        } else if !block["input"].is_object() {
            return Err(ProviderError::Stream(
                "Anthropic completed a tool call without object input".to_string(),
            ));
        }
    } else if partial_tool_inputs.contains_key(&index) {
        return Err(ProviderError::Stream(
            "Anthropic non-tool block carried tool-input state".to_string(),
        ));
    }
    Ok(())
}

fn ensure_native_blocks_closed(
    blocks: &std::collections::BTreeMap<usize, serde_json::Value>,
    open_blocks: &std::collections::BTreeMap<usize, NativeBlockKind>,
    partial_tool_inputs: &std::collections::BTreeMap<usize, String>,
) -> Result<(), ProviderError> {
    if !open_blocks.is_empty() || !partial_tool_inputs.is_empty() {
        return Err(ProviderError::Stream(
            "Anthropic stopped before completing every content block".to_string(),
        ));
    }
    if !blocks.keys().copied().eq(0..blocks.len()) {
        return Err(ProviderError::Stream(
            "Anthropic content-block indexes were not contiguous from zero".to_string(),
        ));
    }
    Ok(())
}

fn tool_calls_from_native_blocks(
    blocks: &[serde_json::Value],
    tools: &[ToolDefinition],
) -> Result<Vec<ToolCall>, ProviderError> {
    let mut calls = Vec::new();
    for block in blocks.iter().filter(|block| block["type"] == "tool_use") {
        let id = block["id"].as_str().unwrap_or("");
        if id.is_empty() {
            return Err(ProviderError::ApiError {
                provider: "anthropic".to_string(),
                status: 200,
                message: "Anthropic returned a tool call with an empty id".to_string(),
            });
        }
        let name = block["name"].as_str().unwrap_or("");
        let arguments = &block["input"];
        validate_response_tool_call("anthropic", name, arguments, tools)?;
        calls.push(ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: arguments.clone(),
            provider_metadata: provider_tool_metadata("anthropic"),
        });
    }
    let mut ids = std::collections::HashSet::with_capacity(calls.len());
    if calls.iter().any(|call| !ids.insert(call.id.as_str())) {
        return Err(ProviderError::ApiError {
            provider: "anthropic".to_string(),
            status: 200,
            message: "Anthropic returned duplicate tool-call ids".to_string(),
        });
    }
    if let Some(first) = calls.first_mut() {
        first
            .provider_metadata
            .extend(anthropic_replay_metadata(blocks)?);
    }
    Ok(calls)
}

fn anthropic_sampling_is_unsupported(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    if model.contains("mythos") {
        return true;
    }
    ["opus", "sonnet", "haiku", "fable"]
        .iter()
        .filter_map(|family| {
            model
                .split_once(&format!("-{family}-"))
                .map(|(_, tail)| tail)
        })
        .any(|tail| {
            let mut pieces = tail.split('-');
            let major = pieces.next().and_then(|value| value.parse::<u32>().ok());
            let minor = pieces
                .next()
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(0);
            major.is_some_and(|major| major > 4 || (major == 4 && minor >= 7))
        })
}

fn parse_anthropic_finish_reason(reason: &str) -> Result<FinishReason, String> {
    match reason {
        "end_turn" | "stop_sequence" => Ok(FinishReason::Stop),
        "tool_use" => Ok(FinishReason::ToolUse),
        "max_tokens" | "model_context_window_exceeded" => Ok(FinishReason::MaxTokens),
        "refusal" => Ok(FinishReason::ContentFilter),
        other => Err(format!(
            "Anthropic returned unsupported finish reason {other}"
        )),
    }
}

fn validate_anthropic_request(request: &ChatRequest, model: &str) -> Result<(), ProviderError> {
    validate_provider_request(request, "anthropic")?;
    if request
        .temperature
        .is_some_and(|temperature| temperature > 1.0)
    {
        return Err(ProviderError::InvalidRequest {
            provider: "anthropic".to_string(),
            message: "temperature must be between zero and one for Anthropic".to_string(),
        });
    }
    if anthropic_sampling_is_unsupported(model)
        && (request.temperature.is_some() || request.top_p.is_some())
    {
        return Err(ProviderError::InvalidRequest {
            provider: "anthropic".to_string(),
            message: format!(
                "model {model} does not support temperature or top_p; leave sampling unset"
            ),
        });
    }
    Ok(())
}

/// Translate multimodal `Parts` into Anthropic's content-block array. Text
/// parts become `{"type":"text"}` blocks; data-URL images become native
/// `{"type":"image","source":{"type":"base64",…}}` blocks. URLs that aren't
/// data: are skipped (Anthropic only accepts inline base64).
fn anthropic_content_blocks(parts: &[axocoatl_core::ContentPart]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for p in parts {
        match p {
            axocoatl_core::ContentPart::Text(s) => {
                out.push(serde_json::json!({"type": "text", "text": s}));
            }
            axocoatl_core::ContentPart::Image { url, .. } => {
                if let Some(idx) = url.find("base64,") {
                    let head = &url[..idx];
                    let media_type = head
                        .trim_start_matches("data:")
                        .trim_end_matches(';')
                        .to_string();
                    let data = &url[idx + "base64,".len()..];
                    out.push(serde_json::json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": media_type,
                            "data": data,
                        }
                    }));
                }
            }
        }
    }
    out
}

/// Anthropic Claude provider using the Messages API directly via reqwest.
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl AnthropicProvider {
    fn capabilities_for_model(model: &str) -> ProviderCapabilities {
        let mut capabilities = ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            structured_output: true,
            vision: true,
            reasoning: true,
            embeddings: false,
            max_context_tokens: 0,
            max_output_tokens: 0,
        };
        match model {
            "claude-opus-4-7" => {
                capabilities.max_context_tokens = 1_000_000;
                capabilities.max_output_tokens = 128_000;
            }
            "claude-sonnet-4-6" => {
                capabilities.max_context_tokens = 1_000_000;
                capabilities.max_output_tokens = 64_000;
            }
            "claude-haiku-4-5-20251001" => {
                capabilities.max_context_tokens = 200_000;
                capabilities.max_output_tokens = 64_000;
            }
            _ => {}
        }
        capabilities
    }

    fn model_constraints_known_for(model: &str) -> bool {
        matches!(
            model,
            "claude-opus-4-7" | "claude-sonnet-4-6" | "claude-haiku-4-5-20251001"
        )
    }

    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: http_client(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    fn build_request_body(
        &self,
        request: &ChatRequest,
    ) -> Result<serde_json::Value, ProviderError> {
        self.validate_request(request)?;
        // Anthropic Messages API: system is a top-level field, not a message role
        let mut system_prompt = None;
        let mut messages = Vec::new();

        for msg in &request.messages {
            // For User messages with multimodal parts we emit Anthropic's
            // native content-array (text + image blocks). Other roles flatten.
            if matches!(msg.role, MessageRole::User) {
                if let MessageContent::Parts(parts) = &msg.content {
                    let blocks = anthropic_content_blocks(parts);
                    if !blocks.is_empty() {
                        messages.push(serde_json::json!({"role": "user", "content": blocks}));
                        continue;
                    }
                }
            }
            let text = match &msg.content {
                MessageContent::Text(s) => s.clone(),
                MessageContent::Parts(parts) => parts
                    .iter()
                    .filter_map(|p| match p {
                        axocoatl_core::ContentPart::Text(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            };

            match msg.role {
                MessageRole::System => {
                    system_prompt = Some(match system_prompt {
                        Some(previous) => format!("{previous}\n{text}"),
                        None => text,
                    });
                }
                MessageRole::User => {
                    messages.push(serde_json::json!({"role": "user", "content": text}));
                }
                MessageRole::Assistant => {
                    if msg.tool_calls.is_empty() {
                        messages.push(serde_json::json!({"role": "assistant", "content": text}));
                    } else if let Some(blocks) = replay_blocks_for_message(&msg.tool_calls)? {
                        messages.push(serde_json::json!({"role": "assistant", "content": blocks}));
                    } else {
                        // Assistant tool calls become `tool_use` content blocks
                        // (preceded by a text block only when there's prose).
                        let mut blocks: Vec<serde_json::Value> = Vec::new();
                        if !text.is_empty() {
                            blocks.push(serde_json::json!({"type": "text", "text": text}));
                        }
                        for tc in &msg.tool_calls {
                            blocks.push(serde_json::json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": tc.name,
                                "input": tc.arguments,
                            }));
                        }
                        messages.push(serde_json::json!({"role": "assistant", "content": blocks}));
                    }
                }
                MessageRole::Tool => {
                    // Anthropic tool results are `tool_result` blocks inside a
                    // *user* turn, correlated by `tool_use_id`. Multiple results
                    // from one assistant turn must share a single user message —
                    // the API requires user/assistant turns to alternate — so we
                    // merge consecutive results into the preceding tool_result
                    // turn rather than emitting a second user message.
                    let tool_use_id = msg
                        .tool_call_id
                        .clone()
                        .or_else(|| msg.name.clone())
                        .unwrap_or_default();
                    let block = serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": text,
                    });
                    let merged = messages
                        .last_mut()
                        .filter(|last| last["role"] == "user")
                        .and_then(|last| last["content"].as_array_mut())
                        .filter(|arr| arr.iter().all(|b| b["type"] == "tool_result"))
                        .map(|arr| arr.push(block.clone()))
                        .is_some();
                    if !merged {
                        messages.push(serde_json::json!({"role": "user", "content": [block]}));
                    }
                }
            }
        }

        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);
        let mut body = serde_json::json!({
            "model": model_for_call,
            "messages": messages,
            "max_tokens": request.max_tokens.unwrap_or(4096),
        });

        if let Some(sys) = system_prompt {
            body["system"] = serde_json::json!(sys);
        }
        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(top_p) = request.top_p {
            body["top_p"] = serde_json::json!(top_p);
        }
        if !request.stop_sequences.is_empty() {
            body["stop_sequences"] = serde_json::json!(&request.stop_sequences);
        }
        // Anthropic's Messages API has no native JSON mode, so enforce it by
        // instruction — appended to the system prompt (or set as one).
        if request.response_format == Some(axocoatl_core::ResponseFormat::Json) {
            const JSON_INSTRUCTION: &str =
                "Respond with only valid JSON. Do not include any other text.";
            let system = match body.get("system").and_then(|s| s.as_str()) {
                Some(existing) => format!("{existing}\n\n{JSON_INSTRUCTION}"),
                None => JSON_INSTRUCTION.to_string(),
            };
            body["system"] = serde_json::json!(system);
        }
        if !request.tools.is_empty() {
            // Anthropic Messages API tool format: {name, description, input_schema}.
            // Without this the model never receives the tools and can't call them.
            body["tools"] = serde_json::Value::Array(
                request
                    .tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "name": t.name,
                            "description": t.description,
                            "input_schema": t.parameters,
                        })
                    })
                    .collect(),
            );
        }

        Ok(body)
    }
}

#[async_trait::async_trait]
impl LlmProvider for AnthropicProvider {
    fn provider_id(&self) -> &str {
        "anthropic"
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> ProviderCapabilities {
        Self::capabilities_for_model(&self.model)
    }

    fn capabilities_for(&self, request: &ChatRequest) -> ProviderCapabilities {
        Self::capabilities_for_model(request.model_override.as_deref().unwrap_or(&self.model))
    }

    fn model_constraints_known(&self, request: &ChatRequest) -> bool {
        Self::model_constraints_known_for(request.model_override.as_deref().unwrap_or(&self.model))
    }

    fn validate_request(&self, request: &ChatRequest) -> Result<(), ProviderError> {
        let model = request.model_override.as_deref().unwrap_or(&self.model);
        validate_anthropic_request(request, model)
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        validate_provider_request(&request, self.provider_id())?;
        let body = self.build_request_body(&request)?;
        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);

        let response = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .timeout(RESPONSE_TIMEOUT)
            .send()
            .await
            .map_err(|error| network_error(&error, &[&self.api_key]))?;

        let status = response.status();
        if status == 429 {
            let retry_after_secs = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok());
            return Err(ProviderError::RateLimited {
                provider: "anthropic".to_string(),
                retry_after_secs,
            });
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ProviderError::AuthError {
                provider: "anthropic".to_string(),
            });
        }
        if status.as_u16() == 404 {
            return Err(ProviderError::ModelNotFound {
                provider: "anthropic".to_string(),
                model: model_for_call.to_string(),
            });
        }

        if !status.is_success() {
            let message = read_error_text(response, &[&self.api_key]).await;
            return Err(ProviderError::ApiError {
                provider: "anthropic".to_string(),
                status: status.as_u16(),
                message,
            });
        }

        let resp_body: serde_json::Value = read_json(response, "anthropic").await?;

        // Extract content from response
        let content = resp_body["content"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|block| {
                        if block["type"] == "text" {
                            block["text"].as_str()
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();

        // Extract tool calls
        let native_content_blocks = resp_body["content"].as_array().cloned().unwrap_or_default();
        let tool_calls = tool_calls_from_native_blocks(&native_content_blocks, &request.tools)?;

        let native_finish =
            resp_body["stop_reason"]
                .as_str()
                .ok_or_else(|| ProviderError::ApiError {
                    provider: "anthropic".to_string(),
                    status: 200,
                    message: "Anthropic response omitted its stop reason".to_string(),
                })?;
        let finish_reason = parse_anthropic_finish_reason(native_finish).map_err(|message| {
            ProviderError::ApiError {
                provider: "anthropic".to_string(),
                status: 200,
                message,
            }
        })?;

        let normalized = ChatResponse {
            content,
            tool_calls,
            finish_reason,
            usage: TokenUsageStats {
                input_tokens: resp_body["usage"]["input_tokens"].as_u64().unwrap_or(0) as usize,
                output_tokens: resp_body["usage"]["output_tokens"].as_u64().unwrap_or(0) as usize,
                reasoning_tokens: None,
            },
            model: resp_body["model"]
                .as_str()
                .unwrap_or(model_for_call)
                .to_string(),
            provider: "anthropic".to_string(),
        };
        validate_chat_response("anthropic", &normalized)?;
        Ok(normalized)
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        use std::collections::BTreeMap;
        validate_provider_request(&request, self.provider_id())?;
        let mut body = self.build_request_body(&request)?;
        body["stream"] = serde_json::json!(true);

        let request_builder = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(CONTENT_TYPE, "application/json")
            .json(&body);

        let response = tokio::time::timeout(RESPONSE_TIMEOUT, request_builder.send())
            .await
            .map_err(|_| {
                ProviderError::Network("Anthropic response headers timed out".to_string())
            })?
            .map_err(|error| network_error(&error, &[&self.api_key]))?;

        let status = response.status();
        if status == 429 {
            let retry_after_secs = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok());
            return Err(ProviderError::RateLimited {
                provider: "anthropic".to_string(),
                retry_after_secs,
            });
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ProviderError::AuthError {
                provider: "anthropic".to_string(),
            });
        }
        if status.as_u16() == 404 {
            let model = request.model_override.as_deref().unwrap_or(&self.model);
            return Err(ProviderError::ModelNotFound {
                provider: "anthropic".to_string(),
                model: model.to_string(),
            });
        }
        if !status.is_success() {
            let message = read_error_text(response, &[&self.api_key]).await;
            return Err(ProviderError::ApiError {
                provider: "anthropic".to_string(),
                status: status.as_u16(),
                message,
            });
        }

        let mut bytes = response.bytes_stream();
        let api_key = self.api_key.clone();
        let offered_tools = request.tools.clone();

        let stream = async_stream::try_stream! {
            let mut decoder = SseDecoder::provider_default();
            let mut tool_blocks: BTreeMap<usize, (String, Option<String>)> = BTreeMap::new();
            let mut native_blocks: BTreeMap<usize, serde_json::Value> = BTreeMap::new();
            let mut open_native_blocks: BTreeMap<usize, NativeBlockKind> = BTreeMap::new();
            let mut partial_tool_inputs: BTreeMap<usize, String> = BTreeMap::new();
            let mut usage = TokenUsageStats::default();
            let mut saw_usage = false;
            let mut pending_finish = None;
            let mut saw_message_stop = false;
            let total_deadline = tokio::time::Instant::now() + STREAM_TOTAL_TIMEOUT;

            'response: loop {
                let next = next_stream_item(
                    &mut bytes,
                    total_deadline,
                    STREAM_IDLE_TIMEOUT,
                    "Anthropic",
                )
                .await?;
                let reached_eof = next.is_none();
                let events = match next {
                    Some(chunk) => {
                        let chunk = chunk.map_err(|error| {
                            ProviderError::Stream(bounded_redacted(&error.to_string(), 8 * 1024, &[&api_key]))
                        })?;
                        decoder.push(&chunk)?
                    }
                    None => decoder.finish()?,
                };

                for event in events {
                    let data: serde_json::Value = serde_json::from_str(&event.data)
                        .map_err(|error| ProviderError::Stream(format!("invalid Anthropic SSE JSON: {error}")))?;

                    match data["type"].as_str() {
                            Some("content_block_start") => {
                                let block = &data["content_block"];
                                let index = data["index"].as_u64().ok_or_else(|| {
                                    ProviderError::Stream(
                                        "Anthropic content block omitted its index".to_string(),
                                    )
                                })? as usize;
                                let kind = start_native_block(
                                    &mut native_blocks,
                                    &mut open_native_blocks,
                                    index,
                                    block,
                                )?;
                                if matches!(kind, NativeBlockKind::ToolUse) {
                                    let id = block["id"].as_str().unwrap_or("");
                                    if id.is_empty() {
                                        Err(ProviderError::Stream(
                                            "Anthropic tool-use block omitted its required id".to_string(),
                                        ))?;
                                    }
                                    if tool_blocks.values().any(|(known_id, _)| known_id == id) {
                                        Err(ProviderError::Stream(
                                            "Anthropic streamed duplicate tool-call ids".to_string(),
                                        ))?;
                                    }
                                    let name = block["name"].as_str().unwrap_or("");
                                    validate_response_tool_call(
                                        "anthropic",
                                        name,
                                        &block["input"],
                                        &offered_tools,
                                    )?;
                                    let id = id.to_string();
                                    let name = name.to_string();
                                    tool_blocks.insert(index, (id.clone(), Some(name.clone())));
                                    partial_tool_inputs.insert(index, String::new());
                                    yield StreamEvent::ToolCallDelta {
                                        index: Some(index),
                                        id: id.clone(),
                                        name: Some(name),
                                        args_delta: String::new(),
                                    };
                                    yield StreamEvent::ToolCallMetadata {
                                        index: Some(index),
                                        id,
                                        metadata: provider_tool_metadata("anthropic"),
                                    };
                                }
                            }
                            Some("content_block_delta") => {
                                let index = data["index"].as_u64().ok_or_else(|| {
                                    ProviderError::Stream(
                                        "Anthropic content delta omitted its block index".to_string(),
                                    )
                                })? as usize;
                                let delta = &data["delta"];
                                apply_native_block_delta(
                                    &mut native_blocks,
                                    &mut partial_tool_inputs,
                                    &open_native_blocks,
                                    index,
                                    delta,
                                )?;
                                match delta["type"].as_str() {
                                    Some("text_delta") => {
                                        if let Some(text) = delta["text"].as_str() {
                                            yield StreamEvent::TextDelta {
                                                delta: text.to_string(),
                                            };
                                        }
                                    }
                                    Some("thinking_delta") => {
                                        if let Some(text) = delta["thinking"].as_str() {
                                            yield StreamEvent::ReasoningDelta {
                                                delta: text.to_string(),
                                            };
                                        }
                                    }
                                    Some("input_json_delta") => {
                                        if let Some(json) = delta["partial_json"].as_str() {
                                            let (id, _) = tool_blocks.get(&index).ok_or_else(|| {
                                                ProviderError::Stream("Anthropic sent tool arguments before the matching tool start".to_string())
                                            })?;
                                            yield StreamEvent::ToolCallDelta {
                                                index: Some(index),
                                                id: id.clone(),
                                                name: None,
                                                args_delta: json.to_string(),
                                            };
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Some("content_block_stop") => {
                                let index = data["index"].as_u64().ok_or_else(|| {
                                    ProviderError::Stream(
                                        "Anthropic content-block stop omitted its index".to_string(),
                                    )
                                })? as usize;
                                finish_native_block(
                                    &mut native_blocks,
                                    &mut partial_tool_inputs,
                                    &mut open_native_blocks,
                                    index,
                                )?;
                            }
                            Some("message_delta") => {
                                if let Some(delta_usage) = data.get("usage") {
                                    saw_usage = true;
                                    if let Some(output) = delta_usage["output_tokens"].as_u64() {
                                        usage.output_tokens = output as usize;
                                    }
                                }
                                if let Some(stop_reason) = data["delta"]["stop_reason"].as_str() {
                                    pending_finish = Some(
                                        parse_anthropic_finish_reason(stop_reason)
                                            .map_err(ProviderError::Stream)?,
                                    );
                                }
                            }
                            Some("message_start") => {
                                if let Some(start_usage) = data["message"]["usage"].as_object() {
                                    saw_usage = true;
                                    usage.input_tokens = start_usage.get("input_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0) as usize;
                                    usage.output_tokens = start_usage.get("output_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0) as usize;
                                }
                            }
                            Some("message_stop") => {
                                ensure_native_blocks_closed(
                                    &native_blocks,
                                    &open_native_blocks,
                                    &partial_tool_inputs,
                                )?;
                                if let Some((index, (id, _))) = tool_blocks.first_key_value() {
                                    let blocks = native_blocks.values().cloned().collect::<Vec<_>>();
                                    yield StreamEvent::ToolCallMetadata {
                                        index: Some(*index),
                                        id: id.clone(),
                                        metadata: anthropic_replay_metadata(&blocks)?,
                                    };
                                }
                                saw_message_stop = true;
                                break 'response;
                            }
                            Some("error") => {
                                let msg = data["error"]["message"]
                                    .as_str()
                                    .unwrap_or("Unknown streaming error");
                                let msg = bounded_redacted(msg, 8 * 1024, &[&api_key]);
                                Err(ProviderError::Stream(msg))?;
                            }
                            _ => {}
                        }
                }

                if reached_eof {
                    break;
                }
            }

            if !saw_message_stop {
                Err(ProviderError::Stream("Anthropic stream ended without message_stop".to_string()))?;
            }
            let finish_reason = pending_finish.ok_or_else(|| {
                ProviderError::Stream("Anthropic stream terminated without a finish reason".to_string())
            })?;
            if saw_usage {
                yield StreamEvent::Usage(usage);
            }
            validate_stream_terminal("Anthropic", &finish_reason, tool_blocks.len())?;
            yield StreamEvent::Done { finish_reason };
        };

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_tool() -> ToolDefinition {
        ToolDefinition {
            name: "lookup".to_string(),
            description: "Look up a value".to_string(),
            parameters: serde_json::json!({"type": "object"}),
            concurrency: Default::default(),
        }
    }

    #[test]
    fn build_request_body_with_system() {
        let provider = AnthropicProvider::new("test-key", "claude-sonnet-4-6");
        let request = ChatRequest::with_system("You are helpful.", "Hello");
        let body = provider.build_request_body(&request).unwrap();

        assert_eq!(body["system"], "You are helpful.");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "Hello");
        assert_eq!(body["model"], "claude-sonnet-4-6");
    }

    #[test]
    fn build_request_body_json_mode_appends_instruction() {
        let provider = AnthropicProvider::new("test-key", "claude-sonnet-4-6");
        let mut request = ChatRequest::with_system("You are helpful.", "Hello");
        request.response_format = Some(axocoatl_core::ResponseFormat::Json);
        let body = provider.build_request_body(&request).unwrap();
        // Anthropic has no native JSON mode → the instruction folds into system.
        let system = body["system"].as_str().unwrap();
        assert!(system.starts_with("You are helpful."));
        assert!(system.contains("valid JSON"));
    }

    #[test]
    fn build_request_body_forwards_top_p() {
        let provider = AnthropicProvider::new("test-key", "claude-sonnet-4-6");
        let mut request = ChatRequest::simple("Hi");
        request.top_p = Some(0.5);
        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(body["top_p"], 0.5);
    }

    #[test]
    fn current_anthropic_models_reject_sampling_locally() {
        for model in ["claude-opus-4-7", "claude-opus-5", "claude-mythos-preview"] {
            let provider = AnthropicProvider::new("key", model);
            let mut request = ChatRequest::simple("Hi");
            request.temperature = Some(0.5);
            let error = provider.build_request_body(&request).unwrap_err();
            assert!(matches!(
                error,
                ProviderError::InvalidRequest { message, .. }
                    if message.contains("leave sampling unset")
            ));
        }
    }

    #[test]
    fn older_anthropic_temperature_range_is_checked_locally() {
        let provider = AnthropicProvider::new("key", "claude-sonnet-4-6");
        let mut request = ChatRequest::simple("Hi");
        request.temperature = Some(1.5);
        assert!(matches!(
            provider.build_request_body(&request),
            Err(ProviderError::InvalidRequest { message, .. })
                if message.contains("between zero and one")
        ));
    }

    #[test]
    fn anthropic_finish_reason_mapping_fails_closed_on_unknown_values() {
        assert_eq!(
            parse_anthropic_finish_reason("stop_sequence").unwrap(),
            FinishReason::Stop
        );
        assert_eq!(
            parse_anthropic_finish_reason("refusal").unwrap(),
            FinishReason::ContentFilter
        );
        assert!(parse_anthropic_finish_reason("pause_turn").is_err());
        assert!(parse_anthropic_finish_reason("future_reason").is_err());
    }

    #[test]
    fn build_request_body_no_system() {
        let provider = AnthropicProvider::new("test-key", "claude-haiku-4-5-20251001");
        let request = ChatRequest::simple("Hi");
        let body = provider.build_request_body(&request).unwrap();

        assert!(body.get("system").is_none());
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn build_request_body_includes_tools() {
        let provider = AnthropicProvider::new("test-key", "claude-sonnet-4-6");
        let mut request = ChatRequest::simple("What's the weather in NYC?");
        request.tools = vec![axocoatl_llm::ToolDefinition {
            name: "get_weather".to_string(),
            description: "Get current weather".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "location": { "type": "string" } },
                "required": ["location"]
            }),
            concurrency: Default::default(),
        }];
        let body = provider.build_request_body(&request).unwrap();

        // Regression: tools must reach the outbound Anthropic request.
        assert!(body["tools"].is_array());
        assert_eq!(body["tools"][0]["name"], "get_weather");
        assert_eq!(body["tools"][0]["input_schema"]["required"][0], "location");
    }

    #[test]
    fn build_request_body_omits_tools_when_none() {
        let provider = AnthropicProvider::new("test-key", "claude-sonnet-4-6");
        let request = ChatRequest::simple("Hello");
        let body = provider.build_request_body(&request).unwrap();

        assert!(body.get("tools").is_none());
    }

    #[test]
    fn capabilities_correct() {
        let provider = AnthropicProvider::new("key", "claude-sonnet-4-6");
        let caps = provider.capabilities();
        assert!(caps.vision);
        assert!(caps.tool_calling);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert!(provider.model_constraints_known(&ChatRequest::simple("test")));
    }

    #[test]
    fn model_override_drives_exact_anthropic_capabilities() {
        let provider = AnthropicProvider::new("key", "claude-haiku-4-5-20251001");
        assert!(provider.capabilities().vision);
        assert_eq!(provider.capabilities().max_context_tokens, 200_000);
        let mut request = ChatRequest::simple("look");
        request.model_override = Some("claude-opus-4-7".to_string());
        let capabilities = provider.capabilities_for(&request);
        assert!(capabilities.vision);
        assert!(capabilities.reasoning);
    }

    #[test]
    fn assistant_tool_calls_become_tool_use_blocks() {
        use axocoatl_core::{ChatMessage, ToolCall};
        let provider = AnthropicProvider::new("key", "claude-sonnet-4-6");
        let mut request = ChatRequest::simple("weather?");
        request
            .messages
            .push(ChatMessage::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "toolu_1".to_string(),
                    name: "get_weather".to_string(),
                    arguments: serde_json::json!({ "location": "NYC" }),
                    provider_metadata: Default::default(),
                }],
            ));
        let body = provider.build_request_body(&request).unwrap();

        let assistant = &body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"][0]["type"], "tool_use");
        assert_eq!(assistant["content"][0]["id"], "toolu_1");
        assert_eq!(assistant["content"][0]["name"], "get_weather");
        assert_eq!(assistant["content"][0]["input"]["location"], "NYC");
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_turn() {
        use axocoatl_core::{ChatMessage, ToolCall};
        let provider = AnthropicProvider::new("key", "claude-sonnet-4-6");
        let mut request = ChatRequest::simple("compare NYC and LA");
        request
            .messages
            .push(ChatMessage::assistant_with_tool_calls(
                "",
                vec![
                    ToolCall {
                        id: "toolu_1".to_string(),
                        name: "get_weather".to_string(),
                        arguments: serde_json::json!({ "location": "NYC" }),
                        provider_metadata: Default::default(),
                    },
                    ToolCall {
                        id: "toolu_2".to_string(),
                        name: "get_weather".to_string(),
                        arguments: serde_json::json!({ "location": "LA" }),
                        provider_metadata: Default::default(),
                    },
                ],
            ));
        request
            .messages
            .push(ChatMessage::tool_result("72F", "get_weather", "toolu_1"));
        request
            .messages
            .push(ChatMessage::tool_result("80F", "get_weather", "toolu_2"));
        let body = provider.build_request_body(&request).unwrap();

        let msgs = body["messages"].as_array().unwrap();
        // user, assistant(tool_use x2), user(tool_result x2) — exactly 3 turns,
        // not 4: the two results share one user turn so roles still alternate.
        assert_eq!(msgs.len(), 3);
        let results_turn = &msgs[2];
        assert_eq!(results_turn["role"], "user");
        let blocks = results_turn["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "toolu_1");
        assert_eq!(blocks[1]["tool_use_id"], "toolu_2");
    }

    #[test]
    fn request_body_forwards_max_tokens_and_stop_sequences() {
        let provider = AnthropicProvider::new("key", "claude-sonnet-4-6");
        let mut request = ChatRequest::simple("hello");
        request.max_tokens = Some(321);
        request.stop_sequences = vec!["END".to_string(), "STOP".to_string()];
        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(body["max_tokens"], 321);
        assert_eq!(body["stop_sequences"], serde_json::json!(["END", "STOP"]));
    }

    #[test]
    fn response_text_blocks_are_joined_in_order() {
        let response = serde_json::json!({
            "content": [
                {"type": "text", "text": "first"},
                {"type": "tool_use", "id": "x", "name": "tool", "input": {}},
                {"type": "text", "text": " second"}
            ]
        });
        let content = response["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| {
                        if block["type"] == "text" {
                            block["text"].as_str()
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap();
        assert_eq!(content, "first second");
    }

    #[test]
    fn nonstream_thinking_tool_response_replays_native_blocks_exactly() {
        use axocoatl_core::ChatMessage;

        let native = vec![
            serde_json::json!({
                "type": "thinking",
                "thinking": "private reasoning",
                "signature": "opaque-signature+/="
            }),
            serde_json::json!({
                "type": "redacted_thinking",
                "data": "opaque-redacted-data"
            }),
            serde_json::json!({
                "type": "tool_use",
                "id": "toolu_thinking",
                "name": "lookup",
                "input": { "query": "launch" }
            }),
        ];
        let calls = tool_calls_from_native_blocks(&native, &[lookup_tool()]).unwrap();
        assert!(calls[0]
            .provider_metadata
            .contains_key(ANTHROPIC_REPLAY_BLOCKS));

        let provider = AnthropicProvider::new("key", "claude-sonnet-4-7");
        let mut request = ChatRequest::simple("research launch");
        request
            .messages
            .push(ChatMessage::assistant_with_tool_calls("", calls));
        request
            .messages
            .push(ChatMessage::tool_result("{}", "lookup", "toolu_thinking"));
        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(body["messages"][1]["content"], serde_json::json!(native));
    }

    #[test]
    fn streamed_signature_deltas_and_redacted_blocks_replay_exactly() {
        use axocoatl_core::ChatMessage;
        use std::collections::BTreeMap;

        let mut blocks = BTreeMap::new();
        let mut partial_inputs = BTreeMap::new();
        let mut open_blocks = BTreeMap::new();
        start_native_block(
            &mut blocks,
            &mut open_blocks,
            0,
            &serde_json::json!({ "type": "thinking", "thinking": "" }),
        )
        .unwrap();
        apply_native_block_delta(
            &mut blocks,
            &mut partial_inputs,
            &open_blocks,
            0,
            &serde_json::json!({ "type": "thinking_delta", "thinking": "reason" }),
        )
        .unwrap();
        apply_native_block_delta(
            &mut blocks,
            &mut partial_inputs,
            &open_blocks,
            0,
            &serde_json::json!({ "type": "signature_delta", "signature": "opaque-" }),
        )
        .unwrap();
        apply_native_block_delta(
            &mut blocks,
            &mut partial_inputs,
            &open_blocks,
            0,
            &serde_json::json!({ "type": "signature_delta", "signature": "signature" }),
        )
        .unwrap();
        finish_native_block(&mut blocks, &mut partial_inputs, &mut open_blocks, 0).unwrap();
        start_native_block(
            &mut blocks,
            &mut open_blocks,
            1,
            &serde_json::json!({
                "type": "redacted_thinking",
                "data": "redacted-payload"
            }),
        )
        .unwrap();
        finish_native_block(&mut blocks, &mut partial_inputs, &mut open_blocks, 1).unwrap();
        start_native_block(
            &mut blocks,
            &mut open_blocks,
            2,
            &serde_json::json!({
                "type": "tool_use",
                "id": "toolu_stream",
                "name": "lookup",
                "input": {}
            }),
        )
        .unwrap();
        partial_inputs.insert(2, String::new());
        apply_native_block_delta(
            &mut blocks,
            &mut partial_inputs,
            &open_blocks,
            2,
            &serde_json::json!({
                "type": "input_json_delta",
                "partial_json": "{\"query\":\"launch\"}"
            }),
        )
        .unwrap();
        finish_native_block(&mut blocks, &mut partial_inputs, &mut open_blocks, 2).unwrap();
        assert!(open_blocks.is_empty());
        let native = blocks.values().cloned().collect::<Vec<_>>();
        assert_eq!(native[0]["signature"], "opaque-signature");

        let calls = tool_calls_from_native_blocks(&native, &[lookup_tool()]).unwrap();
        let provider = AnthropicProvider::new("key", "claude-sonnet-4-7");
        let mut request = ChatRequest::simple("research launch");
        request
            .messages
            .push(ChatMessage::assistant_with_tool_calls("", calls));
        request
            .messages
            .push(ChatMessage::tool_result("{}", "lookup", "toolu_stream"));
        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(body["messages"][1]["content"], serde_json::json!(native));
    }

    #[test]
    fn native_stream_block_lifecycle_fails_closed_on_malformed_sequences() {
        use std::collections::BTreeMap;

        let mut blocks = BTreeMap::new();
        let mut partial_inputs = BTreeMap::new();
        let mut open_blocks = BTreeMap::new();
        start_native_block(
            &mut blocks,
            &mut open_blocks,
            0,
            &serde_json::json!({"type": "text", "text": ""}),
        )
        .unwrap();

        assert!(matches!(
            apply_native_block_delta(
                &mut blocks,
                &mut partial_inputs,
                &open_blocks,
                0,
                &serde_json::json!({"type": "thinking_delta", "thinking": "wrong"}),
            ),
            Err(ProviderError::Stream(message)) if message.contains("unsupported delta")
        ));
        assert!(matches!(
            apply_native_block_delta(
                &mut blocks,
                &mut partial_inputs,
                &open_blocks,
                0,
                &serde_json::json!({}),
            ),
            Err(ProviderError::Stream(message)) if message.contains("unsupported delta")
        ));
        assert!(matches!(
            ensure_native_blocks_closed(&blocks, &open_blocks, &partial_inputs),
            Err(ProviderError::Stream(message)) if message.contains("every content block")
        ));

        finish_native_block(&mut blocks, &mut partial_inputs, &mut open_blocks, 0).unwrap();
        assert!(ensure_native_blocks_closed(&blocks, &open_blocks, &partial_inputs).is_ok());
        assert!(matches!(
            finish_native_block(&mut blocks, &mut partial_inputs, &mut open_blocks, 0),
            Err(ProviderError::Stream(message)) if message.contains("already-closed")
        ));
        assert!(matches!(
            finish_native_block(&mut blocks, &mut partial_inputs, &mut open_blocks, 99),
            Err(ProviderError::Stream(message)) if message.contains("unknown")
        ));
        assert!(matches!(
            start_native_block(
                &mut blocks,
                &mut open_blocks,
                1,
                &serde_json::json!({"type": "future_block"}),
            ),
            Err(ProviderError::Stream(message)) if message.contains("unsupported")
        ));

        let mut starts_at_one = BTreeMap::new();
        let mut starts_at_one_open = BTreeMap::new();
        start_native_block(
            &mut starts_at_one,
            &mut starts_at_one_open,
            1,
            &serde_json::json!({"type": "text", "text": "one"}),
        )
        .unwrap();
        finish_native_block(
            &mut starts_at_one,
            &mut partial_inputs,
            &mut starts_at_one_open,
            1,
        )
        .unwrap();
        assert!(matches!(
            ensure_native_blocks_closed(&starts_at_one, &starts_at_one_open, &partial_inputs),
            Err(ProviderError::Stream(message)) if message.contains("contiguous")
        ));

        let mut has_gap = BTreeMap::new();
        let mut has_gap_open = BTreeMap::new();
        for index in [0, 2] {
            start_native_block(
                &mut has_gap,
                &mut has_gap_open,
                index,
                &serde_json::json!({"type": "text", "text": "part"}),
            )
            .unwrap();
            finish_native_block(&mut has_gap, &mut partial_inputs, &mut has_gap_open, index)
                .unwrap();
        }
        assert!(matches!(
            ensure_native_blocks_closed(&has_gap, &has_gap_open, &partial_inputs),
            Err(ProviderError::Stream(message)) if message.contains("contiguous")
        ));
    }

    #[test]
    fn nonstream_tool_calls_reject_invalid_ids_names_and_inputs() {
        let valid = serde_json::json!({
            "type": "tool_use",
            "id": "toolu_1",
            "name": "lookup",
            "input": {}
        });
        for invalid in [
            serde_json::json!({"type":"tool_use","id":"","name":"lookup","input":{}}),
            serde_json::json!({"type":"tool_use","id":"toolu_1","name":"","input":{}}),
            serde_json::json!({"type":"tool_use","id":"toolu_1","name":"other","input":{}}),
            serde_json::json!({"type":"tool_use","id":"toolu_1","name":"lookup","input":null}),
            serde_json::json!({"type":"tool_use","id":"toolu_1","name":"lookup","input":7}),
        ] {
            assert!(tool_calls_from_native_blocks(&[invalid], &[lookup_tool()]).is_err());
        }
        assert!(tool_calls_from_native_blocks(&[valid], &[lookup_tool()]).is_ok());
    }

    #[test]
    fn reconstructed_parallel_calls_replay_one_exact_native_assistant_group() {
        use axocoatl_core::ChatMessage;

        let native = vec![
            serde_json::json!({
                "type": "thinking",
                "thinking": "compare both",
                "signature": "opaque-parallel-signature"
            }),
            serde_json::json!({
                "type": "tool_use",
                "id": "toolu_a",
                "name": "lookup",
                "input": {"query": "a"}
            }),
            serde_json::json!({
                "type": "tool_use",
                "id": "toolu_b",
                "name": "lookup",
                "input": {"query": "b"}
            }),
        ];
        let calls = tool_calls_from_native_blocks(&native, &[lookup_tool()]).unwrap();
        assert_eq!(calls.len(), 2);

        let provider = AnthropicProvider::new("key", "claude-sonnet-4-6");
        let mut request = ChatRequest::simple("compare a and b");
        request.tools = vec![lookup_tool()];
        request
            .messages
            .push(ChatMessage::assistant_with_tool_calls("", calls));
        request
            .messages
            .push(ChatMessage::tool_result("{}", "lookup", "toolu_a"));
        request
            .messages
            .push(ChatMessage::tool_result("{}", "lookup", "toolu_b"));

        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(body["messages"][1]["content"], serde_json::json!(native));
        assert_eq!(body["messages"][2]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn arbitrary_anthropic_model_constraints_remain_unknown() {
        let provider = AnthropicProvider::new("key", "claude-private-future");
        let request = ChatRequest::simple("test");
        assert!(!provider.model_constraints_known(&request));
        assert_eq!(provider.capabilities_for(&request).max_context_tokens, 0);
        assert_eq!(provider.capabilities_for(&request).max_output_tokens, 0);
    }
}
