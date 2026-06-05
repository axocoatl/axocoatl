//! Mistral AI provider — uses the OpenAI-compatible chat completions API.

use std::pin::Pin;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use tokio_stream::Stream;

use axocoatl_core::{MessageContent, MessageRole, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};

const MISTRAL_API_URL: &str = "https://api.mistral.ai/v1/chat/completions";

/// Parse one Mistral streaming chunk (OpenAI-compatible) into stream events.
/// Pure + synchronous so it is unit-tested without the network.
fn parse_mistral_chunk(data: &serde_json::Value) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    let choice = &data["choices"][0];

    if let Some(text) = choice["delta"]["content"].as_str() {
        if !text.is_empty() {
            events.push(StreamEvent::TextDelta {
                delta: text.to_string(),
            });
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
            _ => FinishReason::Stop,
        };
        events.push(StreamEvent::Done {
            finish_reason: finish,
        });
    }

    events
}

/// Mistral AI provider using their OpenAI-compatible API.
pub struct MistralProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl MistralProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    /// Build the OpenAI-compatible request body shared by `chat` and `chat_stream`.
    fn build_request_body(&self, request: &ChatRequest) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    MessageRole::System => "system",
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::Tool => "tool",
                };
                // For User messages with multimodal parts, emit Mistral's
                // OpenAI-compatible content array (works with pixtral models;
                // non-vision models will reject the image — that's expected).
                if matches!(m.role, MessageRole::User) {
                    if let MessageContent::Parts(parts) = &m.content {
                        let arr: Vec<serde_json::Value> = parts
                            .iter()
                            .map(|p| match p {
                                axocoatl_core::ContentPart::Text(s) => {
                                    serde_json::json!({"type": "text", "text": s})
                                }
                                axocoatl_core::ContentPart::Image { url, .. } => {
                                    serde_json::json!({
                                        "type": "image_url",
                                        "image_url": url,
                                    })
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
                serde_json::json!({"role": role, "content": content})
            })
            .collect();

        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);
        let mut body = serde_json::json!({
            "model": model_for_call,
            "messages": messages,
        });
        if let Some(max) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }
        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        body
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
        ProviderCapabilities {
            streaming: true,
            // Tool-calling is not yet wired for Mistral (no tools sent, no
            // tool_calls parsed). Tracked as a follow-up to #3.
            tool_calling: false,
            structured_output: true,
            vision: self.model.contains("pixtral"),
            reasoning: false,
            embeddings: false,
            max_context_tokens: 128_000,
            max_output_tokens: 4_096,
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let body = self.build_request_body(&request);

        let response = self
            .client
            .post(MISTRAL_API_URL)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        let status = response.status();
        if status == 429 {
            return Err(ProviderError::RateLimited {
                provider: "mistral".to_string(),
                retry_after_secs: None,
            });
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::ApiError {
                provider: "mistral".to_string(),
                status: status.as_u16(),
                message: text,
            });
        }

        let resp: serde_json::Value =
            response.json().await.map_err(|e| ProviderError::ApiError {
                provider: "mistral".to_string(),
                status: 200,
                message: e.to_string(),
            })?;

        Ok(ChatResponse {
            content: resp["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            // Tool-call parsing for Mistral is a follow-up (see capabilities()).
            tool_calls: vec![],
            finish_reason: match resp["choices"][0]["finish_reason"].as_str() {
                Some("stop") => FinishReason::Stop,
                Some("length") => FinishReason::MaxTokens,
                Some("tool_calls") => FinishReason::ToolUse,
                _ => FinishReason::Stop,
            },
            usage: TokenUsageStats {
                input_tokens: resp["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as usize,
                output_tokens: resp["usage"]["completion_tokens"].as_u64().unwrap_or(0) as usize,
                reasoning_tokens: None,
            },
            model: resp["model"].as_str().unwrap_or(&self.model).to_string(),
            provider: "mistral".to_string(),
        })
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        use reqwest_eventsource::{Event, EventSource};
        use tokio_stream::StreamExt;

        let mut body = self.build_request_body(&request);
        body["stream"] = serde_json::json!(true);
        body["stream_options"] = serde_json::json!({ "include_usage": true });

        let req = self
            .client
            .post(MISTRAL_API_URL)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .json(&body);

        let mut es = EventSource::new(req).map_err(|e| ProviderError::Stream(e.to_string()))?;

        let stream = async_stream::try_stream! {
            while let Some(event) = es.next().await {
                match event {
                    Ok(Event::Open) => {}
                    Ok(Event::Message(msg)) => {
                        // OpenAI-compatible streams terminate with a literal
                        // `[DONE]` sentinel rather than valid JSON.
                        if msg.data.trim() == "[DONE]" {
                            break;
                        }
                        let data: serde_json::Value = serde_json::from_str(&msg.data)
                            .map_err(|e| ProviderError::Stream(format!("JSON parse: {e}")))?;
                        for ev in parse_mistral_chunk(&data) {
                            yield ev;
                        }
                    }
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
        let p = MistralProvider::new("key", "mistral-large-latest");
        assert_eq!(p.provider_id(), "mistral");
        assert_eq!(p.model_id(), "mistral-large-latest");
    }

    #[test]
    fn capabilities() {
        let p = MistralProvider::new("key", "mistral-large-latest");
        let caps = p.capabilities();
        assert!(caps.streaming);
        assert!(!caps.vision);
        // Tool-calling is not implemented for Mistral yet.
        assert!(!caps.tool_calling);
    }

    #[test]
    fn parse_chunk_text_delta() {
        let chunk = serde_json::json!({ "choices": [{ "delta": { "content": "Hello" } }] });
        let events = parse_mistral_chunk(&chunk);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::TextDelta { delta } => assert_eq!(delta, "Hello"),
            _ => panic!("expected TextDelta"),
        }
    }

    #[test]
    fn parse_chunk_finish() {
        let chunk = serde_json::json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] });
        let events = parse_mistral_chunk(&chunk);
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::Done {
                finish_reason: FinishReason::Stop
            }
        )));
    }

    #[test]
    fn parse_chunk_usage_final() {
        // Final chunk (include_usage): empty choices + usage.
        let chunk = serde_json::json!({
            "choices": [],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
        });
        let events = parse_mistral_chunk(&chunk);
        assert!(events.iter().any(
            |e| matches!(e, StreamEvent::Usage(u) if u.input_tokens == 10 && u.output_tokens == 5)
        ));
    }
}
