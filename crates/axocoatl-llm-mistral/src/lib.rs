//! Mistral AI provider — uses the OpenAI-compatible chat completions API.

use std::pin::Pin;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use tokio_stream::Stream;

use axocoatl_core::{ChatMessage, MessageContent, MessageRole, TokenUsageStats};
use axocoatl_llm::{
    provider_tool_metadata,
    transport::{
        bounded_redacted, http_client, network_error, next_stream_item, read_error_text, read_json,
        SseDecoder, RESPONSE_TIMEOUT, STREAM_IDLE_TIMEOUT, STREAM_TOTAL_TIMEOUT,
    },
    validate_chat_response, validate_provider_request, validate_required_stream_tool_call_ids,
    validate_response_tool_call, ChatRequest, ChatResponse, FinishReason, LlmProvider,
    ProviderCapabilities, ProviderError, StreamEvent, ToolCall, ToolDefinition,
};

const MISTRAL_API_URL: &str = "https://api.mistral.ai/v1/chat/completions";

fn mistral_tool_call_id_is_compatible(id: &str) -> bool {
    id.len() == 9 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn validate_mistral_history_tool_call_id(id: &str) -> Result<(), ProviderError> {
    if mistral_tool_call_id_is_compatible(id) {
        return Ok(());
    }
    Err(ProviderError::InvalidRequest {
        provider: "mistral".to_string(),
        message: "Mistral tool-call history requires a 9-character ASCII alphanumeric id"
            .to_string(),
    })
}

fn validate_mistral_response_tool_call_id(id: &str) -> Result<(), ProviderError> {
    if mistral_tool_call_id_is_compatible(id) {
        return Ok(());
    }
    Err(ProviderError::ApiError {
        provider: "mistral".to_string(),
        status: 200,
        message: "Mistral returned a tool call without a compatible 9-character id".to_string(),
    })
}

/// Convert chat messages into Mistral's OpenAI-compatible `messages` array.
/// Carries assistant `tool_calls` and each tool result's `name` + `tool_call_id`
/// so a multi-turn tool round-trip replays as a well-formed conversation.
/// (Mistral wants both `name` and `tool_call_id` on tool-result messages.)
fn mistral_messages(messages: &[ChatMessage]) -> Result<Vec<serde_json::Value>, ProviderError> {
    for message in messages {
        for call in &message.tool_calls {
            validate_mistral_history_tool_call_id(&call.id)?;
        }
        if matches!(message.role, MessageRole::Tool) {
            let id = message
                .tool_call_id
                .as_deref()
                .or(message.name.as_deref())
                .unwrap_or_default();
            validate_mistral_history_tool_call_id(id)?;
        }
    }
    Ok(messages
        .iter()
        .map(|m| {
            let role = match m.role {
                MessageRole::System => "system",
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::Tool => "tool",
            };

            // User multimodal: emit Mistral's OpenAI-compatible content array
            // (works with pixtral; non-vision models reject images, as expected).
            if matches!(m.role, MessageRole::User) {
                if let MessageContent::Parts(parts) = &m.content {
                    let arr: Vec<serde_json::Value> = parts
                        .iter()
                        .map(|p| match p {
                            axocoatl_core::ContentPart::Text(s) => {
                                serde_json::json!({"type": "text", "text": s})
                            }
                            axocoatl_core::ContentPart::Image { url, .. } => {
                                serde_json::json!({"type": "image_url", "image_url": url})
                            }
                        })
                        .collect();
                    return serde_json::json!({"role": role, "content": arr});
                }
            }

            let content = match &m.content {
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
            let mut msg = serde_json::json!({"role": role, "content": content});

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
                if let Some(name) = &m.name {
                    msg["name"] = serde_json::json!(name);
                }
            }
            msg
        })
        .collect())
}

/// Convert tool definitions into Mistral's OpenAI-compatible `tools` array.
fn mistral_tools(tools: &[ToolDefinition]) -> serde_json::Value {
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

/// Parse the non-streaming `message.tool_calls` array into [`ToolCall`]s.
fn parse_tool_calls(
    message: &serde_json::Value,
    tools: &[ToolDefinition],
) -> Result<Vec<ToolCall>, ProviderError> {
    let Some(calls) = message["tool_calls"].as_array() else {
        return Ok(Vec::new());
    };
    let calls: Vec<ToolCall> = calls
        .iter()
        .map(|call| {
            let id = call["id"].as_str().unwrap_or("").to_string();
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            let args =
                call["function"]["arguments"]
                    .as_str()
                    .ok_or_else(|| ProviderError::ApiError {
                        provider: "mistral".to_string(),
                        status: 200,
                        message: "provider returned malformed tool-call arguments".to_string(),
                    })?;
            let arguments = serde_json::from_str(args).map_err(|_| ProviderError::ApiError {
                provider: "mistral".to_string(),
                status: 200,
                message: "provider returned malformed tool-call arguments".to_string(),
            })?;
            validate_mistral_response_tool_call_id(&id)?;
            validate_response_tool_call("mistral", &name, &arguments, tools)?;
            Ok(ToolCall {
                id,
                name,
                arguments,
                provider_metadata: provider_tool_metadata("mistral"),
            })
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;
    let mut ids = std::collections::HashSet::with_capacity(calls.len());
    if calls.iter().any(|call| !ids.insert(call.id.as_str())) {
        return Err(ProviderError::ApiError {
            provider: "mistral".to_string(),
            status: 200,
            message: "provider returned duplicate tool-call ids".to_string(),
        });
    }
    Ok(calls)
}

/// Parse one Mistral streaming chunk (OpenAI-compatible) into stream events.
/// Pure + synchronous so it is unit-tested without the network.
fn parse_mistral_chunk(data: &serde_json::Value) -> Result<Vec<StreamEvent>, ProviderError> {
    let mut events = Vec::new();
    let choices = data["choices"]
        .as_array()
        .ok_or_else(|| ProviderError::Stream("Mistral stream frame omitted choices".to_string()))?;
    if choices.len() > 1
        || choices
            .first()
            .is_some_and(|choice| choice["index"].as_u64() != Some(0))
    {
        return Err(ProviderError::Stream(
            "Mistral stream returned multiple alternatives or a nonzero choice index".to_string(),
        ));
    }
    if choices.is_empty() && data.get("usage").is_none_or(serde_json::Value::is_null) {
        return Err(ProviderError::Stream(
            "Mistral stream returned an empty non-usage frame".to_string(),
        ));
    }
    let empty_choice = serde_json::Value::Null;
    let choice = choices.first().unwrap_or(&empty_choice);

    if let Some(text) = choice["delta"]["content"].as_str() {
        if !text.is_empty() {
            events.push(StreamEvent::TextDelta {
                delta: text.to_string(),
            });
        }
    }

    // Tool-call deltas. Mistral keys parallel calls by `index` and sends the id
    // only on the first fragment, matching the OpenAI streaming contract.
    if let Some(tool_calls) = choice["delta"]["tool_calls"].as_array() {
        for tc in tool_calls {
            let index = tc["index"].as_u64().map(|i| i as usize);
            let id = tc["id"].as_str().unwrap_or("").to_string();
            let name = tc["function"]["name"].as_str().map(String::from);
            let args_delta = tc["function"]["arguments"]
                .as_str()
                .unwrap_or("")
                .to_string();
            events.push(StreamEvent::ToolCallDelta {
                index,
                id: id.clone(),
                name: name.clone(),
                args_delta,
            });
            if !id.is_empty() || name.is_some() {
                events.push(StreamEvent::ToolCallMetadata {
                    index,
                    id,
                    metadata: provider_tool_metadata("mistral"),
                });
            }
        }
    }

    // With `stream_options.include_usage`, the final chunk carries usage and an
    // empty `choices` array.
    if let Some(usage) = data.get("usage").filter(|u| !u.is_null()) {
        events.push(StreamEvent::Usage(TokenUsageStats {
            input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0) as usize,
            output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0) as usize,
            reasoning_tokens: None,
        }));
    }

    if let Some(reason) = choice["finish_reason"].as_str() {
        let finish = match reason {
            "stop" => FinishReason::Stop,
            "length" => FinishReason::MaxTokens,
            "tool_calls" => FinishReason::ToolUse,
            other => {
                return Err(ProviderError::Stream(format!(
                    "Mistral returned unsupported finish reason {other}"
                )));
            }
        };
        events.push(StreamEvent::Done {
            finish_reason: finish,
        });
    }

    Ok(events)
}

fn parse_mistral_sse_data(data: &str) -> Result<(bool, Vec<StreamEvent>), ProviderError> {
    if data.trim() == "[DONE]" {
        return Ok((true, Vec::new()));
    }
    let data: serde_json::Value = serde_json::from_str(data)
        .map_err(|error| ProviderError::Stream(format!("invalid Mistral SSE JSON: {error}")))?;
    Ok((false, parse_mistral_chunk(&data)?))
}

fn track_mistral_tool_call_id(
    tool_call_ids: &mut std::collections::BTreeMap<usize, String>,
    event: &StreamEvent,
) -> Result<(), ProviderError> {
    let StreamEvent::ToolCallDelta { index, id, .. } = event else {
        return Ok(());
    };
    let index = index.ok_or_else(|| {
        ProviderError::Stream("Mistral streamed a tool call without an index".to_string())
    })?;
    let known_id = tool_call_ids.entry(index).or_default();
    if !id.is_empty() {
        if !mistral_tool_call_id_is_compatible(id) {
            return Err(ProviderError::Stream(
                "Mistral streamed a tool call without a compatible 9-character id".to_string(),
            ));
        }
        if !known_id.is_empty() && known_id != id {
            return Err(ProviderError::Stream(format!(
                "Mistral changed a tool-call id for index {index}"
            )));
        }
        *known_id = id.clone();
    }
    Ok(())
}

/// Mistral AI provider using their OpenAI-compatible API.
pub struct MistralProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl MistralProvider {
    fn capabilities_for_model(model: &str) -> ProviderCapabilities {
        let mut capabilities = ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            structured_output: true,
            vision: true,
            reasoning: false,
            embeddings: false,
            max_context_tokens: 0,
            max_output_tokens: 0,
        };
        if model == "mistral-large-2512" {
            capabilities.max_context_tokens = 256_000;
        }
        capabilities
    }

    fn model_constraints_known_for(model: &str) -> bool {
        model == "mistral-large-2512"
    }

    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: http_client(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    /// Build the OpenAI-compatible request body shared by `chat` and `chat_stream`.
    fn build_request_body(
        &self,
        request: &ChatRequest,
    ) -> Result<serde_json::Value, ProviderError> {
        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);
        let mut body = serde_json::json!({
            "model": model_for_call,
            "messages": mistral_messages(&request.messages)?,
        });
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
            body["stop"] = serde_json::json!(request.stop_sequences);
        }
        if request.response_format == Some(axocoatl_core::ResponseFormat::Json) {
            body["response_format"] = serde_json::json!({ "type": "json_object" });
        }
        // Without attaching tools the model never receives them and can't emit
        // a tool call.
        if !request.tools.is_empty() {
            body["tools"] = mistral_tools(&request.tools);
        }
        Ok(body)
    }
}

#[async_trait::async_trait]
impl LlmProvider for MistralProvider {
    fn provider_id(&self) -> &str {
        "mistral"
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
            .post(MISTRAL_API_URL)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .timeout(RESPONSE_TIMEOUT)
            .send()
            .await
            .map_err(|error| network_error(&error, &[&self.api_key]))?;

        let status = response.status();
        if status == 429 {
            return Err(ProviderError::RateLimited {
                provider: "mistral".to_string(),
                retry_after_secs: None,
            });
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ProviderError::AuthError {
                provider: "mistral".to_string(),
            });
        }
        if status.as_u16() == 404 {
            return Err(ProviderError::ModelNotFound {
                provider: "mistral".to_string(),
                model: model_for_call.to_string(),
            });
        }
        if !status.is_success() {
            let text = read_error_text(response, &[&self.api_key]).await;
            return Err(ProviderError::ApiError {
                provider: "mistral".to_string(),
                status: status.as_u16(),
                message: text,
            });
        }

        let resp: serde_json::Value = read_json(response, "mistral").await?;

        let choices = resp["choices"]
            .as_array()
            .ok_or_else(|| ProviderError::ApiError {
                provider: "mistral".to_string(),
                status: 200,
                message: "Mistral response omitted choices".to_string(),
            })?;
        if choices.len() != 1 || choices[0]["index"].as_u64() != Some(0) {
            return Err(ProviderError::ApiError {
                provider: "mistral".to_string(),
                status: 200,
                message: "Mistral response must contain exactly one choice at index zero"
                    .to_string(),
            });
        }
        let choice = &choices[0];
        let finish_reason = match choice["finish_reason"].as_str() {
            Some("stop") => FinishReason::Stop,
            Some("length") => FinishReason::MaxTokens,
            Some("tool_calls") => FinishReason::ToolUse,
            Some(other) => {
                return Err(ProviderError::ApiError {
                    provider: "mistral".to_string(),
                    status: 200,
                    message: format!("Mistral returned unsupported finish reason {other}"),
                });
            }
            None => {
                return Err(ProviderError::ApiError {
                    provider: "mistral".to_string(),
                    status: 200,
                    message: "Mistral response omitted its finish reason".to_string(),
                });
            }
        };

        let normalized = ChatResponse {
            content: choice["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            tool_calls: parse_tool_calls(&choice["message"], &request.tools)?,
            finish_reason,
            usage: TokenUsageStats {
                input_tokens: resp["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as usize,
                output_tokens: resp["usage"]["completion_tokens"].as_u64().unwrap_or(0) as usize,
                reasoning_tokens: None,
            },
            model: resp["model"].as_str().unwrap_or(model_for_call).to_string(),
            provider: "mistral".to_string(),
        };
        validate_chat_response("mistral", &normalized)?;
        Ok(normalized)
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        validate_provider_request(&request, self.provider_id())?;
        let mut body = self.build_request_body(&request)?;
        body["stream"] = serde_json::json!(true);
        body["stream_options"] = serde_json::json!({ "include_usage": true });

        let request_builder = self
            .client
            .post(MISTRAL_API_URL)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .json(&body);

        let response = tokio::time::timeout(RESPONSE_TIMEOUT, request_builder.send())
            .await
            .map_err(|_| ProviderError::Network("Mistral response headers timed out".to_string()))?
            .map_err(|error| network_error(&error, &[&self.api_key]))?;

        let status = response.status();
        if status == 429 {
            return Err(ProviderError::RateLimited {
                provider: "mistral".to_string(),
                retry_after_secs: None,
            });
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ProviderError::AuthError {
                provider: "mistral".to_string(),
            });
        }
        if status.as_u16() == 404 {
            let model = request.model_override.as_deref().unwrap_or(&self.model);
            return Err(ProviderError::ModelNotFound {
                provider: "mistral".to_string(),
                model: model.to_string(),
            });
        }
        if !status.is_success() {
            let text = read_error_text(response, &[&self.api_key]).await;
            return Err(ProviderError::ApiError {
                provider: "mistral".to_string(),
                status: status.as_u16(),
                message: text,
            });
        }

        let mut bytes = response.bytes_stream();
        let api_key = self.api_key.clone();

        let stream = async_stream::try_stream! {
            let mut decoder = SseDecoder::provider_default();
            let mut pending_finish = None;
            let mut saw_sentinel = false;
            let mut tool_call_ids = std::collections::BTreeMap::<usize, String>::new();
            let total_deadline = tokio::time::Instant::now() + STREAM_TOTAL_TIMEOUT;

            'response: loop {
                let next = next_stream_item(
                    &mut bytes,
                    total_deadline,
                    STREAM_IDLE_TIMEOUT,
                    "Mistral",
                )
                .await?;
                let Some(chunk) = next else {
                    for event in decoder.finish()? {
                        let (terminal, events) = parse_mistral_sse_data(&event.data)?;
                        if terminal {
                            saw_sentinel = true;
                            break 'response;
                        }
                        for event in events {
                            track_mistral_tool_call_id(&mut tool_call_ids, &event)?;
                            match event {
                                StreamEvent::Done { finish_reason } => pending_finish = Some(finish_reason),
                                other => yield other,
                            }
                        }
                    }
                    break;
                };
                let chunk = chunk.map_err(|error| {
                    ProviderError::Stream(bounded_redacted(&error.to_string(), 8 * 1024, &[&api_key]))
                })?;
                for event in decoder.push(&chunk)? {
                    let (terminal, events) = parse_mistral_sse_data(&event.data)?;
                    if terminal {
                        saw_sentinel = true;
                        break 'response;
                    }
                    for event in events {
                        track_mistral_tool_call_id(&mut tool_call_ids, &event)?;
                        match event {
                            // Usage is often delivered in a final empty-choice
                            // chunk after finish_reason. Defer Done so callers do
                            // not stop before receiving those exact counts.
                            StreamEvent::Done { finish_reason } => pending_finish = Some(finish_reason),
                            other => yield other,
                        }
                    }
                }
            }

            if !saw_sentinel {
                Err(ProviderError::Stream("Mistral stream ended without the [DONE] terminal sentinel".to_string()))?;
            }
            let finish_reason = pending_finish.ok_or_else(|| {
                ProviderError::Stream("Mistral stream terminated without a finish reason".to_string())
            })?;
            validate_required_stream_tool_call_ids(
                "Mistral",
                &finish_reason,
                tool_call_ids.values().map(String::as_str),
            )?;
            yield StreamEvent::Done { finish_reason };
        };

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_identity() {
        let p = MistralProvider::new("key", "mistral-large-latest");
        assert_eq!(p.provider_id(), "mistral");
        assert_eq!(p.model_id(), "mistral-large-latest");
    }

    #[test]
    fn capabilities() {
        let p = MistralProvider::new("key", "mistral-large-latest");
        let caps = p.capabilities();
        assert!(caps.streaming);
        assert!(caps.vision);
        assert!(caps.tool_calling);
        assert_eq!(caps.max_context_tokens, 0);
        assert!(!p.model_constraints_known(&ChatRequest::simple("test")));
    }

    #[test]
    fn model_override_drives_exact_mistral_capabilities() {
        let provider = MistralProvider::new("key", "mistral-small");
        let mut request = ChatRequest::simple("look");
        request.model_override = Some("mistral-large-2512".to_string());
        assert!(provider.capabilities_for(&request).vision);
        assert_eq!(
            provider.capabilities_for(&request).max_context_tokens,
            256_000
        );
        assert!(provider.model_constraints_known(&request));
    }

    #[test]
    fn build_request_body_includes_tools() {
        let p = MistralProvider::new("key", "mistral-large-latest");
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
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "get_weather");
    }

    #[test]
    fn messages_encode_assistant_tool_calls_and_tool_result() {
        let msgs = vec![
            ChatMessage::user("weather?"),
            ChatMessage::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "D681PevKs".to_string(),
                    name: "get_weather".to_string(),
                    arguments: serde_json::json!({ "location": "NYC" }),
                    provider_metadata: Default::default(),
                }],
            ),
            ChatMessage::tool_result("{\"temp\":72}", "get_weather", "D681PevKs"),
        ];
        let out = mistral_messages(&msgs).unwrap();

        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[1]["tool_calls"][0]["id"], "D681PevKs");
        assert_eq!(out[1]["tool_calls"][0]["function"]["name"], "get_weather");

        // Mistral wants both name and tool_call_id on the tool result.
        assert_eq!(out[2]["role"], "tool");
        assert_eq!(out[2]["tool_call_id"], "D681PevKs");
        assert_eq!(out[2]["name"], "get_weather");
    }

    #[test]
    fn parse_chunk_tool_call_delta() {
        let chunk = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "abc123xyz",
                        "function": { "name": "get_weather", "arguments": "{\"location\":\"NYC\"}" }
                    }]
                }
            }]
        });
        let events = parse_mistral_chunk(&chunk).unwrap();
        let found = events.iter().any(|e| {
            matches!(
                e,
                StreamEvent::ToolCallDelta { index: Some(0), id, name: Some(n), .. }
                    if id == "abc123xyz" && n == "get_weather"
            )
        });
        assert!(found, "expected a ToolCallDelta with index 0 and id");
    }

    #[test]
    fn streamed_tool_call_without_native_id_fails_before_done() {
        let chunk = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": { "name": "get_weather", "arguments": "{}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let events = parse_mistral_chunk(&chunk).unwrap();
        let mut ids = std::collections::BTreeMap::new();
        let mut finish = None;
        for event in events {
            track_mistral_tool_call_id(&mut ids, &event).unwrap();
            if let StreamEvent::Done { finish_reason } = event {
                finish = Some(finish_reason);
            }
        }
        let finish = finish.unwrap();
        assert!(matches!(
            validate_required_stream_tool_call_ids(
                "Mistral",
                &finish,
                ids.values().map(String::as_str),
            ),
            Err(ProviderError::Stream(message)) if message.contains("empty id")
        ));
    }

    #[test]
    fn nonstream_tool_calls_fail_closed_on_malformed_or_undeclared_arguments() {
        let tools = vec![ToolDefinition {
            name: "lookup".to_string(),
            description: String::new(),
            parameters: serde_json::json!({ "type": "object" }),
            concurrency: Default::default(),
        }];
        for (name, arguments) in [
            ("lookup", "{"),
            ("lookup", "[]"),
            ("not_declared", "{}"),
            ("", "{}"),
        ] {
            let message = serde_json::json!({
                "tool_calls": [{
                    "id": "Abc123XyZ",
                    "function": { "name": name, "arguments": arguments }
                }]
            });
            assert!(parse_tool_calls(&message, &tools).is_err());
        }
        let missing_id = serde_json::json!({
            "tool_calls": [{
                "id": "",
                "function": { "name": "lookup", "arguments": "{}" }
            }]
        });
        assert!(matches!(
            parse_tool_calls(&missing_id, &tools),
            Err(ProviderError::ApiError { message, .. }) if message.contains("compatible")
        ));
    }

    #[test]
    fn replay_rejects_non_mistral_tool_call_ids_locally() {
        let messages = vec![ChatMessage::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "call_1".to_string(),
                name: "lookup".to_string(),
                arguments: serde_json::json!({}),
                provider_metadata: Default::default(),
            }],
        )];
        assert!(matches!(
            mistral_messages(&messages),
            Err(ProviderError::InvalidRequest { message, .. })
                if message.contains("9-character")
        ));
    }

    #[test]
    fn replay_accepts_portable_synthetic_tool_call_id() {
        let call = ToolCall {
            id: "Axo000000".to_string(),
            name: "lookup".to_string(),
            arguments: serde_json::json!({}),
            provider_metadata: Default::default(),
        };
        let messages = vec![
            ChatMessage::assistant_with_tool_calls("", vec![call]),
            ChatMessage::tool_result("{}", "lookup", "Axo000000"),
        ];

        let encoded = mistral_messages(&messages).unwrap();

        assert_eq!(encoded[0]["tool_calls"][0]["id"], "Axo000000");
        assert_eq!(encoded[1]["tool_call_id"], "Axo000000");
    }

    #[test]
    fn parse_chunk_text_delta() {
        let chunk = serde_json::json!({
            "choices": [{ "index": 0, "delta": { "content": "Hello" } }]
        });
        let events = parse_mistral_chunk(&chunk).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::TextDelta { delta } => assert_eq!(delta, "Hello"),
            _ => panic!("expected TextDelta"),
        }
    }

    #[test]
    fn parse_chunk_finish() {
        let chunk = serde_json::json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
        });
        let events = parse_mistral_chunk(&chunk).unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::Done {
                finish_reason: FinishReason::Stop
            }
        )));
    }

    #[test]
    fn stream_rejects_multiple_choices_and_unknown_finish_reasons() {
        let alternatives = serde_json::json!({
            "choices": [
                {"index": 0, "delta": {"content": "A"}},
                {"index": 1, "delta": {"content": "B"}}
            ]
        });
        assert!(matches!(
            parse_mistral_chunk(&alternatives),
            Err(ProviderError::Stream(message)) if message.contains("multiple alternatives")
        ));

        let first_alternative = serde_json::json!({
            "choices": [{"index": 0, "delta": {"content": "safe"}}]
        });
        assert!(parse_mistral_chunk(&first_alternative).is_ok());
        let later_alternative = serde_json::json!({
            "choices": [{"index": 1, "delta": {"content": "must not merge"}}]
        });
        assert!(matches!(
            parse_mistral_chunk(&later_alternative),
            Err(ProviderError::Stream(message)) if message.contains("nonzero choice index")
        ));

        let unknown = serde_json::json!({
            "choices": [{"index": 0, "delta": {}, "finish_reason": "future_reason"}]
        });
        assert!(matches!(
            parse_mistral_chunk(&unknown),
            Err(ProviderError::Stream(message)) if message.contains("future_reason")
        ));
    }

    #[test]
    fn parse_chunk_usage_final() {
        // Final chunk (include_usage): empty choices + usage.
        let chunk = serde_json::json!({
            "choices": [],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
        });
        let events = parse_mistral_chunk(&chunk).unwrap();
        assert!(events.iter().any(
            |e| matches!(e, StreamEvent::Usage(u) if u.input_tokens == 10 && u.output_tokens == 5)
        ));
    }

    #[test]
    fn request_body_forwards_max_tokens_and_stop_sequences() {
        let provider = MistralProvider::new("key", "mistral-large-latest");
        let mut request = ChatRequest::simple("hello");
        request.max_tokens = Some(321);
        request.stop_sequences = vec!["END".to_string(), "STOP".to_string()];
        let body = provider.build_request_body(&request).unwrap();
        assert_eq!(body["max_tokens"], 321);
        assert_eq!(body["stop"], serde_json::json!(["END", "STOP"]));
    }
}
