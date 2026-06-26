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

/// Strip a single leading and trailing newline — the formatting wrapper the XML
/// tool-call shapes put around multi-line values — while preserving inner
/// indentation. The inner whitespace matters: `edit_file` matches `old` exactly.
fn strip_wrapping_newlines(v: &str) -> String {
    let v = v
        .strip_prefix("\r\n")
        .or_else(|| v.strip_prefix('\n'))
        .unwrap_or(v);
    let v = v
        .strip_suffix("\r\n")
        .or_else(|| v.strip_suffix('\n'))
        .unwrap_or(v);
    v.to_string()
}

/// Recover tool calls a model emitted as *text* in `content` rather than in the
/// structured `tool_calls` field. Most models Ollama serves emit the structured
/// form, but some local coder models (e.g. qwen3-coder) sometimes fall back to
/// text. Two shapes are handled:
///
/// ```text
/// <function=NAME><parameter=KEY>VALUE</parameter>…</function>   (Qwen-coder)
/// <tool_call>{"name":"NAME","arguments":{…}}</tool_call>        (Hermes JSON)
/// ```
///
/// Only calls whose name was actually offered in `tool_names` are returned, so
/// ordinary prose that happens to contain the markers is never misread as a call.
fn parse_text_tool_calls(content: &str, tool_names: &[String]) -> Vec<axocoatl_llm::ToolCall> {
    let mut calls: Vec<axocoatl_llm::ToolCall> = Vec::new();
    let known = |name: &str| tool_names.iter().any(|t| t == name);

    // Shape 1: <function=NAME> … <parameter=KEY>VALUE</parameter> … </function>
    let mut rest = content;
    while let Some(start) = rest.find("<function=") {
        let after = &rest[start + "<function=".len()..];
        let Some(name_end) = after.find('>') else {
            break;
        };
        let name = after[..name_end].trim().to_string();
        let body_start = name_end + 1;
        // Require the closing tag — a complete block, not prose that merely
        // mentions `<function=…>`.
        let Some(close) = after[body_start..].find("</function>") else {
            break;
        };
        let body = &after[body_start..body_start + close];
        let next = &after[body_start + close + "</function>".len()..];
        if known(&name) {
            let mut args = serde_json::Map::new();
            let mut pbody = body;
            while let Some(ps) = pbody.find("<parameter=") {
                let pafter = &pbody[ps + "<parameter=".len()..];
                let Some(key_end) = pafter.find('>') else {
                    break;
                };
                let key = pafter[..key_end].trim().to_string();
                let val_start = key_end + 1;
                let (val, pnext) = match pafter[val_start..].find("</parameter>") {
                    Some(e) => (
                        &pafter[val_start..val_start + e],
                        &pafter[val_start + e + "</parameter>".len()..],
                    ),
                    None => (&pafter[val_start..], ""),
                };
                args.insert(key, serde_json::Value::String(strip_wrapping_newlines(val)));
                pbody = pnext;
            }
            calls.push(axocoatl_llm::ToolCall {
                id: format!("call_{}", calls.len()),
                name,
                arguments: serde_json::Value::Object(args),
            });
        }
        rest = next;
    }

    // Shape 2: <tool_call>{"name":…,"arguments":{…}}</tool_call>
    let mut rest = content;
    while let Some(start) = rest.find("<tool_call>") {
        let after = &rest[start + "<tool_call>".len()..];
        let Some(close) = after.find("</tool_call>") else {
            break;
        };
        let inner = &after[..close];
        let next = &after[close + "</tool_call>".len()..];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(inner.trim()) {
            if let Some(name) = v["name"].as_str() {
                if known(name) {
                    calls.push(axocoatl_llm::ToolCall {
                        id: format!("call_{}", calls.len()),
                        name: name.to_string(),
                        arguments: v
                            .get("arguments")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    });
                }
            }
        }
        rest = next;
    }

    calls
}

/// Largest char-boundary offset of `s` that still leaves `holdback` bytes
/// unflushed, so a tool-call marker split across stream deltas is never
/// half-shown before we can recognise it.
fn flush_boundary(s: &str, holdback: usize) -> usize {
    if s.len() <= holdback {
        return 0;
    }
    let mut end = s.len() - holdback;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
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
        if let Some(top_p) = request.top_p {
            body["top_p"] = serde_json::json!(top_p);
        }
        if request.response_format == Some(axocoatl_core::ResponseFormat::Json) {
            body["format"] = serde_json::json!("json");
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
        let mut tool_calls = resp_body["choices"][0]["message"]["tool_calls"]
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

        // Fallback: some local models emit tool calls as text in `content`
        // (`<function=NAME>…`) instead of the structured `tool_calls` field.
        // Recover them so the call still executes — guarded to offered tools.
        if tool_calls.is_empty() {
            let tool_names: Vec<String> = request.tools.iter().map(|t| t.name.clone()).collect();
            tool_calls = parse_text_tool_calls(&content, &tool_names);
        }

        let finish_reason = if !tool_calls.is_empty() {
            FinishReason::ToolUse
        } else {
            match resp_body["choices"][0]["finish_reason"].as_str() {
                Some("length") => FinishReason::MaxTokens,
                _ => FinishReason::Stop,
            }
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
        if let Some(top_p) = request.top_p {
            body["top_p"] = serde_json::json!(top_p);
        }
        if request.response_format == Some(axocoatl_core::ResponseFormat::Json) {
            body["format"] = serde_json::json!("json");
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

        // Captured for the text-tool-call fallback in the finish branch below.
        let tool_names: Vec<String> = request.tools.iter().map(|t| t.name.clone()).collect();

        let stream = async_stream::try_stream! {
            let mut buffer = String::new();
            // Accumulated assistant text plus how much we've already streamed out.
            // Lets the finish branch recover a tool call a model emits as text while
            // keeping its raw markup off-screen.
            let mut content_acc = String::new();
            let mut flushed = 0usize;
            let mut in_text_tool_call = false;
            let mut saw_struct_tool_call = false;

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
                            // Text content deltas. Accumulate everything so the finish
                            // branch can recover a tool call emitted as text. Until a
                            // `<function=`/`<tool_call>` marker appears we stream text
                            // through, holding back a short tail so a marker split
                            // across deltas is never half-shown.
                            if let Some(content) = choice["delta"]["content"].as_str() {
                                if !content.is_empty() {
                                    content_acc.push_str(content);
                                    if !in_text_tool_call {
                                        if content_acc[flushed..].contains("<function=")
                                            || content_acc[flushed..].contains("<tool_call>")
                                        {
                                            in_text_tool_call = true;
                                        } else {
                                            let end = flush_boundary(&content_acc, 16);
                                            if end > flushed {
                                                let delta = content_acc[flushed..end].to_string();
                                                flushed = end;
                                                yield StreamEvent::TextDelta { delta };
                                            }
                                        }
                                    }
                                }
                            }

                            // Structured tool call deltas (the usual path). OpenAI-
                            // compatible streams send the id once and key later
                            // argument fragments by `index`.
                            if let Some(tool_calls) = choice["delta"]["tool_calls"].as_array() {
                                saw_struct_tool_call = true;
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
                                if let Some(usage) = parsed.get("usage") {
                                    yield StreamEvent::Usage(TokenUsageStats {
                                        input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0) as usize,
                                        output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0) as usize,
                                        reasoning_tokens: None,
                                    });
                                }

                                // Recover a tool call emitted as text when the model
                                // never sent a structured one.
                                let recovered = if saw_struct_tool_call {
                                    Vec::new()
                                } else {
                                    parse_text_tool_calls(&content_acc, &tool_names)
                                };

                                if !recovered.is_empty() {
                                    for (i, call) in recovered.iter().enumerate() {
                                        yield StreamEvent::ToolCallDelta {
                                            index: Some(i),
                                            id: call.id.clone(),
                                            name: Some(call.name.clone()),
                                            args_delta: serde_json::to_string(&call.arguments)
                                                .unwrap_or_else(|_| "{}".to_string()),
                                        };
                                    }
                                    yield StreamEvent::Done { finish_reason: FinishReason::ToolUse };
                                } else {
                                    // Not a tool call after all — flush any held text.
                                    if flushed < content_acc.len() {
                                        let delta = content_acc[flushed..].to_string();
                                        flushed = content_acc.len();
                                        yield StreamEvent::TextDelta { delta };
                                    }
                                    let finish = match reason {
                                        "stop" => FinishReason::Stop,
                                        "tool_calls" => FinishReason::ToolUse,
                                        "length" => FinishReason::MaxTokens,
                                        _ => FinishReason::Stop,
                                    };
                                    yield StreamEvent::Done { finish_reason: finish };
                                }
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

    #[test]
    fn recovers_qwen_coder_function_tool_call_from_text() {
        // The shape qwen3-coder emits as text when Ollama doesn't convert it.
        // `concat!` keeps the literal 2-space indentation inside the values.
        let content = concat!(
            "I'll update the heading.\n",
            "<function=edit_file>\n",
            "<parameter=path>\nindex.html\n</parameter>\n",
            "<parameter=old>\n  h1 { color: #fff; }\n</parameter>\n",
            "<parameter=new>\n  h1 { color: #9c27b0; font-weight: bold; }\n</parameter>\n",
            "</function>\n</tool_call>",
        );
        let names = vec!["edit_file".to_string(), "write_file".to_string()];
        let calls = parse_text_tool_calls(content, &names);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "edit_file");
        assert_eq!(calls[0].arguments["path"], "index.html");
        // Inner indentation is preserved (exact-match `old`); only the wrapper
        // newlines are stripped.
        assert_eq!(calls[0].arguments["old"], "  h1 { color: #fff; }");
        assert_eq!(
            calls[0].arguments["new"],
            "  h1 { color: #9c27b0; font-weight: bold; }"
        );
    }

    #[test]
    fn recovers_hermes_json_tool_call() {
        let content = "<tool_call>\n\
            {\"name\": \"write_file\", \"arguments\": {\"path\": \"a.txt\", \"content\": \"hi\"}}\n\
            </tool_call>";
        let names = vec!["write_file".to_string()];
        let calls = parse_text_tool_calls(content, &names);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "write_file");
        assert_eq!(calls[0].arguments["path"], "a.txt");
        assert_eq!(calls[0].arguments["content"], "hi");
    }

    #[test]
    fn ignores_function_names_not_offered() {
        let content = "<function=rm_rf>\n<parameter=path>/</parameter>\n</function>";
        let names = vec!["edit_file".to_string()];
        assert!(parse_text_tool_calls(content, &names).is_empty());
    }

    #[test]
    fn prose_mentioning_a_marker_is_not_a_call() {
        // Offered tool name appears in prose, but with no complete block.
        let content = "Use <function=edit_file> when you need to change a file.";
        let names = vec!["edit_file".to_string()];
        assert!(parse_text_tool_calls(content, &names).is_empty());
    }

    #[test]
    fn no_markers_yields_no_calls() {
        let content = "Just a normal assistant reply with no tool calls at all.";
        let names = vec!["edit_file".to_string()];
        assert!(parse_text_tool_calls(content, &names).is_empty());
    }

    #[test]
    fn strip_wrapping_newlines_keeps_inner_indentation() {
        assert_eq!(
            strip_wrapping_newlines("\n  h1 {\n    color: red;\n  }\n"),
            "  h1 {\n    color: red;\n  }"
        );
        assert_eq!(strip_wrapping_newlines("index.html"), "index.html");
        assert_eq!(strip_wrapping_newlines("\r\nx\r\n"), "x");
    }

    #[test]
    fn flush_boundary_respects_utf8() {
        // 'é' is two bytes; the boundary must not split it.
        let s = "abcdé";
        let b = flush_boundary(s, 1);
        assert!(s.is_char_boundary(b));
        // Short strings hold everything back.
        assert_eq!(flush_boundary("ab", 16), 0);
    }
}
