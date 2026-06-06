use std::pin::Pin;

use reqwest::header::CONTENT_TYPE;
use tokio_stream::Stream;

use axocoatl_core::{MessageContent, MessageRole, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent, ToolCall,
};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

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
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    fn build_request_body(&self, request: &ChatRequest) -> serde_json::Value {
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
                    system_prompt = Some(text);
                }
                MessageRole::User => {
                    messages.push(serde_json::json!({"role": "user", "content": text}));
                }
                MessageRole::Assistant => {
                    if msg.tool_calls.is_empty() {
                        messages.push(serde_json::json!({"role": "assistant", "content": text}));
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

        body
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
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            structured_output: true,
            vision: self.model.contains("sonnet") || self.model.contains("opus"),
            reasoning: self.model.contains("opus"),
            embeddings: false,
            max_context_tokens: 200_000,
            max_output_tokens: 64_000,
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let body = self.build_request_body(&request);

        let response = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        let status = response.status();
        if status == 429 {
            return Err(ProviderError::RateLimited {
                provider: "anthropic".to_string(),
                retry_after_secs: None,
            });
        }
        if status == 401 {
            return Err(ProviderError::AuthError {
                provider: "anthropic".to_string(),
            });
        }

        let resp_body: serde_json::Value =
            response.json().await.map_err(|e| ProviderError::ApiError {
                provider: "anthropic".to_string(),
                status: status.as_u16(),
                message: e.to_string(),
            })?;

        if !status.is_success() {
            return Err(ProviderError::ApiError {
                provider: "anthropic".to_string(),
                status: status.as_u16(),
                message: resp_body["error"]["message"]
                    .as_str()
                    .unwrap_or("Unknown error")
                    .to_string(),
            });
        }

        // Extract content from response
        let content = resp_body["content"]
            .as_array()
            .and_then(|arr| {
                arr.iter()
                    .filter_map(|block| {
                        if block["type"] == "text" {
                            block["text"].as_str().map(String::from)
                        } else {
                            None
                        }
                    })
                    .next()
            })
            .unwrap_or_default();

        // Extract tool calls
        let tool_calls = resp_body["content"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|block| {
                        if block["type"] == "tool_use" {
                            Some(ToolCall {
                                id: block["id"].as_str().unwrap_or("").to_string(),
                                name: block["name"].as_str().unwrap_or("").to_string(),
                                arguments: block["input"].clone(),
                            })
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let finish_reason = match resp_body["stop_reason"].as_str() {
            Some("end_turn") => FinishReason::Stop,
            Some("tool_use") => FinishReason::ToolUse,
            Some("max_tokens") => FinishReason::MaxTokens,
            _ => FinishReason::Stop,
        };

        Ok(ChatResponse {
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
                .unwrap_or(&self.model)
                .to_string(),
            provider: "anthropic".to_string(),
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

        let req = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(CONTENT_TYPE, "application/json")
            .json(&body);

        let mut es = EventSource::new(req).map_err(|e| ProviderError::Stream(e.to_string()))?;

        let stream = async_stream::try_stream! {
            // Track current tool call state for streaming tool_use deltas
            let mut current_tool_id = String::new();
            let mut current_tool_name: Option<String>;

            while let Some(event) = es.next().await {
                match event {
                    Ok(Event::Open) => {}
                    Ok(Event::Message(msg)) => {
                        let data: serde_json::Value = serde_json::from_str(&msg.data)
                            .map_err(|e| ProviderError::Stream(format!("JSON parse: {e}")))?;

                        match data["type"].as_str() {
                            Some("content_block_start") => {
                                let block = &data["content_block"];
                                if block["type"] == "tool_use" {
                                    current_tool_id = block["id"].as_str().unwrap_or("").to_string();
                                    current_tool_name = block["name"].as_str().map(String::from);
                                    // Anthropic repeats the full id on every delta,
                                    // so id-correlation suffices — no index needed.
                                    yield StreamEvent::ToolCallDelta {
                                        index: None,
                                        id: current_tool_id.clone(),
                                        name: current_tool_name.clone(),
                                        args_delta: String::new(),
                                    };
                                }
                            }
                            Some("content_block_delta") => {
                                let delta = &data["delta"];
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
                                            yield StreamEvent::ToolCallDelta {
                                                index: None,
                                                id: current_tool_id.clone(),
                                                name: None,
                                                args_delta: json.to_string(),
                                            };
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Some("message_delta") => {
                                // Usage stats
                                if let Some(usage) = data.get("usage") {
                                    let output_tokens = usage["output_tokens"].as_u64().unwrap_or(0) as usize;
                                    yield StreamEvent::Usage(TokenUsageStats {
                                        input_tokens: 0,
                                        output_tokens,
                                        reasoning_tokens: None,
                                    });
                                }
                                // Stop reason comes in message_delta, NOT message_stop
                                if let Some(stop_reason) = data["delta"]["stop_reason"].as_str() {
                                    let finish = match stop_reason {
                                        "tool_use" => FinishReason::ToolUse,
                                        "max_tokens" => FinishReason::MaxTokens,
                                        _ => FinishReason::Stop,
                                    };
                                    yield StreamEvent::Done { finish_reason: finish };
                                }
                            }
                            Some("message_start") => {
                                if let Some(usage) = data["message"]["usage"].as_object() {
                                    let input_tokens = usage.get("input_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0) as usize;
                                    yield StreamEvent::Usage(TokenUsageStats {
                                        input_tokens,
                                        output_tokens: 0,
                                        reasoning_tokens: None,
                                    });
                                }
                            }
                            Some("message_stop") => {
                                // Stream is complete — break out
                                break;
                            }
                            Some("error") => {
                                let msg = data["error"]["message"]
                                    .as_str()
                                    .unwrap_or("Unknown streaming error")
                                    .to_string();
                                Err(ProviderError::Stream(msg))?;
                            }
                            _ => {}
                        }
                    }
                    Err(reqwest_eventsource::Error::StreamEnded) => {
                        break;
                    }
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
    fn build_request_body_with_system() {
        let provider = AnthropicProvider::new("test-key", "claude-sonnet-4-6");
        let request = ChatRequest::with_system("You are helpful.", "Hello");
        let body = provider.build_request_body(&request);

        assert_eq!(body["system"], "You are helpful.");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "Hello");
        assert_eq!(body["model"], "claude-sonnet-4-6");
    }

    #[test]
    fn build_request_body_no_system() {
        let provider = AnthropicProvider::new("test-key", "claude-haiku-4-5-20251001");
        let request = ChatRequest::simple("Hi");
        let body = provider.build_request_body(&request);

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
        let body = provider.build_request_body(&request);

        // Regression: tools must reach the outbound Anthropic request.
        assert!(body["tools"].is_array());
        assert_eq!(body["tools"][0]["name"], "get_weather");
        assert_eq!(body["tools"][0]["input_schema"]["required"][0], "location");
    }

    #[test]
    fn build_request_body_omits_tools_when_none() {
        let provider = AnthropicProvider::new("test-key", "claude-sonnet-4-6");
        let request = ChatRequest::simple("Hello");
        let body = provider.build_request_body(&request);

        assert!(body.get("tools").is_none());
    }

    #[test]
    fn capabilities_correct() {
        let provider = AnthropicProvider::new("key", "claude-sonnet-4-6");
        let caps = provider.capabilities();
        assert!(caps.vision);
        assert!(caps.tool_calling);
        assert_eq!(caps.max_context_tokens, 200_000);
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
                }],
            ));
        let body = provider.build_request_body(&request);

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
                    },
                    ToolCall {
                        id: "toolu_2".to_string(),
                        name: "get_weather".to_string(),
                        arguments: serde_json::json!({ "location": "LA" }),
                    },
                ],
            ));
        request
            .messages
            .push(ChatMessage::tool_result("72F", "get_weather", "toolu_1"));
        request
            .messages
            .push(ChatMessage::tool_result("80F", "get_weather", "toolu_2"));
        let body = provider.build_request_body(&request);

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
}
