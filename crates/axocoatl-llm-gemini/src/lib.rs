//! Google Gemini provider — uses the Gemini REST API (generateContent).

use std::pin::Pin;

use tokio_stream::Stream;

use axocoatl_core::{MessageContent, MessageRole, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};

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

// The `v1` API serves current models; `v1beta` 404s them (e.g. gemini-2.5-flash).
const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1/models";

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
    /// by `chat` and `chat_stream`.
    ///
    /// The Gemini `v1` API has no `systemInstruction` field, so any system
    /// message is folded into the first user turn's text.
    fn build_request_body(&self, request: &ChatRequest) -> serde_json::Value {
        // Gemini uses a different message format: "contents" with "parts".
        let mut system_text: Option<String> = None;
        let mut contents: Vec<serde_json::Value> = Vec::new();

        for msg in &request.messages {
            // For User messages with multimodal Parts, build Gemini's native
            // parts array: text + inline_data { mime_type, data }.
            let parts_json: Vec<serde_json::Value> = if matches!(msg.role, MessageRole::User) {
                if let MessageContent::Parts(parts) = &msg.content {
                    gemini_parts(parts)
                } else {
                    vec![serde_json::json!({"text": match &msg.content {
                        MessageContent::Text(s) => s.clone(),
                        _ => String::new(),
                    }})]
                }
            } else {
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
                vec![serde_json::json!({"text": text})]
            };

            match msg.role {
                MessageRole::System => {
                    // `v1` has no systemInstruction field; accumulate and fold
                    // into the first user turn below.
                    let text = parts_json
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    system_text = Some(match system_text {
                        Some(prev) => format!("{prev}\n{text}"),
                        None => text,
                    });
                }
                MessageRole::User => {
                    contents.push(serde_json::json!({ "role": "user", "parts": parts_json }));
                }
                MessageRole::Assistant => {
                    contents.push(serde_json::json!({ "role": "model", "parts": parts_json }));
                }
                MessageRole::Tool => {
                    contents.push(serde_json::json!({ "role": "user", "parts": parts_json }));
                }
            }
        }

        // Fold the system prompt into the first user turn (Gemini v1 has no
        // systemInstruction field).
        if let Some(sys) = system_text {
            if let Some(first_user) = contents.iter_mut().find(|c| c["role"] == "user") {
                if let Some(parts) = first_user["parts"].as_array_mut() {
                    parts.insert(0, serde_json::json!({ "text": format!("{sys}\n\n") }));
                }
            } else {
                contents.insert(
                    0,
                    serde_json::json!({ "role": "user", "parts": [{ "text": sys }] }),
                );
            }
        }

        let mut body = serde_json::json!({ "contents": contents });
        let mut gen_config = serde_json::Map::new();
        if let Some(max) = request.max_tokens {
            gen_config.insert("maxOutputTokens".to_string(), serde_json::json!(max));
        }
        if let Some(temp) = request.temperature {
            gen_config.insert("temperature".to_string(), serde_json::json!(temp));
        }
        if !gen_config.is_empty() {
            body["generationConfig"] = serde_json::Value::Object(gen_config);
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
            // Tool-calling is not yet wired for Gemini (no functionDeclarations
            // sent, no functionCall parsed). Tracked as a follow-up to #3.
            tool_calling: false,
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

        let content = resp["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let finish_reason = match resp["candidates"][0]["finishReason"].as_str() {
            Some("STOP") => FinishReason::Stop,
            Some("MAX_TOKENS") => FinishReason::MaxTokens,
            Some("SAFETY") => FinishReason::ContentFilter,
            _ => FinishReason::Stop,
        };

        Ok(ChatResponse {
            content,
            // Tool-call parsing for Gemini is a follow-up (see capabilities()).
            tool_calls: vec![],
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
        // Tool-calling is not implemented for Gemini yet.
        assert!(!caps.tool_calling);
        assert_eq!(caps.max_context_tokens, 1_000_000);
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
    fn system_prompt_folds_into_first_user_turn() {
        let p = GeminiProvider::new("key", "gemini-2.5-flash");
        let req = ChatRequest::with_system("Be brief.", "Hi");
        let body = p.build_request_body(&req);
        // Gemini v1 rejects a systemInstruction field — it must NOT be present.
        assert!(body.get("system_instruction").is_none());
        assert!(body.get("systemInstruction").is_none());
        // The system text is folded into the first user turn instead.
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents[0]["role"], "user");
        let first_text = contents[0]["parts"][0]["text"].as_str().unwrap();
        assert!(first_text.contains("Be brief."));
    }
}
