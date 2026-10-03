//! Explicit local Ollama execution under the shared provider/controller contract.
//!
//! The audited 0.20.6 GGUF runners reject oversized inputs with `truncate:false`
//! and stop at positive `num_predict`. Thinking+structured output can run twice,
//! so format requests reserve both full passes. Reported final metrics cover
//! only the last pass and are consequently lower bounds for format requests.
//! This is an operator-configured server capability, not model-weight attestation.
//! The existing OpenAI-compatible provider is intentionally unchanged.

use std::{
    collections::{HashMap, HashSet},
    io::Write,
    pin::Pin,
};

use axocoatl_core::{
    ChatMessage, ContentPart, ImageDetail, MeasuredTokenUsage, MessageContent, MessageRole,
    ResponseFormat, TokenUsageStats,
};
use axocoatl_llm::{
    provider_tool_metadata,
    transport::{
        network_error, next_stream_item, validated_endpoint, RESPONSE_TIMEOUT, STREAM_IDLE_TIMEOUT,
        STREAM_TOTAL_TIMEOUT,
    },
    validate_chat_response, validate_provider_request, validate_required_stream_tool_call_ids,
    validate_required_tool_call_id, validate_response_tool_call, AccountedChatOutcome, ChatRequest,
    ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    ProviderExecutionBounds, StreamEvent, ToolCall, TOOL_METADATA_PROVIDER_ID,
    TOOL_METADATA_ROUTE_MODEL, TOOL_METADATA_ROUTE_PROVIDER, TOOL_METADATA_ROUTE_SLOT,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Serialize;
use serde_json::{json, Value};
use tokio_stream::{Stream, StreamExt};

mod observed_context;
mod server_check;
pub use observed_context::{observe_native_ollama_context, NativeOllamaContextObservation};
use server_check::reports_cloud_disabled;
pub use server_check::{
    check_native_ollama_server, validate_native_ollama_endpoint, NativeOllamaServerCheck,
    OllamaCloudMode, NATIVE_OLLAMA_SERVER_VERSION,
};

const PROVIDER: &str = "ollama";
const VERSION: &str = "0.20.6";
const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
// Reserved even when normal payload space is exhausted: one newly observed
// cumulative count snapshot, final accounting, and terminal disposition.
const ACCOUNTING_RESERVE: usize = 2048;
const MAX_MESSAGES: usize = 4096;
const MAX_RECORDS: usize = 65_536;
const THINKING_METADATA: &str = "ollama.native.thinking";

/// Explicit finite limits for an operator-configured, cloud-disabled GGUF server.
#[derive(Debug, Clone)]
pub struct NativeOllamaConfig {
    pub base_url: String,
    pub model: String,
    pub context_tokens: usize,
    pub max_output_tokens: usize,
    pub max_response_bytes: usize,
}

/// Constructed only after real server version/status/model verification.
/// Every inference rechecks the same requirements before posting input.
pub struct NativeOllamaProvider {
    client: reqwest::Client,
    config: NativeOllamaConfig,
    tools_supported: bool,
    thinking_supported: bool,
    vision_supported: bool,
    observed_context: Option<NativeOllamaContextObservation>,
}

fn invalid(message: impl Into<String>) -> ProviderError {
    ProviderError::InvalidRequest {
        provider: PROVIDER.into(),
        message: message.into(),
    }
}
fn protocol(message: impl Into<String>) -> ProviderError {
    ProviderError::Stream(format!("native Ollama: {}", message.into()))
}
/// Ollama closed the response without its `done` record. Tool calls are only
/// released at `done`, so nothing from this response ran.
fn incomplete_stream(message: impl Into<String>) -> ProviderError {
    ProviderError::IncompleteStream {
        provider: PROVIDER.into(),
        message: message.into(),
    }
}

// A counting serializer bounds caller-owned input before constructing cloned
// wire values. The same helper accounts escaped output without a second buffer.
struct LimitWriter {
    remaining: usize,
    written: usize,
}
impl Write for LimitWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(std::io::Error::other(
                "encoded provider data exceeds its bound",
            ));
        }
        self.remaining -= bytes.len();
        self.written += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encoded_size(value: &impl Serialize, limit: usize) -> Result<usize, ProviderError> {
    let mut writer = LimitWriter {
        remaining: limit,
        written: 0,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| protocol("encoded byte limit exceeded"))?;
    Ok(writer.written)
}

/// Refuse an endpoint native execution cannot use: not plain HTTP(S), carrying
/// credential material, or not a loopback address.
fn require_loopback(base_url: &str) -> Result<(), ProviderError> {
    validated_endpoint(base_url, "api/chat", PROVIDER)?;
    let url = reqwest::Url::parse(base_url).map_err(|_| invalid("invalid local endpoint"))?;
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']');
    if host != "localhost"
        && !host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    {
        return Err(invalid(
            "zero-API-spend native execution requires a loopback Ollama endpoint",
        ));
    }
    Ok(())
}

fn local_client(base_url: &str) -> Result<reqwest::Client, ProviderError> {
    require_loopback(base_url)?;
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(15))
        .resolve_to_addrs(
            "localhost",
            &[
                std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
                std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, 0)),
            ],
        )
        .build()
        .map_err(|error| network_error(&error, &[]))
}

async fn metadata_request(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value, ProviderError> {
    let url = validated_endpoint(base_url, path, PROVIDER)?;
    let builder = match body {
        Some(body) => client.post(url).json(&body),
        None => client.get(url),
    };
    let mut response = builder
        .timeout(RESPONSE_TIMEOUT)
        .send()
        .await
        .map_err(|error| network_error(&error, &[]))?;
    if !response.status().is_success() {
        return Err(invalid("local profile metadata request failed"));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
    {
        return Err(invalid("local profile metadata exceeds its byte limit"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| network_error(&error, &[]))?
    {
        if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
            return Err(invalid("local profile metadata exceeds its byte limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid("malformed local profile metadata"))
}

impl NativeOllamaProvider {
    pub async fn connect(config: NativeOllamaConfig) -> Result<Self, ProviderError> {
        validated_endpoint(&config.base_url, "api/chat", PROVIDER)?;
        if config.model.is_empty()
            || config.model.len() > 256
            || config.model.chars().any(char::is_control)
            || config.context_tokens < 2048
            || config.context_tokens > 16 * 1024 * 1024
            || config.max_output_tokens == 0
            || config.max_output_tokens > config.context_tokens.saturating_mul(10)
            || config.max_response_bytes < ACCOUNTING_RESERVE
            || config.max_response_bytes > MAX_RESPONSE_BYTES
        {
            return Err(invalid("invalid finite native Ollama execution profile"));
        }
        let client = local_client(&config.base_url)?;
        let mut provider = Self {
            client,
            config,
            tools_supported: false,
            thinking_supported: false,
            vision_supported: false,
            observed_context: None,
        };
        let (tools, thinking, vision) = provider.verify().await?;
        provider.tools_supported = tools;
        provider.thinking_supported = thinking;
        provider.vision_supported = vision;
        Ok(provider)
    }

    async fn metadata(&self, path: &str, body: Option<Value>) -> Result<Value, ProviderError> {
        metadata_request(&self.client, &self.config.base_url, path, body).await
    }

    async fn verify(&self) -> Result<(bool, bool, bool), ProviderError> {
        let version = self.metadata("api/version", None).await?;
        if version.get("version").and_then(Value::as_str) != Some(VERSION) {
            return Err(invalid(
                "native execution requires the audited Ollama 0.20.6 server",
            ));
        }
        let status = self.metadata("api/status", None).await?;
        if !reports_cloud_disabled(&status) {
            return Err(invalid(
                "native execution requires server-reported cloud-disabled mode",
            ));
        }
        let model = self
            .metadata("api/show", Some(json!({"model": self.config.model})))
            .await?;
        let remote = ["remote_host", "remote_model"].iter().any(|key| {
            model
                .get(key)
                .is_some_and(|value| value.as_str() != Some(""))
        });
        let caps = model
            .get("capabilities")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("model capabilities were not reported"))?;
        if remote
            || model.pointer("/details/format").and_then(Value::as_str) != Some("gguf")
            || !caps.iter().any(|v| v.as_str() == Some("completion"))
            || caps.iter().any(|v| v.as_str() == Some("image"))
        {
            return Err(invalid(
                "native execution requires a local GGUF completion model",
            ));
        }
        Ok((
            caps.iter().any(|v| v.as_str() == Some("tools")),
            caps.iter().any(|v| v.as_str() == Some("thinking")),
            caps.iter().any(|v| v.as_str() == Some("vision")),
        ))
    }

    fn prediction_limit(&self, request: &ChatRequest) -> usize {
        request.max_tokens.unwrap_or(self.config.max_output_tokens)
    }

    fn wire_request(&self, request: &ChatRequest) -> Result<Value, ProviderError> {
        self.validate_request(request)?;
        let messages = native_messages(&request.messages)?;
        let mut options = json!({"num_ctx": self.config.context_tokens,
            "num_predict": self.prediction_limit(request), "stop": request.stop_sequences});
        if let Some(value) = request.temperature {
            options["temperature"] = json!(value);
        }
        if let Some(value) = request.top_p {
            options["top_p"] = json!(value);
        }
        let mut body = json!({"model": self.config.model, "messages": messages,
            "options": options, "stream": true, "truncate": false, "shift": false,
            "tools": super::tools_json(&request.tools)});
        if request.response_format == Some(ResponseFormat::Json) {
            body["format"] = json!("json");
        }
        if request
            .provider_options
            .as_ref()
            .and_then(|v| v.get("reasoning_effort"))
            .and_then(Value::as_str)
            == Some("none")
        {
            body["think"] = json!(false);
        }
        encoded_size(&body, MAX_REQUEST_BYTES)?;
        Ok(body)
    }

    async fn native_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        let body = self.wire_request(&request)?;
        let (tools, thinking, vision) = self.verify().await?;
        if tools != self.tools_supported
            || thinking != self.thinking_supported
            || vision != self.vision_supported
        {
            return Err(invalid(
                "local model capabilities changed; reconnect the execution profile",
            ));
        }
        if let Some(observation) = &self.observed_context {
            observed_context::verify_identity(self, observation).await?;
        }
        let url = validated_endpoint(&self.config.base_url, "api/chat", PROVIDER)?;
        let response =
            tokio::time::timeout(RESPONSE_TIMEOUT, self.client.post(url).json(&body).send())
                .await
                .map_err(|_| protocol("request header timeout"))?
                .map_err(|error| network_error(&error, &[]))?;
        let status = response.status().as_u16();
        let cap = self.config.max_response_bytes;
        let too_long = response.content_length().is_some_and(|n| n > cap as u64);
        let mut bytes = response.bytes_stream();
        let token_limit = self
            .execution_bounds(&request)
            .ok_or_else(|| invalid("request has no native bound"))?
            .token_limit;
        let mut state = NativeResponse::new(self.config.model.clone(), request, cap, token_limit);
        let stream = async_stream::stream! {
            if too_long { yield Err(protocol("response exceeds its byte limit")); return; }
            let deadline = tokio::time::Instant::now() + STREAM_TOTAL_TIMEOUT;
            let mut pending = Vec::new();
            let mut received = 0usize;
            let mut records = 0usize;
            loop {
                let next = match next_stream_item(&mut bytes, deadline, STREAM_IDLE_TIMEOUT, PROVIDER).await {
                    Ok(value) => value,
                    Err(error) => { yield Err(error); return; }
                };
                let eof = next.is_none();
                if let Some(chunk) = next {
                    let chunk = match chunk { Ok(chunk) => chunk, Err(error) => { yield Err(network_error(&error, &[])); return; } };
                    if chunk.len() > cap.saturating_sub(received) {
                        yield Err(protocol("response exceeds its byte limit")); return;
                    }
                    received += chunk.len();
                    pending.extend_from_slice(&chunk);
                }
                // Process complete records immediately; never delay text until the
                // full response arrives. The last record need not end in a newline.
                while let Some(end) = pending.iter().position(|b| *b == b'\n').or_else(|| (eof && !pending.is_empty()).then_some(pending.len())) {
                    let line: Vec<_> = pending.drain(..end).collect();
                    if pending.first() == Some(&b'\n') { pending.remove(0); }
                    if line.iter().all(u8::is_ascii_whitespace) { continue; }
                    records += 1;
                    if records > MAX_RECORDS { yield Err(protocol("too many response records")); return; }
                    let value: Value = match serde_json::from_slice(&line) {
                        Ok(value) => value,
                        Err(_) => { yield Err(protocol("malformed NDJSON record")); return; }
                    };
                    // Even an error record may contain observed, incurred usage.
                    let observed = state.observe_counts(&value);
                    if let Some(usage) = observed {
                        let event = StreamEvent::UsageObservation(usage);
                        if let Err(error) = state.charge(&event) { yield Err(error); return; }
                        yield Ok(event);
                        if state.emitted > state.cap.saturating_sub(ACCOUNTING_RESERVE) {
                            yield Err(protocol("payload exhausted its reserved accounting margin")); return;
                        }
                    }
                    let events = match state.record(&value, status) {
                        Ok(events) => events,
                        Err(error) => { yield Err(error); return; }
                    };
                    for event in events {
                        if let Err(error) = state.charge(&event) { yield Err(error); return; }
                        yield Ok(event);
                    }
                }
                if eof { break; }
            }
            // A refused request with no readable body is a rejection, not a
            // response that ended early, so it is never retried.
            if !(200..300).contains(&status) {
                yield Err(ProviderError::ApiError {
                    provider: PROVIDER.into(),
                    status,
                    message: "native Ollama rejected or failed the bounded inference request".into(),
                });
                return;
            }
            if let Some((usage, error)) = state.take_rejection() {
                if let Some(event) = usage {
                    if state.charge(&event).is_ok() { yield Ok(event); }
                }
                yield Err(error);
                return;
            }
            let events = match state.finish() { Ok(events) => events, Err(error) => { yield Err(error); return; } };
            for event in events {
                if let Err(error) = state.charge(&event) { yield Err(error); return; }
                yield Ok(event);
            }
        };
        Ok(Box::pin(stream))
    }
}

#[async_trait::async_trait]
impl LlmProvider for NativeOllamaProvider {
    fn provider_id(&self) -> &str {
        PROVIDER
    }
    fn model_id(&self) -> &str {
        &self.config.model
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: self.tools_supported,
            structured_output: true,
            reasoning: self.thinking_supported,
            vision: self.vision_supported,
            max_context_tokens: self.config.context_tokens,
            max_output_tokens: self.config.max_output_tokens,
            ..ProviderCapabilities::default()
        }
    }
    fn validate_request(&self, request: &ChatRequest) -> Result<(), ProviderError> {
        validate_provider_request(request, PROVIDER)?;
        if request
            .model_override
            .as_ref()
            .is_some_and(|model| model != &self.config.model)
            || self.prediction_limit(request) > self.config.max_output_tokens
            || request.messages.is_empty()
            || request.messages.len() > MAX_MESSAGES
            || (!request.tools.is_empty() && !self.tools_supported)
        {
            return Err(invalid("request exceeds the verified native profile"));
        }
        if let Some(options) = &request.provider_options {
            let valid = options.as_object().is_some_and(|options| {
                options.is_empty()
                    || (options.len() == 1
                        && options.get("reasoning_effort").and_then(Value::as_str) == Some("none"))
            });
            if !valid {
                return Err(invalid("unsupported native Ollama request options"));
            }
        }
        encoded_size(
            &(
                &request.messages,
                &request.tools,
                &request.provider_options,
                &request.stop_sequences,
            ),
            MAX_REQUEST_BYTES,
        )?;
        for message in &request.messages {
            if let MessageContent::Parts(parts) = &message.content {
                if parts
                    .iter()
                    .any(|part| matches!(part, ContentPart::Image { .. }))
                    && !self.vision_supported
                {
                    return Err(invalid(
                        "the selected local model does not report vision input support",
                    ));
                }
            }
            for call in &message.tool_calls {
                if call
                    .provider_metadata
                    .get(TOOL_METADATA_ROUTE_MODEL)
                    .is_some_and(|model| model != &self.config.model)
                {
                    return Err(invalid(
                        "native history belongs to a different configured model",
                    ));
                }
            }
        }
        validate_history(&request.messages)
    }
    fn execution_bounds(&self, request: &ChatRequest) -> Option<ProviderExecutionBounds> {
        self.validate_request(request).ok()?;
        let passes = if request.response_format == Some(ResponseFormat::Json) {
            2u64
        } else {
            1
        };
        Some(ProviderExecutionBounds {
            token_limit: (self.config.context_tokens as u64)
                .checked_add(self.prediction_limit(request) as u64)?
                .checked_mul(passes)?,
            cost_microunits: 0,
            response_bytes: self.config.max_response_bytes,
        })
    }
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        self.chat_with_accounting(request).await.response
    }
    async fn chat_with_accounting(&self, request: ChatRequest) -> AccountedChatOutcome {
        let mut observed = MeasuredTokenUsage::lower_bound(TokenUsageStats::default());
        let mut stream = match self.native_stream(request).await {
            Ok(stream) => stream,
            Err(error) => {
                return AccountedChatOutcome {
                    cost_microunits: None,
                    response: Err(error),
                    usage: observed,
                }
            }
        };
        let mut content = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::TextDelta { delta }) => content.push_str(&delta),
                Ok(StreamEvent::ToolCallDelta {
                    id,
                    name: Some(name),
                    args_delta,
                    ..
                }) => {
                    let arguments = match serde_json::from_str(&args_delta) {
                        Ok(value) => value,
                        Err(_) => {
                            return AccountedChatOutcome {
                                cost_microunits: None,
                                response: Err(protocol("invalid normalized tool arguments")),
                                usage: incomplete(observed),
                            }
                        }
                    };
                    calls.push(ToolCall {
                        id,
                        name,
                        arguments,
                        provider_metadata: provider_tool_metadata(PROVIDER),
                    });
                }
                Ok(StreamEvent::ToolCallMetadata { id, metadata, .. }) => {
                    if let Some(call) = calls.iter_mut().find(|call| call.id == id) {
                        call.provider_metadata.extend(metadata);
                    }
                }
                Ok(StreamEvent::UsageObservation(usage)) => observed = usage,
                Ok(StreamEvent::Done { finish_reason }) => {
                    let response = ChatResponse {
                        content,
                        tool_calls: calls,
                        finish_reason,
                        usage: observed.usage.clone(),
                        model: self.config.model.clone(),
                        provider: PROVIDER.into(),
                    };
                    if let Err(error) = validate_chat_response(PROVIDER, &response) {
                        return AccountedChatOutcome {
                            cost_microunits: None,
                            response: Err(error),
                            usage: incomplete(observed),
                        };
                    }
                    return AccountedChatOutcome {
                        cost_microunits: None,
                        response: Ok(response),
                        usage: observed,
                    };
                }
                Err(error) => {
                    return AccountedChatOutcome {
                        cost_microunits: None,
                        response: Err(error),
                        usage: incomplete(observed),
                    }
                }
                _ => {}
            }
        }
        AccountedChatOutcome {
            cost_microunits: None,
            response: Err(incomplete_stream("stream ended without completion")),
            usage: incomplete(observed),
        }
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        self.native_stream(request).await
    }
}

fn incomplete(mut usage: MeasuredTokenUsage) -> MeasuredTokenUsage {
    usage.complete = false;
    usage
}

fn validate_history(messages: &[ChatMessage]) -> Result<(), ProviderError> {
    let mut pending: HashMap<&str, &str> = HashMap::new();
    let mut seen = HashSet::new();
    for message in messages {
        validate_message_content(message)?;
        if !matches!(message.role, MessageRole::Tool)
            && (message.name.is_some() || message.tool_call_id.is_some())
        {
            return Err(invalid("unsupported participant or tool-result fields"));
        }
        if !matches!(message.role, MessageRole::Assistant) && !message.tool_calls.is_empty() {
            return Err(invalid("tool calls must belong to an assistant message"));
        }
        if !matches!(message.role, MessageRole::Tool) && !pending.is_empty() {
            return Err(invalid("native tool history has unresolved calls"));
        }
        let mut thinking: Option<&str> = None;
        for call in &message.tool_calls {
            if message.tool_calls.len() > 128
                || call.id.len() > 256
                || call.name.len() > 64
                || call.id.chars().any(char::is_control)
                || call.name.is_empty()
                || !call.arguments.is_object()
                || !seen.insert(call.id.as_str())
            {
                return Err(invalid("invalid or duplicated native history tool call"));
            }
            validate_required_tool_call_id(PROVIDER, &call.id)?;
            for (key, value) in &call.provider_metadata {
                match key.as_str() {
                    TOOL_METADATA_PROVIDER_ID | TOOL_METADATA_ROUTE_PROVIDER
                        if value == PROVIDER => {}
                    TOOL_METADATA_ROUTE_SLOT | TOOL_METADATA_ROUTE_MODEL => {}
                    THINKING_METADATA => {
                        if thinking.is_some_and(|previous| previous != value) {
                            return Err(invalid("conflicting native thinking history"));
                        }
                        thinking = Some(value);
                    }
                    _ => {
                        return Err(invalid(
                            "unsupported provider metadata in native tool history",
                        ))
                    }
                }
            }
            pending.insert(&call.id, &call.name);
        }
        if matches!(message.role, MessageRole::Tool) {
            let id = message
                .tool_call_id
                .as_deref()
                .ok_or_else(|| invalid("tool result omitted its exact call id"))?;
            let name = pending
                .remove(id)
                .ok_or_else(|| invalid("tool result has no unique pending native call"))?;
            if message.name.as_deref().is_some_and(|actual| actual != name) {
                return Err(invalid("tool result name differs from its native call"));
            }
        }
    }
    if !pending.is_empty() {
        return Err(invalid("native tool history has unresolved calls"));
    }
    Ok(())
}

// The audited runners expand multimodal inputs before their num_ctx check:
// runner/{ollamarunner,llamarunner}/runner.go, NewSequence and inputs (v0.20.6).
// truncate:false rejects the expanded prompt; num_predict bounds generation.
// Images therefore consume the existing full context reservation, never an
// estimated image-token allowance or an additional unbounded provider route.
fn inline_image<'a>(url: &'a str, detail: &ImageDetail) -> Result<&'a str, ProviderError> {
    const MAX_IMAGE_BYTES: usize = 25 * 1024 * 1024;
    if !matches!(detail, ImageDetail::Auto) {
        return Err(invalid(
            "native Ollama does not support image detail overrides",
        ));
    }
    let (header, body) = url
        .split_once(";base64,")
        .ok_or_else(|| invalid("native images require retained inline base64 bytes"))?;
    let mime = header
        .strip_prefix("data:")
        .ok_or_else(|| invalid("native images cannot fetch external URLs"))?;
    if !mime
        .get(..6)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("image/"))
        || mime.len() > 256
        || mime.len() == 6
        || mime.bytes().any(|byte| {
            byte.is_ascii_whitespace() || byte.is_ascii_control() || matches!(byte, b';' | b',')
        })
        || body.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4
        || body.is_empty()
    {
        return Err(invalid(
            "native image type or encoded byte limit is invalid",
        ));
    }
    let bytes = STANDARD
        .decode(body)
        .map_err(|_| invalid("native image encoding is invalid"))?;
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err(invalid("native image exceeds its byte limit"));
    }
    Ok(body)
}

fn validate_message_content(message: &ChatMessage) -> Result<(), ProviderError> {
    if let MessageContent::Parts(parts) = &message.content {
        if parts.is_empty() || parts.len() > MAX_MESSAGES {
            return Err(invalid("native content parts exceed their limit"));
        }
        for part in parts {
            if let ContentPart::Image { url, detail } = part {
                if !matches!(message.role, MessageRole::User) {
                    return Err(invalid("native image inputs must belong to a user message"));
                }
                inline_image(url, detail)?;
            }
        }
    }
    Ok(())
}

fn native_content(message: &ChatMessage) -> Result<(String, Vec<&str>), ProviderError> {
    match &message.content {
        MessageContent::Text(text) => Ok((text.clone(), Vec::new())),
        MessageContent::Parts(parts) => {
            let mut text = String::new();
            let mut images = Vec::new();
            for part in parts {
                match part {
                    ContentPart::Text(value) => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(value);
                    }
                    ContentPart::Image { url, detail } => images.push(inline_image(url, detail)?),
                }
            }
            Ok((text, images))
        }
    }
}

fn native_messages(messages: &[ChatMessage]) -> Result<Vec<Value>, ProviderError> {
    let mut names = HashMap::new();
    let mut values = Vec::with_capacity(messages.len());
    for message in messages {
        let role = match message.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        let (content, images) = native_content(message)?;
        let mut value = json!({"role": role, "content": content});
        if !images.is_empty() {
            value["images"] = json!(images);
        }
        if !message.tool_calls.is_empty() {
            value["tool_calls"] = Value::Array(message.tool_calls.iter().enumerate().map(|(index, call)| {
                names.insert(call.id.as_str(), call.name.as_str());
                json!({"id": call.id, "function": {"index": index, "name": call.name, "arguments": call.arguments}})
            }).collect());
            if let Some(thinking) = message
                .tool_calls
                .iter()
                .find_map(|call| call.provider_metadata.get(THINKING_METADATA))
            {
                value["thinking"] = json!(thinking);
            }
        }
        if let Some(id) = &message.tool_call_id {
            value["tool_call_id"] = json!(id);
            value["tool_name"] = json!(names
                .get(id.as_str())
                .ok_or_else(|| invalid("tool result has no native name"))?);
        }
        values.push(value);
    }
    Ok(values)
}

struct NativeResponse {
    model: String,
    request: ChatRequest,
    cap: usize,
    emitted: usize,
    usage: MeasuredTokenUsage,
    terminal_counts: bool,
    counts_conflict: bool,
    token_limit: u64,
    terminal: Option<FinishReason>,
    content: String,
    thinking: String,
    calls: Vec<ToolCall>,
    /// Why the model's tool calls were refused. The response is still read to
    /// its `done` record so the usage it incurred is observed and settled.
    rejected: Option<ProviderError>,
}
impl NativeResponse {
    fn new(model: String, request: ChatRequest, cap: usize, token_limit: u64) -> Self {
        Self {
            model,
            request,
            cap,
            emitted: 0,
            usage: MeasuredTokenUsage::lower_bound(TokenUsageStats::default()),
            terminal_counts: false,
            counts_conflict: false,
            token_limit,
            terminal: None,
            content: String::new(),
            thinking: String::new(),
            calls: Vec::new(),
            rejected: None,
        }
    }
    fn observe_counts(&mut self, value: &Value) -> Option<MeasuredTokenUsage> {
        let input = value
            .get("prompt_eval_count")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok());
        let output = value
            .get("eval_count")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok());
        if input.is_none() && output.is_none() {
            return None;
        }
        if self.request.response_format != Some(ResponseFormat::Json)
            && (input.is_some_and(|n| n < self.usage.usage.input_tokens)
                || output.is_some_and(|n| n < self.usage.usage.output_tokens))
        {
            self.counts_conflict = true;
        }
        // Counters may restart for the hidden structured-output pass. Retain a
        // monotonic lower bound; never add snapshots as if they were deltas.
        self.usage.usage.input_tokens = self.usage.usage.input_tokens.max(input.unwrap_or(0));
        self.usage.usage.output_tokens = self.usage.usage.output_tokens.max(output.unwrap_or(0));
        if value.get("done").and_then(Value::as_bool) == Some(true) {
            self.terminal_counts = input.is_some() && output.is_some();
        }
        Some(self.usage.clone())
    }
    fn charge(&mut self, event: &StreamEvent) -> Result<(), ProviderError> {
        // 256 covers the normalized event tag/field names/index and leaves a
        // conservative margin over the shared controller's event serializer.
        let ceiling = if matches!(
            event,
            StreamEvent::UsageObservation(_) | StreamEvent::Done { .. }
        ) {
            self.cap
        } else {
            self.cap.saturating_sub(ACCOUNTING_RESERVE)
        };
        let remaining = ceiling.saturating_sub(self.emitted);
        let size = match event {
            StreamEvent::TextDelta { delta } | StreamEvent::ReasoningDelta { delta } => {
                encoded_size(delta, remaining)?
            }
            StreamEvent::ToolCallDelta {
                id,
                name,
                args_delta,
                index,
            } => encoded_size(&(id, name, args_delta, index), remaining)?,
            StreamEvent::ToolCallMetadata {
                id,
                metadata,
                index,
            } => encoded_size(&(id, metadata, index), remaining)?,
            StreamEvent::UsageObservation(usage) => encoded_size(usage, remaining)?,
            StreamEvent::Done { finish_reason } => encoded_size(finish_reason, remaining)?,
            _ => return Err(protocol("unexpected normalized event")),
        }
        .checked_add(256)
        .ok_or_else(|| protocol("event size overflow"))?;
        if size > remaining {
            return Err(protocol("normalized response exceeds its byte limit"));
        }
        self.emitted += size;
        Ok(())
    }
    /// Validate one record's tool calls as a whole. Nothing from a record
    /// with a malformed call is released.
    fn accept_calls(&self, calls: &Value) -> Result<Vec<ToolCall>, ProviderError> {
        let calls = calls
            .as_array()
            .ok_or_else(|| protocol("native tool calls are not an array"))?;
        if calls.len() > 128usize.saturating_sub(self.calls.len()) {
            return Err(protocol("too many native tool calls"));
        }
        let mut accepted: Vec<ToolCall> = Vec::with_capacity(calls.len());
        for call in calls {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| protocol("native call omitted its id"))?;
            let function = call
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| protocol("native call omitted its function"))?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| protocol("native call omitted its name"))?;
            let arguments = function
                .get("arguments")
                .ok_or_else(|| protocol("native call omitted its arguments"))?;
            validate_required_tool_call_id(PROVIDER, id)?;
            if id.len() > 256
                || id.chars().any(char::is_control)
                || self.calls.iter().chain(&accepted).any(|call| call.id == id)
            {
                return Err(protocol("invalid or duplicate native tool id"));
            }
            validate_response_tool_call(PROVIDER, name, arguments, &self.request.tools)?;
            accepted.push(ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: arguments.clone(),
                provider_metadata: provider_tool_metadata(PROVIDER),
            });
        }
        Ok(accepted)
    }
    /// A refused response's error. When the server finished the response,
    /// its terminal usage comes first and the error says the completed
    /// response was refused, so that usage is settled rather than lost.
    fn take_rejection(&mut self) -> Option<(Option<StreamEvent>, ProviderError)> {
        let error = self.rejected.take()?;
        if self.terminal.is_none() {
            return Some((None, error));
        }
        let mut usage = self.usage.clone();
        usage.complete =
            self.terminal_counts && self.request.response_format != Some(ResponseFormat::Json);
        let error = ProviderError::RefusedResponse {
            provider: PROVIDER.into(),
            message: error.to_string(),
        };
        Some((Some(StreamEvent::UsageObservation(usage)), error))
    }
    fn record(&mut self, value: &Value, status: u16) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.terminal.is_some() {
            return Err(protocol("record after native terminal"));
        }
        // An error record inside an accepted response means generation started
        // and then failed (for example the model's tool-call text could not be
        // parsed) before the `done` record, so nothing it proposed was
        // released: the same situation as a stream that simply ends early.
        if (200..300).contains(&status) {
            if let Some(failure) = value.get("error") {
                let detail: String = failure
                    .as_str()
                    .unwrap_or("generation error")
                    .chars()
                    .take(200)
                    .collect();
                return Err(incomplete_stream(format!(
                    "native Ollama ended the response with an error before done: {detail}"
                )));
            }
        }
        if !(200..300).contains(&status) || value.get("error").is_some() {
            return Err(ProviderError::ApiError {
                provider: PROVIDER.into(),
                status,
                message: "native Ollama rejected or failed the bounded inference request".into(),
            });
        }
        if self.counts_conflict {
            return Err(protocol("decreasing native usage counters"));
        }
        if (self.usage.usage.input_tokens as u64)
            .checked_add(self.usage.usage.output_tokens as u64)
            .is_none_or(|n| n > self.token_limit)
        {
            return Err(protocol(
                "reported usage exceeds the reserved native token bound",
            ));
        }
        for field in ["prompt_eval_count", "eval_count"] {
            if value
                .get(field)
                .is_some_and(|v| v.as_u64().and_then(|n| usize::try_from(n).ok()).is_none())
            {
                return Err(protocol("invalid native token count"));
            }
        }
        if value.get("model").and_then(Value::as_str) != Some(&self.model) {
            return Err(protocol(
                "native response changed or omitted its requested model",
            ));
        }
        if ["remote_host", "remote_model"]
            .iter()
            .any(|key| value.get(key).is_some_and(|v| v.as_str() != Some("")))
        {
            return Err(protocol("local inference returned remote routing evidence"));
        }
        let done = value
            .get("done")
            .and_then(Value::as_bool)
            .ok_or_else(|| protocol("missing native done flag"))?;
        let message = value
            .get("message")
            .and_then(Value::as_object)
            .ok_or_else(|| protocol("missing native assistant message"))?;
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return Err(protocol("native response has a non-assistant role"));
        }
        let content = message
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| protocol("native content is not text"))?;
        let thinking = match message.get("thinking") {
            None => "",
            Some(value) => value
                .as_str()
                .ok_or_else(|| protocol("native thinking is not text"))?,
        };
        self.content.push_str(content);
        self.thinking.push_str(thinking);
        let mut events = Vec::new();
        if !thinking.is_empty() {
            events.push(StreamEvent::ReasoningDelta {
                delta: thinking.into(),
            });
        }
        if !content.is_empty() {
            events.push(StreamEvent::TextDelta {
                delta: content.into(),
            });
        }
        if let Some(calls) = message.get("tool_calls") {
            if self.rejected.is_none() {
                match self.accept_calls(calls) {
                    Ok(accepted) => {
                        for call in accepted {
                            events.push(StreamEvent::ToolCallDelta {
                                index: Some(self.calls.len()),
                                id: call.id.clone(),
                                name: Some(call.name.clone()),
                                args_delta: call.arguments.to_string(),
                            });
                            self.calls.push(call);
                        }
                    }
                    Err(error) => self.rejected = Some(error),
                }
            }
        }
        if done {
            let reason = value
                .get("done_reason")
                .and_then(Value::as_str)
                .ok_or_else(|| protocol("missing native done reason"))?;
            self.terminal = Some(match reason {
                "stop" if !self.calls.is_empty() => FinishReason::ToolUse,
                "stop" => FinishReason::Stop,
                "length" => FinishReason::MaxTokens,
                _ => return Err(protocol("unsupported native terminal reason")),
            });
        } else if value
            .get("done_reason")
            .is_some_and(|v| v.as_str() != Some(""))
        {
            return Err(protocol("premature native terminal reason"));
        }
        Ok(events)
    }
    fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        let reason = self
            .terminal
            .clone()
            .ok_or_else(|| incomplete_stream("EOF before native terminal"))?;
        validate_required_stream_tool_call_ids(
            PROVIDER,
            &reason,
            self.calls.iter().map(|call| call.id.as_str()),
        )?;
        if self.content.trim().is_empty() && self.calls.is_empty() {
            return Err(protocol(
                "native response has no final content or tool call",
            ));
        }
        if self.request.response_format == Some(ResponseFormat::Json) && self.calls.is_empty() {
            serde_json::from_str::<Value>(&self.content)
                .map_err(|_| protocol("native structured response is not complete JSON"))?;
        }
        // Native thinking belongs to each replayed call. Check the entire
        // repeated encoding before cloning it into a vector of metadata events.
        let mut metadata_bytes = 0usize;
        let available = self
            .cap
            .saturating_sub(ACCOUNTING_RESERVE)
            .saturating_sub(self.emitted);
        for (index, call) in self.calls.iter().enumerate() {
            let size = encoded_size(
                &(
                    index,
                    &call.id,
                    TOOL_METADATA_PROVIDER_ID,
                    PROVIDER,
                    THINKING_METADATA,
                    &self.thinking,
                ),
                available,
            )?
            .checked_add(512)
            .ok_or_else(|| protocol("metadata size overflow"))?;
            metadata_bytes = metadata_bytes
                .checked_add(size)
                .ok_or_else(|| protocol("metadata size overflow"))?;
            if metadata_bytes > available {
                return Err(protocol("native replay metadata exceeds response capacity"));
            }
        }
        let mut events = Vec::new();
        for (index, call) in self.calls.iter().enumerate() {
            let mut metadata = provider_tool_metadata(PROVIDER);
            if !self.thinking.is_empty() {
                metadata.insert(THINKING_METADATA.into(), self.thinking.clone());
            }
            events.push(StreamEvent::ToolCallMetadata {
                index: Some(index),
                id: call.id.clone(),
                metadata,
            });
        }
        self.usage.complete =
            self.terminal_counts && self.request.response_format != Some(ResponseFormat::Json);
        events.push(StreamEvent::UsageObservation(self.usage.clone()));
        events.push(StreamEvent::Done {
            finish_reason: reason,
        });
        Ok(events)
    }
}

#[cfg(test)]
mod tests;
