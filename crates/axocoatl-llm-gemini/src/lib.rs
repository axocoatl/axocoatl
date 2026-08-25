//! Google Gemini provider — uses the Gemini REST API (generateContent).

use std::pin::Pin;

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

/// Gemini REST returns this opaque value on the content part containing a
/// function call. Gemini 3 rejects the next function-response request unless
/// the signature is replayed on that exact function-call part.
const GEMINI_THOUGHT_SIGNATURE: &str = "gemini.thought_signature";
/// Exact ordered native assistant parts for a tool-bearing Gemini response.
/// This retains signatures on text/thought parts as well as function calls.
const GEMINI_REPLAY_PARTS: &str = "gemini.assistant_content_parts";

fn gemini_tool_metadata(part: &serde_json::Value) -> ProviderMetadata {
    let mut metadata = provider_tool_metadata("gemini");
    if let Some(signature) = part
        .get("thoughtSignature")
        .and_then(|value| value.as_str())
    {
        metadata.insert(GEMINI_THOUGHT_SIGNATURE.to_string(), signature.to_string());
    }
    metadata
}

fn gemini_replay_metadata(parts: &[serde_json::Value]) -> Result<ProviderMetadata, ProviderError> {
    let mut metadata = provider_tool_metadata("gemini");
    let encoded = serde_json::to_string(parts)?;
    if encoded.len() > axocoatl_llm::transport::MAX_RESPONSE_BODY_BYTES {
        return Err(ProviderError::Stream(
            "Gemini replay parts exceeded the response safety limit".to_string(),
        ));
    }
    metadata.insert(GEMINI_REPLAY_PARTS.to_string(), encoded);
    Ok(metadata)
}

fn replay_parts_for_message(
    content: &str,
    tool_calls: &[ToolCall],
) -> Result<Option<Vec<serde_json::Value>>, ProviderError> {
    let mut encoded: Option<&str> = None;
    for call in tool_calls {
        if let Some(candidate) = call.provider_metadata.get(GEMINI_REPLAY_PARTS) {
            if encoded.is_some_and(|existing| existing != candidate) {
                return Err(ProviderError::InvalidRequest {
                    provider: "gemini".to_string(),
                    message: "assistant tool calls contain conflicting Gemini replay parts"
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
            provider: "gemini".to_string(),
            message: "Gemini replay parts exceed the response safety limit".to_string(),
        });
    }
    let parts: Vec<serde_json::Value> =
        serde_json::from_str(encoded).map_err(|_| ProviderError::InvalidRequest {
            provider: "gemini".to_string(),
            message: "Gemini replay parts are not valid bounded JSON".to_string(),
        })?;
    let native_calls = parts
        .iter()
        .filter_map(|part| part.get("functionCall"))
        .collect::<Vec<_>>();
    let calls_match = native_calls.len() == tool_calls.len()
        && native_calls.iter().zip(tool_calls).all(|(native, call)| {
            native["id"].as_str().unwrap_or("") == call.id
                && native["name"].as_str() == Some(call.name.as_str())
                && native.get("args") == Some(&call.arguments)
        });
    let replay_text = parts
        .iter()
        .filter(|part| part["thought"].as_bool() != Some(true))
        .filter_map(|part| part["text"].as_str())
        .collect::<String>();
    if !calls_match || replay_text != content {
        return Err(ProviderError::InvalidRequest {
            provider: "gemini".to_string(),
            message: "Gemini replay parts do not match their assistant message".to_string(),
        });
    }
    Ok(Some(parts))
}

fn gemini_tool_call_from_part(
    part: &serde_json::Value,
    tools: &[ToolDefinition],
) -> Result<Option<ToolCall>, ProviderError> {
    let Some(function_call) = part.get("functionCall") else {
        return Ok(None);
    };
    let name = function_call["name"].as_str().unwrap_or("");
    validate_response_tool_call("gemini", name, &function_call["args"], tools)?;
    Ok(Some(ToolCall {
        id: function_call["id"].as_str().unwrap_or("").to_string(),
        name: name.to_string(),
        arguments: function_call["args"].clone(),
        provider_metadata: gemini_tool_metadata(part),
    }))
}

fn gemini_tool_calls_from_parts(
    parts: &[serde_json::Value],
    tools: &[ToolDefinition],
) -> Result<Vec<ToolCall>, ProviderError> {
    let mut calls = Vec::new();
    for part in parts {
        if let Some(call) = gemini_tool_call_from_part(part, tools)? {
            calls.push(call);
        }
    }
    if let Some(first) = calls.first_mut() {
        first
            .provider_metadata
            .extend(gemini_replay_metadata(parts)?);
    }
    Ok(calls)
}

fn normalize_gemini_finish_reason(
    finish_reason: FinishReason,
    tool_call_count: usize,
) -> FinishReason {
    if tool_call_count > 0 && matches!(&finish_reason, FinishReason::Stop) {
        FinishReason::ToolUse
    } else {
        finish_reason
    }
}

fn parse_gemini_finish_reason(reason: &str) -> Result<FinishReason, String> {
    match reason {
        "STOP" => Ok(FinishReason::Stop),
        "MAX_TOKENS" => Ok(FinishReason::MaxTokens),
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY" => {
            Ok(FinishReason::ContentFilter)
        }
        other => Err(format!(
            "Gemini returned unsuccessful or unsupported finish reason {other}"
        )),
    }
}

/// Flatten a message's content down to plain text (Gemini system/assistant text
/// and tool-result fallbacks).
fn flatten_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                axocoatl_core::ContentPart::Text(s) => Some(s.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Convert tool definitions into Gemini's `tools` array. Gemini groups all
/// declarations under a single `functionDeclarations` entry.
fn gemini_tools(tools: &[ToolDefinition]) -> serde_json::Value {
    serde_json::json!([{
        "functionDeclarations": tools
            .iter()
            .map(|t| serde_json::json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            }))
            .collect::<Vec<_>>()
    }])
}

/// Translate `ContentPart`s into Gemini's native parts array. Text becomes
/// `{"text": "..."}`; data-URL images become `{"inline_data": { mime_type, data }}`.
fn gemini_parts(parts: &[axocoatl_core::ContentPart]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for p in parts {
        match p {
            axocoatl_core::ContentPart::Text(s) => {
                out.push(serde_json::json!({"text": s}));
            }
            axocoatl_core::ContentPart::Image { url, .. } => {
                if let Some(idx) = url.find("base64,") {
                    let head = &url[..idx];
                    let mime = head
                        .trim_start_matches("data:")
                        .trim_end_matches(';')
                        .to_string();
                    let data = &url[idx + "base64,".len()..];
                    out.push(serde_json::json!({
                        "inline_data": { "mime_type": mime, "data": data }
                    }));
                }
            }
        }
    }
    out
}

// Function calling (the `tools` / `functionDeclarations` field) and
// `systemInstruction` are only served by the `v1beta` endpoint — the `v1`
// endpoint rejects `tools` with `Unknown name "tools"`. `v1beta` serves the
// current models too (verified against `gemini-2.5-flash`), so it's the right
// base for a tool-capable provider.
const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// Parse one Gemini streaming chunk (a `GenerateContentResponse`) into stream
/// events. Pure + synchronous so it is unit-tested without the network.
fn parse_gemini_chunk_with_index(
    data: &serde_json::Value,
    next_function_call_index: &mut usize,
    tools: &[ToolDefinition],
) -> Result<Vec<StreamEvent>, ProviderError> {
    let mut events = Vec::new();
    let candidates = data["candidates"].as_array().ok_or_else(|| {
        ProviderError::Stream("Gemini stream frame omitted candidates".to_string())
    })?;
    if candidates.len() != 1
        || candidates[0]
            .get("index")
            .is_some_and(|index| index.as_u64() != Some(0))
    {
        return Err(ProviderError::Stream(
            "Gemini stream frame must contain exactly one candidate at index zero".to_string(),
        ));
    }
    let candidate = &candidates[0];

    if let Some(parts) = candidate["content"]["parts"].as_array() {
        for part in parts {
            if let Some(text) = part["text"].as_str() {
                if !text.is_empty() {
                    events.push(if part["thought"].as_bool() == Some(true) {
                        StreamEvent::ReasoningDelta {
                            delta: text.to_string(),
                        }
                    } else {
                        StreamEvent::TextDelta {
                            delta: text.to_string(),
                        }
                    });
                }
            }
            // Gemini emits each function call as a complete `functionCall` part
            // (arguments are never fragmented), so one delta carries the whole
            // call. No `index` and a possibly-empty `id` means the accumulator
            // keeps each call as its own entry — exactly what we want.
            if let Some(call) = gemini_tool_call_from_part(part, tools)? {
                let index = *next_function_call_index;
                *next_function_call_index = (*next_function_call_index).saturating_add(1);
                events.push(StreamEvent::ToolCallDelta {
                    // Gemini commonly omits call ids. A local index keeps
                    // parallel calls and their metadata unambiguous while
                    // the stream is normalized by the actor.
                    index: Some(index),
                    id: call.id.clone(),
                    name: Some(call.name),
                    args_delta: serde_json::to_string(&call.arguments)
                        .unwrap_or_else(|_| "{}".to_string()),
                });
                events.push(StreamEvent::ToolCallMetadata {
                    index: Some(index),
                    id: call.id,
                    metadata: call.provider_metadata,
                });
            }
        }
    }

    if let Some(usage) = data.get("usageMetadata") {
        events.push(StreamEvent::Usage(TokenUsageStats {
            input_tokens: usage["promptTokenCount"].as_u64().unwrap_or(0) as usize,
            output_tokens: usage["candidatesTokenCount"].as_u64().unwrap_or(0) as usize,
            reasoning_tokens: None,
        }));
    }

    if let Some(reason) = candidate["finishReason"].as_str() {
        let finish = parse_gemini_finish_reason(reason).map_err(ProviderError::Stream)?;
        events.push(StreamEvent::Done {
            finish_reason: finish,
        });
    }

    Ok(events)
}

#[cfg(test)]
fn parse_gemini_chunk(
    data: &serde_json::Value,
    tools: &[ToolDefinition],
) -> Result<Vec<StreamEvent>, ProviderError> {
    let mut next_function_call_index = 0;
    parse_gemini_chunk_with_index(data, &mut next_function_call_index, tools)
}

fn parse_gemini_sse_data(
    data: &str,
    next_function_call_index: &mut usize,
    tools: &[ToolDefinition],
) -> Result<(bool, Vec<StreamEvent>, Vec<serde_json::Value>), ProviderError> {
    if data.trim() == "[DONE]" {
        return Ok((true, Vec::new(), Vec::new()));
    }
    let data: serde_json::Value = serde_json::from_str(data)
        .map_err(|error| ProviderError::Stream(format!("invalid Gemini SSE JSON: {error}")))?;
    let parts = data["candidates"][0]["content"]["parts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    Ok((
        false,
        parse_gemini_chunk_with_index(&data, next_function_call_index, tools)?,
        parts,
    ))
}

/// Google Gemini provider using the generateContent REST API.
pub struct GeminiProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl GeminiProvider {
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
        if model == "gemini-2.5-flash" {
            capabilities.max_context_tokens = 1_048_576;
            capabilities.max_output_tokens = 65_536;
        }
        capabilities
    }

    fn model_constraints_known_for(model: &str) -> bool {
        model == "gemini-2.5-flash"
    }

    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: http_client(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    /// REST endpoint for a model. The API key is **not** in the URL — it's sent
    /// in the `x-goog-api-key` header (see [`chat`]) so it can't leak into
    /// reqwest's network-error strings or any log line that prints the URL.
    fn endpoint_for(&self, model: &str) -> String {
        format!("{GEMINI_API_BASE}/{model}:generateContent")
    }

    /// SSE streaming endpoint (`streamGenerateContent?alt=sse`).
    fn stream_endpoint_for(&self, model: &str) -> String {
        format!("{GEMINI_API_BASE}/{model}:streamGenerateContent?alt=sse")
    }

    /// Build the Gemini request body (`contents` + `generationConfig`), shared
    /// by `chat` and `chat_stream`. System messages map to the native
    /// `systemInstruction` field (supported by the `v1beta` endpoint).
    fn build_request_body(
        &self,
        request: &ChatRequest,
    ) -> Result<serde_json::Value, ProviderError> {
        // Gemini uses a different message format: "contents" with "parts".
        let mut system_text: Option<String> = None;
        let mut contents: Vec<serde_json::Value> = Vec::new();
        // Gemini requires parallel function responses from one model step to
        // be consecutive parts of one user Content, not separate user turns.
        let mut pending_tool_parts: Vec<serde_json::Value> = Vec::new();

        for msg in &request.messages {
            if !matches!(msg.role, MessageRole::Tool) && !pending_tool_parts.is_empty() {
                contents.push(serde_json::json!({
                    "role": "user",
                    "parts": std::mem::take(&mut pending_tool_parts),
                }));
            }
            match msg.role {
                MessageRole::System => {
                    // Accumulate; emitted as a top-level systemInstruction below.
                    let text = flatten_text(&msg.content);
                    system_text = Some(match system_text {
                        Some(prev) => format!("{prev}\n{text}"),
                        None => text,
                    });
                }
                MessageRole::User => {
                    // Native parts array for multimodal: text + inline_data.
                    let parts = if let MessageContent::Parts(parts) = &msg.content {
                        gemini_parts(parts)
                    } else {
                        vec![serde_json::json!({ "text": flatten_text(&msg.content) })]
                    };
                    contents.push(serde_json::json!({ "role": "user", "parts": parts }));
                }
                MessageRole::Assistant => {
                    // A `model` turn: optional text, then a `functionCall` part
                    // per requested tool call so the model sees its own calls.
                    let text = flatten_text(&msg.content);
                    let parts =
                        if let Some(parts) = replay_parts_for_message(&text, &msg.tool_calls)? {
                            parts
                        } else {
                            let mut parts: Vec<serde_json::Value> = Vec::new();
                            if !text.is_empty() {
                                parts.push(serde_json::json!({ "text": text }));
                            }
                            for tc in &msg.tool_calls {
                                let mut fc =
                                    serde_json::json!({ "name": tc.name, "args": tc.arguments });
                                if !tc.id.is_empty() {
                                    fc["id"] = serde_json::json!(tc.id);
                                }
                                let mut part = serde_json::json!({ "functionCall": fc });
                                if let Some(signature) =
                                    tc.provider_metadata.get(GEMINI_THOUGHT_SIGNATURE)
                                {
                                    part["thoughtSignature"] = serde_json::json!(signature);
                                }
                                parts.push(part);
                            }
                            // Gemini rejects an empty parts array; guarantee one part.
                            if parts.is_empty() {
                                parts.push(serde_json::json!({ "text": "" }));
                            }
                            parts
                        };
                    contents.push(serde_json::json!({ "role": "model", "parts": parts }));
                }
                MessageRole::Tool => {
                    // Gemini function results travel in a `user` turn as a
                    // `functionResponse` part, correlated by function name. The
                    // `response` field must be an object — wrap non-objects.
                    let name = msg.name.clone().unwrap_or_default();
                    let text = flatten_text(&msg.content);
                    let parsed: serde_json::Value =
                        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
                    let response_obj = if parsed.is_object() {
                        parsed
                    } else {
                        serde_json::json!({ "result": parsed })
                    };
                    let mut fr = serde_json::json!({ "name": name, "response": response_obj });
                    if let Some(id) = msg.tool_call_id.as_ref().filter(|s| !s.is_empty()) {
                        fr["id"] = serde_json::json!(id);
                    }
                    pending_tool_parts.push(serde_json::json!({ "functionResponse": fr }));
                }
            }
        }
        if !pending_tool_parts.is_empty() {
            contents.push(serde_json::json!({
                "role": "user",
                "parts": pending_tool_parts,
            }));
        }

        let mut body = serde_json::json!({ "contents": contents });
        // Native system prompt — `v1beta` accepts `systemInstruction` as a
        // top-level field (a Content with text parts).
        if let Some(sys) = system_text {
            body["systemInstruction"] = serde_json::json!({ "parts": [{ "text": sys }] });
        }
        let mut gen_config = serde_json::Map::new();
        if let Some(max) = request.max_tokens {
            gen_config.insert("maxOutputTokens".to_string(), serde_json::json!(max));
        }
        if let Some(temp) = request.temperature {
            gen_config.insert("temperature".to_string(), serde_json::json!(temp));
        }
        if let Some(top_p) = request.top_p {
            gen_config.insert("topP".to_string(), serde_json::json!(top_p));
        }
        if !request.stop_sequences.is_empty() {
            gen_config.insert(
                "stopSequences".to_string(),
                serde_json::json!(&request.stop_sequences),
            );
        }
        if request.response_format == Some(axocoatl_core::ResponseFormat::Json) {
            gen_config.insert(
                "responseMimeType".to_string(),
                serde_json::json!("application/json"),
            );
        }
        if !gen_config.is_empty() {
            body["generationConfig"] = serde_json::Value::Object(gen_config);
        }
        // Without functionDeclarations the model never sees the tools and can't
        // emit a functionCall.
        if !request.tools.is_empty() {
            body["tools"] = gemini_tools(&request.tools);
        }
        Ok(body)
    }
}

#[async_trait::async_trait]
impl LlmProvider for GeminiProvider {
    fn provider_id(&self) -> &str {
        "gemini"
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

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        validate_provider_request(&request, self.provider_id())?;
        let body = self.build_request_body(&request)?;
        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);

        let response = self
            .client
            .post(self.endpoint_for(model_for_call))
            .header("x-goog-api-key", &self.api_key)
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
                provider: "gemini".to_string(),
                retry_after_secs,
            });
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ProviderError::AuthError {
                provider: "gemini".to_string(),
            });
        }
        if status.as_u16() == 404 {
            return Err(ProviderError::ModelNotFound {
                provider: "gemini".to_string(),
                model: model_for_call.to_string(),
            });
        }
        if !status.is_success() {
            let text = read_error_text(response, &[&self.api_key]).await;
            return Err(ProviderError::ApiError {
                provider: "gemini".to_string(),
                status: status.as_u16(),
                message: text,
            });
        }

        let resp: serde_json::Value = read_json(response, "gemini").await?;

        let candidates = resp["candidates"]
            .as_array()
            .ok_or_else(|| ProviderError::ApiError {
                provider: "gemini".to_string(),
                status: 200,
                message: "Gemini response omitted candidates".to_string(),
            })?;
        if candidates.len() != 1
            || candidates[0]
                .get("index")
                .is_some_and(|index| index.as_u64() != Some(0))
        {
            return Err(ProviderError::ApiError {
                provider: "gemini".to_string(),
                status: 200,
                message: "Gemini response must contain exactly one candidate at index zero"
                    .to_string(),
            });
        }
        let candidate = &candidates[0];

        // Walk every part: text parts concatenate into content, functionCall
        // parts become tool calls.
        let mut content = String::new();
        let parts = candidate["content"]["parts"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for part in &parts {
            if part["thought"].as_bool() != Some(true) {
                if let Some(text) = part["text"].as_str() {
                    content.push_str(text);
                }
            }
        }
        let tool_calls = gemini_tool_calls_from_parts(&parts, &request.tools)?;

        let native_finish =
            candidate["finishReason"]
                .as_str()
                .ok_or_else(|| ProviderError::ApiError {
                    provider: "gemini".to_string(),
                    status: 200,
                    message: "Gemini response omitted its finish reason".to_string(),
                })?;
        let mut finish_reason = parse_gemini_finish_reason(native_finish).map_err(|message| {
            ProviderError::ApiError {
                provider: "gemini".to_string(),
                status: 200,
                message,
            }
        })?;
        finish_reason = normalize_gemini_finish_reason(finish_reason, tool_calls.len());

        let normalized = ChatResponse {
            content,
            tool_calls,
            finish_reason,
            usage: TokenUsageStats {
                input_tokens: resp["usageMetadata"]["promptTokenCount"]
                    .as_u64()
                    .unwrap_or(0) as usize,
                output_tokens: resp["usageMetadata"]["candidatesTokenCount"]
                    .as_u64()
                    .unwrap_or(0) as usize,
                reasoning_tokens: None,
            },
            model: resp["modelVersion"]
                .as_str()
                .unwrap_or(model_for_call)
                .to_string(),
            provider: "gemini".to_string(),
        };
        validate_chat_response("gemini", &normalized)?;
        Ok(normalized)
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        validate_provider_request(&request, self.provider_id())?;
        let body = self.build_request_body(&request)?;
        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);

        let request_builder = self
            .client
            .post(self.stream_endpoint_for(model_for_call))
            .header("x-goog-api-key", &self.api_key)
            .json(&body);

        let response = tokio::time::timeout(RESPONSE_TIMEOUT, request_builder.send())
            .await
            .map_err(|_| ProviderError::Network("Gemini response headers timed out".to_string()))?
            .map_err(|error| network_error(&error, &[&self.api_key]))?;

        let status = response.status();
        if status == 429 {
            let retry_after_secs = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok());
            return Err(ProviderError::RateLimited {
                provider: "gemini".to_string(),
                retry_after_secs,
            });
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ProviderError::AuthError {
                provider: "gemini".to_string(),
            });
        }
        if !status.is_success() {
            let text = read_error_text(response, &[&self.api_key]).await;
            return Err(ProviderError::ApiError {
                provider: "gemini".to_string(),
                status: status.as_u16(),
                message: text,
            });
        }

        let mut bytes = response.bytes_stream();
        let api_key = self.api_key.clone();
        let offered_tools = request.tools.clone();

        let stream = async_stream::try_stream! {
            let mut decoder = SseDecoder::provider_default();
            let mut pending_finish = None;
            let mut saw_terminal_sentinel = false;
            let mut next_function_call_index = 0usize;
            let mut first_tool_call: Option<(usize, String)> = None;
            let mut native_parts = Vec::new();
            let total_deadline = tokio::time::Instant::now() + STREAM_TOTAL_TIMEOUT;

            loop {
                let next = next_stream_item(
                    &mut bytes,
                    total_deadline,
                    STREAM_IDLE_TIMEOUT,
                    "Gemini",
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
                    let (terminal, parsed, parts) =
                        parse_gemini_sse_data(
                            &event.data,
                            &mut next_function_call_index,
                            &offered_tools,
                        )?;
                    if terminal {
                        saw_terminal_sentinel = true;
                        break;
                    }
                    native_parts.extend(parts);
                    for event in parsed {
                        match event {
                            StreamEvent::Done { finish_reason } => pending_finish = Some(finish_reason),
                            StreamEvent::ToolCallDelta { index: Some(index), ref id, .. } => {
                                if first_tool_call.is_none() {
                                    first_tool_call = Some((index, id.clone()));
                                }
                                yield event;
                            }
                            other => yield other,
                        }
                    }
                }
                if reached_eof || saw_terminal_sentinel {
                    break;
                }
            }

            let mut finish_reason = pending_finish.ok_or_else(|| {
                ProviderError::Stream("Gemini stream ended without a finish reason".to_string())
            })?;
            finish_reason = normalize_gemini_finish_reason(
                finish_reason,
                next_function_call_index,
            );
            if let Some((index, id)) = first_tool_call {
                yield StreamEvent::ToolCallMetadata {
                    index: Some(index),
                    id,
                    metadata: gemini_replay_metadata(&native_parts)?,
                };
            }
            validate_stream_terminal("Gemini", &finish_reason, next_function_call_index)?;
            yield StreamEvent::Done { finish_reason };
        };

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
            concurrency: Default::default(),
        }
    }

    #[test]
    fn provider_identity() {
        let p = GeminiProvider::new("key", "gemini-2.5-flash");
        assert_eq!(p.provider_id(), "gemini");
        assert_eq!(p.model_id(), "gemini-2.5-flash");
    }

    #[test]
    fn capabilities() {
        let p = GeminiProvider::new("key", "gemini-2.5-flash");
        let caps = p.capabilities();
        assert!(caps.streaming);
        assert!(caps.vision);
        assert!(caps.tool_calling);
        assert_eq!(caps.max_context_tokens, 1_048_576);
        assert_eq!(caps.max_output_tokens, 65_536);
    }

    #[test]
    fn model_override_drives_exact_gemini_capabilities() {
        let provider = GeminiProvider::new("key", "gemini-2.5-flash");
        assert!(provider.capabilities().reasoning);
        assert!(provider.model_constraints_known(&ChatRequest::simple("think")));
        let mut request = ChatRequest::simple("think");
        request.model_override = Some("gemini-3-flash-preview".to_string());
        assert!(provider.capabilities_for(&request).reasoning);
        assert!(!provider.model_constraints_known(&request));
        assert_eq!(provider.capabilities_for(&request).max_context_tokens, 0);
    }

    #[test]
    fn build_request_body_includes_function_declarations() {
        let p = GeminiProvider::new("key", "gemini-2.5-flash");
        let mut request = ChatRequest::simple("weather in NYC?");
        request.tools = vec![ToolDefinition {
            name: "get_weather".to_string(),
            description: "Get current weather".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "location": { "type": "string" } },
                "required": ["location"]
            }),
            concurrency: Default::default(),
        }];
        let body = p.build_request_body(&request).unwrap();
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["name"],
            "get_weather"
        );
    }

    #[test]
    fn assistant_and_tool_turns_become_function_call_and_response() {
        use axocoatl_core::ChatMessage;
        let p = GeminiProvider::new("key", "gemini-2.5-flash");
        let mut request = ChatRequest::simple("weather in NYC?");
        request
            .messages
            .push(ChatMessage::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "fc_1".to_string(),
                    name: "get_weather".to_string(),
                    arguments: serde_json::json!({ "location": "NYC" }),
                    provider_metadata: Default::default(),
                }],
            ));
        request.messages.push(ChatMessage::tool_result(
            "{\"temp\":72}",
            "get_weather",
            "fc_1",
        ));
        let body = p.build_request_body(&request).unwrap();
        let contents = body["contents"].as_array().unwrap();

        // model turn carries the functionCall...
        let model_turn = contents.iter().find(|c| c["role"] == "model").unwrap();
        assert_eq!(
            model_turn["parts"][0]["functionCall"]["name"],
            "get_weather"
        );
        assert_eq!(
            model_turn["parts"][0]["functionCall"]["args"]["location"],
            "NYC"
        );

        // ...and the result is a functionResponse in a user turn, by name.
        let fr_turn = contents
            .iter()
            .find(|c| c["parts"][0].get("functionResponse").is_some())
            .unwrap();
        assert_eq!(fr_turn["role"], "user");
        assert_eq!(
            fr_turn["parts"][0]["functionResponse"]["name"],
            "get_weather"
        );
        assert_eq!(
            fr_turn["parts"][0]["functionResponse"]["response"]["temp"],
            72
        );
    }

    #[test]
    fn parse_chunk_function_call() {
        let chunk = serde_json::json!({
            "candidates": [{
                "content": { "parts": [{
                    "functionCall": { "name": "get_weather", "args": { "location": "NYC" } }
                }] }
            }]
        });
        let events = parse_gemini_chunk(&chunk, &[tool("get_weather")]).unwrap();
        let found = events.iter().any(|e| {
            matches!(
                e,
                StreamEvent::ToolCallDelta { name: Some(n), args_delta, .. }
                    if n == "get_weather" && args_delta.contains("NYC")
            )
        });
        assert!(found, "expected a ToolCallDelta from the functionCall part");
    }

    #[test]
    fn function_call_indexes_remain_unique_across_sse_frames() {
        let first = serde_json::json!({
            "candidates": [{ "content": { "parts": [{
                "functionCall": { "name": "lookup", "args": { "q": 1 } }
            }] } }]
        });
        let second = serde_json::json!({
            "candidates": [{ "content": { "parts": [{
                "functionCall": { "name": "lookup", "args": { "q": 2 } }
            }] } }]
        });
        let mut next_index = 0;
        let mut events =
            parse_gemini_chunk_with_index(&first, &mut next_index, &[tool("lookup")]).unwrap();
        events.extend(
            parse_gemini_chunk_with_index(&second, &mut next_index, &[tool("lookup")]).unwrap(),
        );
        let indexes = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ToolCallDelta { index, .. } => *index,
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(indexes, vec![0, 1]);
    }

    #[test]
    fn nonstream_and_stream_normalization_reject_invalid_gemini_calls() {
        for function_call in [
            serde_json::json!({"name":"","args":{}}),
            serde_json::json!({"name":"other","args":{}}),
            serde_json::json!({"name":"lookup"}),
            serde_json::json!({"name":"lookup","args":null}),
            serde_json::json!({"name":"lookup","args":7}),
        ] {
            let part = serde_json::json!({"functionCall": function_call});
            assert!(gemini_tool_call_from_part(&part, &[tool("lookup")]).is_err());
            let chunk = serde_json::json!({
                "candidates": [{"content": {"parts": [part]}}]
            });
            assert!(parse_gemini_chunk(&chunk, &[tool("lookup")]).is_err());
        }
    }

    #[test]
    fn gemini_25_single_call_signature_round_trips_exactly() {
        use axocoatl_core::ChatMessage;

        let signature = "opaque+/=\u{00e9} exact bytes";
        let chunk = serde_json::json!({
            "candidates": [{
                "content": { "parts": [{
                    "functionCall": {
                        "id": "fc_25",
                        "name": "lookup",
                        "args": { "query": "axolotl" }
                    },
                    "thoughtSignature": signature
                }] }
            }]
        });
        let events = parse_gemini_chunk(&chunk, &[tool("lookup")]).unwrap();
        let metadata = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::ToolCallMetadata { metadata, .. } => Some(metadata.clone()),
                _ => None,
            })
            .expect("Gemini stream must preserve tool-call metadata");
        assert_eq!(metadata.get(GEMINI_THOUGHT_SIGNATURE).unwrap(), signature);

        let provider = GeminiProvider::new("key", "gemini-2.5-flash");
        let mut request = ChatRequest::simple("look it up");
        request
            .messages
            .push(ChatMessage::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "fc_25".to_string(),
                    name: "lookup".to_string(),
                    arguments: serde_json::json!({ "query": "axolotl" }),
                    provider_metadata: metadata,
                }],
            ));
        request
            .messages
            .push(ChatMessage::tool_result("{}", "lookup", "fc_25"));
        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(
            body["contents"][1]["parts"][0]["thoughtSignature"],
            signature
        );
    }

    #[test]
    fn gemini_3_parallel_calls_preserve_signature_and_group_responses() {
        use axocoatl_core::ChatMessage;

        let signature = "parallel-signature";
        let chunk = serde_json::json!({
            "candidates": [{
                "content": { "parts": [
                    {
                        "functionCall": {
                            "name": "weather",
                            "args": { "city": "Paris" }
                        },
                        "thoughtSignature": signature
                    },
                    {
                        "functionCall": {
                            "name": "weather",
                            "args": { "city": "London" }
                        }
                    }
                ] }
            }]
        });
        let events = parse_gemini_chunk(&chunk, &[tool("weather")]).unwrap();
        let mut calls = Vec::new();
        let mut metadata = std::collections::BTreeMap::new();
        for event in events {
            match event {
                StreamEvent::ToolCallDelta {
                    index,
                    id,
                    name: Some(name),
                    args_delta,
                } => calls.push((index.unwrap(), id, name, args_delta)),
                StreamEvent::ToolCallMetadata {
                    index: Some(index),
                    metadata: value,
                    ..
                } => {
                    metadata.insert(index, value);
                }
                _ => {}
            }
        }
        assert_eq!(calls.len(), 2);
        assert_eq!(
            metadata[&0].get(GEMINI_THOUGHT_SIGNATURE).unwrap(),
            signature
        );
        assert!(!metadata[&1].contains_key(GEMINI_THOUGHT_SIGNATURE));

        let provider = GeminiProvider::new("key", "gemini-3-flash-preview");
        let mut request = ChatRequest::simple("Paris and London weather?");
        request
            .messages
            .push(ChatMessage::assistant_with_tool_calls(
                "",
                calls
                    .into_iter()
                    .map(|(index, id, name, args)| ToolCall {
                        id,
                        name,
                        arguments: serde_json::from_str(&args).unwrap(),
                        provider_metadata: metadata.remove(&index).unwrap(),
                    })
                    .collect(),
            ));
        request
            .messages
            .push(ChatMessage::tool_result("{\"temp\":15}", "weather", ""));
        request
            .messages
            .push(ChatMessage::tool_result("{\"temp\":12}", "weather", ""));

        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(body["contents"][1]["parts"].as_array().unwrap().len(), 2);
        assert_eq!(
            body["contents"][1]["parts"][0]["thoughtSignature"],
            signature
        );
        assert!(body["contents"][1]["parts"][1]
            .get("thoughtSignature")
            .is_none());
        let responses = &body["contents"][2];
        assert_eq!(responses["role"], "user");
        assert_eq!(responses["parts"].as_array().unwrap().len(), 2);
        assert!(responses["parts"][0].get("functionResponse").is_some());
        assert!(responses["parts"][1].get("functionResponse").is_some());
    }

    #[test]
    fn gemini_text_signatures_and_parallel_parts_replay_exactly() {
        use axocoatl_core::ChatMessage;

        let native = vec![
            serde_json::json!({
                "text": "Checking both",
                "thoughtSignature": "opaque-text-signature"
            }),
            serde_json::json!({
                "functionCall": {"name":"weather","args":{"city":"Paris"}},
                "thoughtSignature": "opaque-call-signature"
            }),
            serde_json::json!({
                "functionCall": {"name":"weather","args":{"city":"London"}}
            }),
        ];
        let calls = gemini_tool_calls_from_parts(&native, &[tool("weather")]).unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].provider_metadata.contains_key(GEMINI_REPLAY_PARTS));

        let provider = GeminiProvider::new("key", "gemini-3-flash-preview");
        let mut request = ChatRequest::simple("compare weather");
        request
            .messages
            .push(ChatMessage::assistant_with_tool_calls(
                "Checking both",
                calls,
            ));
        request
            .messages
            .push(ChatMessage::tool_result("{}", "weather", ""));
        request
            .messages
            .push(ChatMessage::tool_result("{}", "weather", ""));
        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(body["contents"][1]["parts"], serde_json::json!(native));
    }

    #[test]
    fn conflicting_gemini_replay_parts_fail_closed() {
        let native = vec![serde_json::json!({
            "functionCall": {"id":"fc","name":"lookup","args":{"q":1}},
            "thoughtSignature": "signature"
        })];
        let mut calls = gemini_tool_calls_from_parts(&native, &[tool("lookup")]).unwrap();
        calls[0].arguments = serde_json::json!({"q": 2});
        assert!(replay_parts_for_message("", &calls).is_err());
    }

    #[test]
    fn endpoint_format() {
        let p = GeminiProvider::new("test-key", "gemini-2.5-flash");
        let url = p.endpoint_for("gemini-2.5-flash");
        assert!(url.contains("gemini-2.5-flash:generateContent"));
        // The key must NOT be in the URL — it travels in the x-goog-api-key
        // header so it can't leak via error strings or logs.
        assert!(!url.contains("test-key"));
        assert!(!url.contains("key="));

        let surl = p.stream_endpoint_for("gemini-2.5-flash");
        assert!(surl.contains("streamGenerateContent?alt=sse"));
        assert!(!surl.contains("test-key"));
    }

    #[test]
    fn parse_chunk_text_delta() {
        let chunk = serde_json::json!({
            "candidates": [{ "content": { "parts": [{ "text": "Hello" }] } }]
        });
        let events = parse_gemini_chunk(&chunk, &[]).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::TextDelta { delta } => assert_eq!(delta, "Hello"),
            _ => panic!("expected TextDelta"),
        }
    }

    #[test]
    fn thought_text_is_reasoning_but_remains_in_native_replay() {
        let chunk = serde_json::json!({
            "candidates": [{ "content": { "parts": [
                {"text":"private", "thought":true, "thoughtSignature":"thought-sig"},
                {"functionCall":{"name":"lookup","args":{}}}
            ] } }]
        });
        let events = parse_gemini_chunk(&chunk, &[tool("lookup")]).unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::ReasoningDelta { delta } if delta == "private"
        )));
        assert!(!events
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta { .. })));

        let native = chunk["candidates"][0]["content"]["parts"]
            .as_array()
            .unwrap();
        let calls = gemini_tool_calls_from_parts(native, &[tool("lookup")]).unwrap();
        assert!(replay_parts_for_message("", &calls).is_ok());
    }

    #[test]
    fn parse_chunk_finish_and_usage() {
        let chunk = serde_json::json!({
            "candidates": [{
                "content": { "parts": [{ "text": "!" }] },
                "finishReason": "STOP"
            }],
            "usageMetadata": { "promptTokenCount": 12, "candidatesTokenCount": 7 }
        });
        let events = parse_gemini_chunk(&chunk, &[]).unwrap();
        assert!(matches!(events[0], StreamEvent::TextDelta { .. }));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::Usage(u) if u.output_tokens == 7)));
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::Done {
                finish_reason: FinishReason::Stop
            }
        )));
    }

    #[test]
    fn gemini_stop_with_function_calls_normalizes_to_tool_use() {
        assert_eq!(
            normalize_gemini_finish_reason(FinishReason::Stop, 2),
            FinishReason::ToolUse
        );
        assert_eq!(
            normalize_gemini_finish_reason(FinishReason::Stop, 0),
            FinishReason::Stop
        );
        assert_eq!(
            normalize_gemini_finish_reason(FinishReason::MaxTokens, 1),
            FinishReason::MaxTokens
        );
    }

    #[test]
    fn unsuccessful_or_unknown_finish_never_makes_a_function_call_actionable() {
        for reason in [
            "MALFORMED_FUNCTION_CALL",
            "UNEXPECTED_TOOL_CALL",
            "OTHER",
            "FUTURE_UNKNOWN_REASON",
        ] {
            let chunk = serde_json::json!({
                "candidates": [{
                    "content": {"parts": [{
                        "functionCall": {"name": "lookup", "args": {}}
                    }]},
                    "finishReason": reason
                }]
            });
            assert!(matches!(
                parse_gemini_chunk(&chunk, &[tool("lookup")]),
                Err(ProviderError::Stream(message)) if message.contains(reason)
            ));
        }

        let filtered = serde_json::json!({
            "candidates": [{"content": {"parts": []}, "finishReason": "SAFETY"}]
        });
        assert!(parse_gemini_chunk(&filtered, &[])
            .unwrap()
            .iter()
            .any(|event| matches!(
                event,
                StreamEvent::Done {
                    finish_reason: FinishReason::ContentFilter
                }
            )));
    }

    #[test]
    fn multiple_candidates_fail_closed_instead_of_merging_or_selecting() {
        let chunk = serde_json::json!({
            "candidates": [
                {"content":{"parts":[{"text":"A"}]}},
                {"content":{"parts":[{"text":"B"}]}}
            ]
        });
        assert!(matches!(
            parse_gemini_chunk(&chunk, &[]),
            Err(ProviderError::Stream(message)) if message.contains("exactly one candidate")
        ));

        let first_frame = serde_json::json!({
            "candidates": [{"index": 0, "content":{"parts":[{"text":"A"}]}}]
        });
        assert!(parse_gemini_chunk(&first_frame, &[]).is_ok());
        let other_candidate_later = serde_json::json!({
            "candidates": [{"index": 1, "content":{"parts":[{"text":"B"}]}}]
        });
        assert!(matches!(
            parse_gemini_chunk(&other_candidate_later, &[]),
            Err(ProviderError::Stream(message)) if message.contains("index zero")
        ));
    }

    #[test]
    fn system_prompt_uses_native_system_instruction() {
        let p = GeminiProvider::new("key", "gemini-2.5-flash");
        let req = ChatRequest::with_system("Be brief.", "Hi");
        let body = p.build_request_body(&req).unwrap();
        // v1beta accepts systemInstruction natively — the user turn stays clean.
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Be brief.");
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "Hi");
    }

    #[test]
    fn endpoint_uses_v1beta() {
        // Function calling + systemInstruction require the v1beta endpoint.
        let p = GeminiProvider::new("key", "gemini-2.5-flash");
        assert!(p.endpoint_for("gemini-2.5-flash").contains("/v1beta/"));
        assert!(p
            .stream_endpoint_for("gemini-2.5-flash")
            .contains("/v1beta/"));
    }

    #[test]
    fn request_body_forwards_max_tokens_and_stop_sequences() {
        let provider = GeminiProvider::new("key", "gemini-2.5-flash");
        let mut request = ChatRequest::simple("hello");
        request.max_tokens = Some(321);
        request.stop_sequences = vec!["END".to_string(), "STOP".to_string()];
        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 321);
        assert_eq!(
            body["generationConfig"]["stopSequences"],
            serde_json::json!(["END", "STOP"])
        );
    }
}
