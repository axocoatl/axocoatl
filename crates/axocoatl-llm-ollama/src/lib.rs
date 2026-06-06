use std::pin::Pin;

use reqwest::header::CONTENT_TYPE;
use tokio_stream::Stream;

use axocoatl_core::{MessageContent, MessageRole, TokenUsageStats};
use axocoatl_llm::{
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

/// Ollama / LM Studio provider using the OpenAI-compatible chat completions endpoint.
/// Works with any server that exposes `/v1/chat/completions`.
pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

impl OllamaProvider {
    /// Create a provider for a local Ollama instance (default: http://localhost:11434).
    pub fn new(model: impl Into<String>) -> Self {
        Self::with_base_url("http://localhost:11434", model)
    }

    /// Create with a custom base URL (for LM Studio, remote Ollama, etc.).
    pub fn with_base_url(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url)
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
            structured_output: false,
            vision: false,
            reasoning: false,
            embeddings: false,
            max_context_tokens: 128_000, // Model-dependent
            max_output_tokens: 4_096,
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let messages = ollama_messages(&request.messages);

        // `model_override` lets the Chat tab pick a different model per turn
        // without spinning up a new provider instance. Falls back to the
        // configured default when None.
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
        if !request.tools.is_empty() {
            body["tools"] = tools_json(&request.tools);
        }

        let response = self
            .client
            .post(self.endpoint())
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let err_text = response.text().await.unwrap_or_default();
            return Err(ProviderError::ApiError {
                provider: "ollama".to_string(),
                status: status.as_u16(),
                message: err_text,
            });
        }

        let resp_body: serde_json::Value =
            response.json().await.map_err(|e| ProviderError::ApiError {
                provider: "ollama".to_string(),
                status: 200,
                message: e.to_string(),
            })?;

        let content = resp_body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        // Extract tool calls from OpenAI-compatible response
        let tool_calls = resp_body["choices"][0]["message"]["tool_calls"]
            .as_array()
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|tc| {
                        let id = tc["id"].as_str().unwrap_or("").to_string();
                        let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                        let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
                        let arguments =
                            serde_json::from_str(args_str).unwrap_or(serde_json::Value::Null);
                        if name.is_empty() {
                            None
                        } else {
                            Some(axocoatl_llm::ToolCall {
                                id,
                                name,
                                arguments,
                            })
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let finish_reason = match resp_body["choices"][0]["finish_reason"].as_str() {
            Some("stop") => FinishReason::Stop,
            Some("tool_calls") => FinishReason::ToolUse,
            Some("length") => FinishReason::MaxTokens,
            _ => FinishReason::Stop,
        };

        Ok(ChatResponse {
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
                .unwrap_or(&self.model)
                .to_string(),
            provider: "ollama".to_string(),
        })
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        use tokio_stream::StreamExt;

        let messages = ollama_messages(&request.messages);

        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);
        let mut body = serde_json::json!({
            "model": model_for_call,
            "messages": messages,
            "stream": true,
        });

        if let Some(max) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }
        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if !request.tools.is_empty() {
            body["tools"] = tools_json(&request.tools);
        }

        let response = self
            .client
            .post(self.endpoint())
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let err_text = response.text().await.unwrap_or_default();
            return Err(ProviderError::ApiError {
                provider: "ollama".to_string(),
                status: status.as_u16(),
                message: err_text,
            });
        }

        // OpenAI-compatible SSE: each line is "data: {json}\n\n" or "data: [DONE]"
        let byte_stream = response.bytes_stream();
        let mut lines_stream = tokio_stream::StreamExt::map(byte_stream, |chunk| {
            chunk.map_err(|e| ProviderError::Stream(e.to_string()))
        });

        let stream = async_stream::try_stream! {
            let mut buffer = String::new();

            while let Some(chunk) = lines_stream.next().await {
                let bytes = chunk?;
                buffer.push_str(&String::from_utf8_lossy(&bytes));

                // Process complete SSE lines from buffer
                while let Some(line_end) = buffer.find('\n') {
                    let line = buffer[..line_end].trim().to_string();
                    buffer = buffer[line_end + 1..].to_string();

                    if line.is_empty() {
                        continue;
                    }

                    let data = if let Some(stripped) = line.strip_prefix("data: ") {
                        stripped
                    } else {
                        continue;
                    };

                    if data == "[DONE]" {
                        // Only emit Done if we haven't already from a finish_reason chunk
                        break;
                    }

                    let parsed: serde_json::Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::debug!(error = %e, "Skipping unparseable SSE chunk");
                            continue;
                        }
                    };

                    if let Some(choices) = parsed["choices"].as_array() {
                        for choice in choices {
                            // Text content deltas
                            if let Some(content) = choice["delta"]["content"].as_str() {
                                if !content.is_empty() {
                                    yield StreamEvent::TextDelta {
                                        delta: content.to_string(),
                                    };
                                }
                            }

                            // Tool call deltas. OpenAI-compatible streams send the
                            // id once and key later argument fragments by `index`.
                            if let Some(tool_calls) = choice["delta"]["tool_calls"].as_array() {
                                for tc in tool_calls {
                                    let index = tc["index"].as_u64().map(|i| i as usize);
                                    let id = tc["id"].as_str().unwrap_or("").to_string();
                                    let name = tc["function"]["name"].as_str().map(String::from);
                                    let args_delta = tc["function"]["arguments"]
                                        .as_str()
                                        .unwrap_or("")
                                        .to_string();
                                    yield StreamEvent::ToolCallDelta { index, id, name, args_delta };
                                }
                            }

                            // Finish reason
                            if let Some(reason) = choice["finish_reason"].as_str() {
                                let finish = match reason {
                                    "stop" => FinishReason::Stop,
                                    "tool_calls" => FinishReason::ToolUse,
                                    "length" => FinishReason::MaxTokens,
                                    _ => FinishReason::Stop,
                                };
                                if let Some(usage) = parsed.get("usage") {
                                    yield StreamEvent::Usage(TokenUsageStats {
                                        input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0) as usize,
                                        output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0) as usize,
                                        reasoning_tokens: None,
                                    });
                                }
                                yield StreamEvent::Done { finish_reason: finish };
                            }
                        }
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
    fn default_base_url() {
        let provider = OllamaProvider::new("llama3");
        assert_eq!(
            provider.endpoint(),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(provider.model_id(), "llama3");
        assert_eq!(provider.provider_id(), "ollama");
    }

    #[test]
    fn custom_base_url() {
        let provider = OllamaProvider::with_base_url("http://gpu-server:11434", "mistral");
        assert_eq!(
            provider.endpoint(),
            "http://gpu-server:11434/v1/chat/completions"
        );
    }

    #[test]
    fn trailing_slash_stripped() {
        let provider = OllamaProvider::with_base_url("http://localhost:11434/", "llama3");
        assert_eq!(
            provider.endpoint(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn capabilities_local_model() {
        let provider = OllamaProvider::new("llama3");
        let caps = provider.capabilities();
        assert!(!caps.vision);
        assert!(caps.tool_calling);
        assert_eq!(caps.max_context_tokens, 128_000);
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
}
