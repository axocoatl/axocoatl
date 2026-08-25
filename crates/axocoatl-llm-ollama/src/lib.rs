use std::pin::Pin;

use reqwest::header::CONTENT_TYPE;
use tokio_stream::Stream;

use axocoatl_core::{MessageContent, MessageRole, TokenUsageStats};
use axocoatl_llm::{
    provider_tool_metadata,
    transport::{
        bounded_redacted, http_client, network_error, next_stream_item, read_error_text, read_json,
        validated_endpoint, SseDecoder, RESPONSE_TIMEOUT, STREAM_IDLE_TIMEOUT,
        STREAM_TOTAL_TIMEOUT,
    },
    validate_chat_response, validate_provider_request, validate_required_stream_tool_call_ids,
    validate_required_tool_call_id, validate_response_tool_call, validate_stream_terminal,
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};

/// Mirrors the shared portable provider-response limit. Text recovery must
/// enforce it while parsing so a bounded response cannot first materialize an
/// unbounded number of actionable calls.
const MAX_RECOVERED_TOOL_CALLS: usize = 128;
/// Invalid/prose JSON objects are not actionable, but parsing an unlimited
/// number of them would still amplify CPU. This bounds candidate decoding
/// while leaving ample room for malformed prose before a legitimate call.
const MAX_TEXT_TOOL_CANDIDATES: usize = 1_024;

fn text_tool_recovery_error(message: &'static str) -> ProviderError {
    ProviderError::ApiError {
        provider: "ollama".to_string(),
        status: 200,
        message: message.to_string(),
    }
}

fn note_text_tool_candidate(count: &mut usize) -> Result<(), ProviderError> {
    if *count >= MAX_TEXT_TOOL_CANDIDATES {
        return Err(text_tool_recovery_error(
            "Ollama text tool recovery exceeded its bounded candidate scan limit",
        ));
    }
    *count += 1;
    Ok(())
}

fn push_recovered_tool_call(
    calls: &mut Vec<axocoatl_llm::ToolCall>,
    call: axocoatl_llm::ToolCall,
) -> Result<(), ProviderError> {
    if calls.len() >= MAX_RECOVERED_TOOL_CALLS {
        return Err(text_tool_recovery_error(
            "Ollama recovered more than 128 text tool calls",
        ));
    }
    calls.push(call);
    Ok(())
}

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
/// {"name":"NAME","arguments":{…}}                               (bare JSON)
/// ```
///
/// The bare shape matters more than it looks: a model whose calls are never
/// lifted into `tool_calls` is not merely degraded, it is silently useless —
/// it edits nothing and the run still reports success.
///
/// Only calls whose name was actually offered in `tool_names` are returned, so
/// ordinary prose that happens to contain the markers is never misread as a call.
fn parse_text_tool_calls(
    content: &str,
    tool_names: &[String],
) -> Result<Vec<axocoatl_llm::ToolCall>, ProviderError> {
    let mut calls: Vec<axocoatl_llm::ToolCall> = Vec::new();
    let mut candidate_count = 0usize;
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
        note_text_tool_candidate(&mut candidate_count)?;
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
            let call = axocoatl_llm::ToolCall {
                id: format!("call_{}", calls.len()),
                name,
                arguments: serde_json::Value::Object(args),
                provider_metadata: provider_tool_metadata("ollama"),
            };
            push_recovered_tool_call(&mut calls, call)?;
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
        note_text_tool_candidate(&mut candidate_count)?;
        let inner = &after[..close];
        let next = &after[close + "</tool_call>".len()..];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(inner.trim()) {
            if let Some(name) = v["name"].as_str() {
                if known(name) {
                    let Some(arguments) = v.get("arguments").filter(|value| value.is_object())
                    else {
                        rest = next;
                        continue;
                    };
                    let call = axocoatl_llm::ToolCall {
                        id: format!("call_{}", calls.len()),
                        name: name.to_string(),
                        arguments: arguments.clone(),
                        provider_metadata: provider_tool_metadata("ollama"),
                    };
                    push_recovered_tool_call(&mut calls, call)?;
                }
            }
        }
        rest = next;
    }

    // Shape 3: a bare JSON object, no wrapper at all —
    //   {"name":"NAME","arguments":{…}}
    // qwen2.5-coder emits exactly this. Ollama's template for it does not lift
    // the call into `tool_calls`, so without this the model appears to do
    // nothing: it answers in a few tokens, edits no files, and every check then
    // passes because there is nothing to check. The name gate below is what
    // keeps this safe — prose is only ever read as a call when it names a tool
    // that was actually offered.
    if calls.is_empty() {
        for candidate in bare_json_objects(content) {
            note_text_tool_candidate(&mut candidate_count)?;
            let Ok(v) = serde_json::from_str::<serde_json::Value>(candidate) else {
                continue;
            };
            // Accept `arguments` (OpenAI/Hermes) or `parameters` (some Qwen builds).
            let Some(name) = v["name"].as_str() else {
                continue;
            };
            if !known(name) {
                continue;
            }
            let args = v
                .get("arguments")
                .or_else(|| v.get("parameters"))
                .filter(|value| value.is_object());
            let Some(args) = args else {
                continue;
            };
            let call = axocoatl_llm::ToolCall {
                id: format!("call_{}", calls.len()),
                name: name.to_string(),
                arguments: args.clone(),
                provider_metadata: provider_tool_metadata("ollama"),
            };
            push_recovered_tool_call(&mut calls, call)?;
        }
    }

    Ok(calls)
}

/// Lazy balanced top-level `{…}` spans in `s`, so a JSON object embedded in
/// prose (or several of them) can each be tried without first materializing a
/// potentially huge vector of slices. Brace counting ignores braces inside
/// string literals, which is what makes nested argument objects work.
struct BareJsonObjects<'a> {
    source: &'a str,
    offset: usize,
    depth: usize,
    start: usize,
    in_string: bool,
    escaped: bool,
}

impl<'a> Iterator for BareJsonObjects<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        let bytes = self.source.as_bytes();
        while self.offset < bytes.len() {
            let index = self.offset;
            let byte = bytes[index];
            self.offset += 1;
            if self.in_string {
                match byte {
                    _ if self.escaped => self.escaped = false,
                    b'\\' => self.escaped = true,
                    b'"' => self.in_string = false,
                    _ => {}
                }
                continue;
            }
            match byte {
                b'"' => self.in_string = true,
                b'{' => {
                    if self.depth == 0 {
                        self.start = index;
                    }
                    self.depth = self.depth.saturating_add(1);
                }
                b'}' if self.depth > 0 => {
                    self.depth -= 1;
                    if self.depth == 0 {
                        return self.source.get(self.start..=index);
                    }
                }
                _ => {}
            }
        }
        None
    }
}

fn bare_json_objects(s: &str) -> BareJsonObjects<'_> {
    BareJsonObjects {
        source: s,
        offset: 0,
        depth: 0,
        start: 0,
        in_string: false,
        escaped: false,
    }
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

/// Ollama provider using its OpenAI-compatible chat completions endpoint.
/// Compatible implementations must honor the Ollama request/response contract.
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

    /// Create with a custom base URL (for remote Ollama or a compatible implementation).
    pub fn with_base_url(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: http_client(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
        }
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
            .timeout(RESPONSE_TIMEOUT)
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

        let content = choice["message"]["content"]
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

        // Fallback: some local models emit tool calls as text in `content`
        // (`<function=NAME>…`) instead of the structured `tool_calls` field.
        // Recover them so the call still executes — guarded to offered tools.
        if tool_calls.is_empty() {
            let tool_names: Vec<String> = request.tools.iter().map(|t| t.name.clone()).collect();
            tool_calls = parse_text_tool_calls(&content, &tool_names)?;
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

        let response = tokio::time::timeout(
            RESPONSE_TIMEOUT,
            self.client
                .post(self.endpoint()?)
                .header(CONTENT_TYPE, "application/json")
                .json(&body)
                .send(),
        )
        .await
        .map_err(|_| ProviderError::Network("Ollama response headers timed out".to_string()))?
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

        // Captured for the text-tool-call fallback in the finish branch below.
        let tool_names: Vec<String> = request.tools.iter().map(|t| t.name.clone()).collect();

        let stream = async_stream::try_stream! {
            let mut decoder = SseDecoder::provider_default();
            // Accumulated assistant text plus how much we've already streamed out.
            // Lets the finish branch recover a tool call a model emits as text while
            // keeping its raw markup off-screen.
            let mut content_acc = String::new();
            let mut flushed = 0usize;
            let mut in_text_tool_call = false;
            let mut saw_struct_tool_call = false;
            let mut saw_reasoning = false;
            let mut pending_finish = None;
            let mut saw_sentinel = false;
            let mut structured_tool_call_ids = std::collections::BTreeMap::<usize, String>::new();
            let mut recovered_tool_call_count = 0usize;
            let total_deadline = tokio::time::Instant::now() + STREAM_TOTAL_TIMEOUT;

            'response: loop {
                let next = next_stream_item(
                    &mut byte_stream,
                    total_deadline,
                    STREAM_IDLE_TIMEOUT,
                    "Ollama",
                )
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

                            // Finish reason
                            if let Some(reason) = choice["finish_reason"].as_str() {
                                // Recover a tool call emitted as text when the model
                                // never sent a structured one.
                                let recovered = if saw_struct_tool_call {
                                    Vec::new()
                                } else {
                                    parse_text_tool_calls(&content_acc, &tool_names)?
                                };

                                if !recovered.is_empty() {
                                    if !matches!(reason, "stop" | "tool_calls") {
                                        Err(ProviderError::Stream(
                                            "Ollama returned a recovered text tool call under an incompatible finish reason"
                                                .to_string(),
                                        ))?;
                                    }
                                    recovered_tool_call_count = recovered.len();
                                    for (i, call) in recovered.iter().enumerate() {
                                        yield StreamEvent::ToolCallDelta {
                                            index: Some(i),
                                            id: call.id.clone(),
                                            name: Some(call.name.clone()),
                                            args_delta: serde_json::to_string(&call.arguments)
                                                .unwrap_or_else(|_| "{}".to_string()),
                                        };
                                        yield StreamEvent::ToolCallMetadata {
                                            index: Some(i),
                                            id: call.id.clone(),
                                            metadata: call.provider_metadata.clone(),
                                        };
                                    }
                                    pending_finish = Some(FinishReason::ToolUse);
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
                                        "content_filter" => FinishReason::ContentFilter,
                                        other => Err(ProviderError::Stream(format!(
                                            "Ollama returned unsupported finish reason {other}"
                                        )))?,
                                    };
                                    pending_finish = Some(finish);
                                }
                            }
                    }
                }

                if reached_eof {
                    break;
                }
            }

            let finish_reason = pending_finish.ok_or_else(|| {
                let terminal = if saw_sentinel { "terminal sentinel" } else { "connection close" };
                ProviderError::Stream(format!("Ollama stream reached {terminal} without a finish reason"))
            })?;
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
                && content_acc.trim().is_empty()
                && !saw_struct_tool_call
                && recovered_tool_call_count == 0
            {
                Err(ProviderError::Stream(
                    reasoning_only_terminal_message(&finish_reason).to_string(),
                ))?;
            }
            yield StreamEvent::Done { finish_reason };
        };

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod bare_json_tool_call_tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["read_file".to_string(), "write_file".to_string()]
    }

    #[test]
    fn recovers_a_bare_json_call() {
        // Exactly what qwen2.5-coder emits, and what was being discarded.
        let calls = parse_text_tool_calls(
            r#"{"name": "read_file", "arguments": {"path": "lib/orders.ts"}}"#,
            &names(),
        )
        .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], "lib/orders.ts");
    }

    #[test]
    fn recovers_a_call_embedded_in_prose() {
        let calls = parse_text_tool_calls(
            "Sure, I'll read it.\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.ts\"}}\nDone.",
            &names(),
        )
        .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments["path"], "a.ts");
    }

    #[test]
    fn accepts_parameters_as_an_alias_for_arguments() {
        let calls = parse_text_tool_calls(
            r#"{"name":"read_file","parameters":{"path":"b.ts"}}"#,
            &names(),
        )
        .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments["path"], "b.ts");
    }

    #[test]
    fn nested_objects_and_braces_in_strings_survive() {
        let calls = parse_text_tool_calls(
            r#"{"name":"write_file","arguments":{"path":"x.rs","content":"fn main() { let s = \"}\"; }"}}"#,
            &names(),
        )
        .unwrap();
        assert_eq!(
            calls.len(),
            1,
            "a brace inside a string must not end the object"
        );
        assert_eq!(calls[0].arguments["path"], "x.rs");
    }

    #[test]
    fn prose_is_never_mistaken_for_a_call() {
        // The name gate is what makes this safe.
        assert!(parse_text_tool_calls(
            r#"You could use {"name": "some_other_tool", "arguments": {}} here."#,
            &names(),
        )
        .unwrap()
        .is_empty());
        assert!(parse_text_tool_calls("{ just an object }", &names())
            .unwrap()
            .is_empty());
        assert!(parse_text_tool_calls("no json at all", &names())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn structured_shapes_still_win_and_bare_does_not_double_count() {
        // A wrapped call is parsed by shape 2; the bare pass must not add a
        // duplicate for the same JSON.
        let calls = parse_text_tool_calls(
            r#"<tool_call>{"name":"read_file","arguments":{"path":"c.ts"}}</tool_call>"#,
            &names(),
        )
        .unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn text_recovery_never_makes_missing_null_or_scalar_arguments_actionable() {
        for payload in [
            r#"{"name":"read_file"}"#,
            r#"{"name":"read_file","arguments":null}"#,
            r#"{"name":"read_file","arguments":"/secret"}"#,
            r#"<tool_call>{"name":"read_file"}</tool_call>"#,
            r#"<tool_call>{"name":"read_file","arguments":7}</tool_call>"#,
        ] {
            assert!(
                parse_text_tool_calls(payload, &names()).unwrap().is_empty(),
                "invalid arguments unexpectedly became a call: {payload}"
            );
        }
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

    #[test]
    fn text_tool_recovery_bounds_qualifying_calls_and_candidate_scans() {
        let flood = recovered_tool_call_flood(MAX_RECOVERED_TOOL_CALLS + 1);
        assert!(matches!(
            parse_text_tool_calls(&flood, &["lookup".to_string()]),
            Err(ProviderError::ApiError { message, .. })
                if message.contains("more than 128")
        ));

        let candidate_flood = "{}".repeat(MAX_TEXT_TOOL_CANDIDATES + 1);
        assert!(matches!(
            parse_text_tool_calls(&candidate_flood, &["lookup".to_string()]),
            Err(ProviderError::ApiError { message, .. })
                if message.contains("bounded candidate scan limit")
        ));
    }

    #[tokio::test]
    async fn nonstream_recovered_tool_flood_fails_before_returning_actionable_calls() {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let response = serde_json::json!({
            "choices": [{
                "index": 0,
                "message": {
                    "content": recovered_tool_call_flood(MAX_RECOVERED_TOOL_CALLS + 1)
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
            Err(ProviderError::ApiError { message, .. })
                if message.contains("more than 128")
        ));
    }

    #[tokio::test]
    async fn stream_recovered_tool_flood_emits_zero_actionable_events() {
        use tokio_stream::StreamExt;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let frame = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "content": recovered_tool_call_flood(MAX_RECOVERED_TOOL_CALLS + 1)
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
            Err(ProviderError::ApiError { message, .. })
                if message.contains("more than 128")
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
        assert!(events.iter().any(|event| matches!(
            event,
            Err(ProviderError::Stream(message))
                if message.contains("incompatible finish reason")
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            Ok(StreamEvent::ToolCallDelta { .. } | StreamEvent::Done { .. })
        )));
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
            ("not_declared", "{}"),
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
        let calls = parse_text_tool_calls(content, &names).unwrap();
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
        let calls = parse_text_tool_calls(content, &names).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "write_file");
        assert_eq!(calls[0].arguments["path"], "a.txt");
        assert_eq!(calls[0].arguments["content"], "hi");
    }

    #[test]
    fn ignores_function_names_not_offered() {
        let content = "<function=rm_rf>\n<parameter=path>/</parameter>\n</function>";
        let names = vec!["edit_file".to_string()];
        assert!(parse_text_tool_calls(content, &names).unwrap().is_empty());
    }

    #[test]
    fn prose_mentioning_a_marker_is_not_a_call() {
        // Offered tool name appears in prose, but with no complete block.
        let content = "Use <function=edit_file> when you need to change a file.";
        let names = vec!["edit_file".to_string()];
        assert!(parse_text_tool_calls(content, &names).unwrap().is_empty());
    }

    #[test]
    fn no_markers_yields_no_calls() {
        let content = "Just a normal assistant reply with no tool calls at all.";
        let names = vec!["edit_file".to_string()];
        assert!(parse_text_tool_calls(content, &names).unwrap().is_empty());
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
