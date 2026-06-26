//! Google Gemini provider — uses the Gemini REST API (generateContent).

use std::pin::Pin;

use tokio_stream::Stream;

use axocoatl_core::{MessageContent, MessageRole, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent, ToolCall, ToolDefinition,
};

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
fn parse_gemini_chunk(data: &serde_json::Value) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    let candidate = &data["candidates"][0];

    if let Some(parts) = candidate["content"]["parts"].as_array() {
        for part in parts {
            if let Some(text) = part["text"].as_str() {
                if !text.is_empty() {
                    events.push(StreamEvent::TextDelta {
                        delta: text.to_string(),
                    });
                }
            }
            // Gemini emits each function call as a complete `functionCall` part
            // (arguments are never fragmented), so one delta carries the whole
            // call. No `index` and a possibly-empty `id` means the accumulator
            // keeps each call as its own entry — exactly what we want.
            if let Some(fc) = part.get("functionCall") {
                let name = fc["name"].as_str().unwrap_or("").to_string();
                if !name.is_empty() {
                    events.push(StreamEvent::ToolCallDelta {
                        index: None,
                        id: fc["id"].as_str().unwrap_or("").to_string(),
                        name: Some(name),
                        args_delta: serde_json::to_string(&fc["args"])
                            .unwrap_or_else(|_| "{}".to_string()),
                    });
                }
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
        let finish = match reason {
            "STOP" => FinishReason::Stop,
            "MAX_TOKENS" => FinishReason::MaxTokens,
            "SAFETY" => FinishReason::ContentFilter,
            _ => FinishReason::Stop,
        };
        events.push(StreamEvent::Done {
            finish_reason: finish,
        });
    }

    events
}

/// Google Gemini provider using the generateContent REST API.
pub struct GeminiProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl GeminiProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    /// REST endpoint for a model. The API key is **not** in the URL — it's sent
    /// in the `x-goog-api-key` header (see [`chat`]) so it can't leak into
    /// reqwest's network-error strings or any log line that prints the URL.
    fn endpoint_for(&self, model: &str) -> String {
        format!("{}/{}:generateContent", GEMINI_API_BASE, model)
    }

    /// SSE streaming endpoint (`streamGenerateContent?alt=sse`).
    fn stream_endpoint_for(&self, model: &str) -> String {
        format!(
            "{}/{}:streamGenerateContent?alt=sse",
            GEMINI_API_BASE, model
        )
    }

    /// Build the Gemini request body (`contents` + `generationConfig`), shared
    /// by `chat` and `chat_stream`. System messages map to the native
    /// `systemInstruction` field (supported by the `v1beta` endpoint).
    fn build_request_body(&self, request: &ChatRequest) -> serde_json::Value {
        // Gemini uses a different message format: "contents" with "parts".
        let mut system_text: Option<String> = None;
        let mut contents: Vec<serde_json::Value> = Vec::new();

        for msg in &request.messages {
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
                    let mut parts: Vec<serde_json::Value> = Vec::new();
                    let text = flatten_text(&msg.content);
                    if !text.is_empty() {
                        parts.push(serde_json::json!({ "text": text }));
                    }
                    for tc in &msg.tool_calls {
                        let mut fc = serde_json::json!({ "name": tc.name, "args": tc.arguments });
                        if !tc.id.is_empty() {
                            fc["id"] = serde_json::json!(tc.id);
                        }
                        parts.push(serde_json::json!({ "functionCall": fc }));
                    }
                    // Gemini rejects an empty parts array; guarantee one part.
                    if parts.is_empty() {
                        parts.push(serde_json::json!({ "text": "" }));
                    }
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
                    contents.push(serde_json::json!({
                        "role": "user",
                        "parts": [{ "functionResponse": fr }]
                    }));
                }
            }
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
        body
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
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            structured_output: true,
            vision: true,
            reasoning: self.model.contains("thinking"),
            embeddings: false,
            max_context_tokens: 1_000_000, // Gemini 1.5 Pro
            max_output_tokens: 8_192,
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let body = self.build_request_body(&request);
        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);

        let response = self
            .client
            .post(self.endpoint_for(model_for_call))
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        let status = response.status();
        if status == 429 {
            return Err(ProviderError::RateLimited {
                provider: "gemini".to_string(),
                retry_after_secs: None,
            });
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::ApiError {
                provider: "gemini".to_string(),
                status: status.as_u16(),
                message: text,
            });
        }

        let resp: serde_json::Value =
            response.json().await.map_err(|e| ProviderError::ApiError {
                provider: "gemini".to_string(),
                status: 200,
                message: e.to_string(),
            })?;

        // Walk every part: text parts concatenate into content, functionCall
        // parts become tool calls.
        let mut content = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        if let Some(parts) = resp["candidates"][0]["content"]["parts"].as_array() {
            for part in parts {
                if let Some(text) = part["text"].as_str() {
                    content.push_str(text);
                }
                if let Some(fc) = part.get("functionCall") {
                    let name = fc["name"].as_str().unwrap_or("").to_string();
                    if !name.is_empty() {
                        tool_calls.push(ToolCall {
                            id: fc["id"].as_str().unwrap_or("").to_string(),
                            name,
                            arguments: fc["args"].clone(),
                        });
                    }
                }
            }
        }

        let finish_reason = match resp["candidates"][0]["finishReason"].as_str() {
            Some("STOP") => FinishReason::Stop,
            Some("MAX_TOKENS") => FinishReason::MaxTokens,
            Some("SAFETY") => FinishReason::ContentFilter,
            _ => FinishReason::Stop,
        };

        Ok(ChatResponse {
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
            model: self.model.clone(),
            provider: "gemini".to_string(),
        })
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        use reqwest_eventsource::{Event, EventSource};
        use tokio_stream::StreamExt;

        let body = self.build_request_body(&request);
        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);

        let req = self
            .client
            .post(self.stream_endpoint_for(model_for_call))
            .header("x-goog-api-key", &self.api_key)
            .json(&body);

        let mut es = EventSource::new(req).map_err(|e| ProviderError::Stream(e.to_string()))?;

        let stream = async_stream::try_stream! {
            while let Some(event) = es.next().await {
                match event {
                    Ok(Event::Open) => {}
                    Ok(Event::Message(msg)) => {
                        let data: serde_json::Value = serde_json::from_str(&msg.data)
                            .map_err(|e| ProviderError::Stream(format!("JSON parse: {e}")))?;
                        for ev in parse_gemini_chunk(&data) {
                            yield ev;
                        }
                    }
                    // Gemini closes the connection when generation completes;
                    // that surfaces as StreamEnded, not an error.
                    Err(reqwest_eventsource::Error::StreamEnded) => break,
                    Err(e) => {
                        Err(ProviderError::Stream(e.to_string()))?;
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(caps.max_context_tokens, 1_000_000);
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
        let body = p.build_request_body(&request);
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
                }],
            ));
        request.messages.push(ChatMessage::tool_result(
            "{\"temp\":72}",
            "get_weather",
            "fc_1",
        ));
        let body = p.build_request_body(&request);
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
        let events = parse_gemini_chunk(&chunk);
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
        let events = parse_gemini_chunk(&chunk);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::TextDelta { delta } => assert_eq!(delta, "Hello"),
            _ => panic!("expected TextDelta"),
        }
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
        let events = parse_gemini_chunk(&chunk);
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
    fn system_prompt_uses_native_system_instruction() {
        let p = GeminiProvider::new("key", "gemini-2.5-flash");
        let req = ChatRequest::with_system("Be brief.", "Hi");
        let body = p.build_request_body(&req);
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
}
