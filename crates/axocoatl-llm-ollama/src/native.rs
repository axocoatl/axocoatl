//! Explicit local Ollama execution under the shared provider/controller contract.
//!
//! The audited Ollama 0.40.6 runs every GGUF model in llama.cpp's
//! `llama-server`, which rejects oversized inputs with `truncate:false` and
//! stops at a positive `num_predict`; the contract probes check both on the
//! person's own server and model before a Team uses it. Thinking+structured
//! output runs in one pass on 0.40, but format requests still reserve two full
//! passes (a valid, loose bound). Reported final metrics cover only the last
//! pass and are consequently lower bounds for format requests.
//! This is an operator-configured server capability, not model-weight attestation.
//! The existing OpenAI-compatible provider is intentionally unchanged.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    io::Write,
    pin::Pin,
    sync::Arc,
};

use axocoatl_core::{
    ChatMessage, ContentPart, ImageDetail, MeasuredTokenUsage, MessageContent, MessageRole,
    ResponseFormat, TokenUsageStats,
};
use axocoatl_llm::{
    provider_tool_metadata,
    transport::{
        network_error, validated_endpoint, StreamDeadlines, StreamTimeouts, RESPONSE_TIMEOUT,
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

mod contract_probe;
/// The stop probe's prompt, for test servers that stand in for Ollama and
/// answer the contract probes. The over-long probe's message is `"x "`
/// repeated.
pub use contract_probe::STOP_PROBE_PROMPT as CONTRACT_STOP_PROBE_PROMPT;
mod http_error;
pub use http_error::{describe_native_ollama_error, native_ollama_error_hint};
mod observed_context;
mod server_check;
mod version_policy;
pub use observed_context::{
    inspect_native_ollama_model, observe_native_ollama_context,
    observe_native_ollama_context_within, NativeOllamaContextObservation,
    OBSERVATION_SCHEMA_VERSION,
};
use server_check::reports_cloud_disabled;
pub use server_check::{
    check_native_ollama_server, show_native_ollama_model, validate_native_ollama_endpoint,
    NativeOllamaServerCheck, OllamaCloudMode,
};
pub use version_policy::{
    ollama_version_accepted, ollama_version_note, ollama_version_refusal, ollama_version_unaudited,
    AUDITED_OLLAMA_VERSION, DENIED_OLLAMA_VERSIONS,
};

const PROVIDER: &str = "ollama";
const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
// Reserved even when normal payload space is exhausted: one newly observed
// cumulative count snapshot, final accounting, and terminal disposition.
const ACCOUNTING_RESERVE: usize = 2048;
const MAX_MESSAGES: usize = 4096;
const MAX_RECORDS: usize = 65_536;
const THINKING_METADATA: &str = "ollama.native.thinking";
// Where a tool call Qwen3-Coder writes as text begins when it samples a plain
// `<` instead of its `<tool_call>` token (and so where Ollama's parser leaves it
// in the content).
const TEXT_CALL_MARKERS: [&str; 2] = ["<function=", "<tool_call>"];

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
    served: Option<Arc<OllamaServedVersions>>,
    /// How long an answer may take to start and to pause
    /// ([`StreamTimeouts::LOCAL`] unless the configuration sets them).
    stream_timeouts: StreamTimeouts,
}

/// The Ollama versions that served a model's inference requests, as checked
/// right before each request was sent. A run's record and receipt name them.
#[derive(Debug, Default)]
pub struct OllamaServedVersions(std::sync::Mutex<BTreeSet<String>>);

impl OllamaServedVersions {
    fn record(&self, version: &str) {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(version.to_owned());
    }

    /// The versions that served, in order.
    pub fn versions(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}

/// What `GET /api/show` says about a model native execution may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShownModel {
    /// The model reports tool calling.
    pub tools: bool,
    /// The model reports thinking.
    pub thinking: bool,
    /// The model reports image input.
    pub vision: bool,
}

/// A model name as a refusal shows it.
fn shown_name(model: &str) -> String {
    model
        .chars()
        .filter(|ch| !ch.is_control())
        .take(120)
        .collect()
}

/// Refuse a model native execution cannot bound: a cloud model, a model that
/// is not GGUF or that Ollama runs with MLX (MLX holds the model's own maximum
/// context, not `num_ctx`, as its limit), one that does not complete text, and
/// an image-generation model. Shared by admission, observation and the check
/// before each inference.
pub fn check_shown_model(model: &str, shown: &Value) -> Result<ShownModel, ProviderError> {
    let name = shown_name(model);
    let remote = ["remote_host", "remote_model"].iter().any(|key| {
        shown
            .get(key)
            .is_some_and(|value| value.as_str() != Some(""))
    });
    if remote {
        return Err(invalid(format!(
            "{name} is a cloud model that Ollama runs remotely; native execution runs only local models"
        )));
    }
    let format = shown
        .pointer("/details/format")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let runner = shown
        .pointer("/details/runner")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if runner.eq_ignore_ascii_case("mlx") || format != "gguf" {
        return Err(invalid(format!(
            "{name} is {}; native execution accepts only GGUF models, which Ollama runs in \
             llama.cpp's llama-server. Ollama's MLX runner holds the model's own maximum context, \
             not num_ctx, as its limit, so it cannot bound a call. On Apple silicon, Ollama pulls \
             the MLX variant of some models by default",
            if runner.eq_ignore_ascii_case("mlx") {
                "run by Ollama's MLX runner".to_string()
            } else if format.is_empty() {
                "of an unreported format".to_string()
            } else {
                format!("a {} model", shown_name(format))
            }
        )));
    }
    let caps = shown
        .get("capabilities")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("model capabilities were not reported"))?;
    let has = |capability: &str| caps.iter().any(|v| v.as_str() == Some(capability));
    if !has("completion") {
        return Err(invalid(format!(
            "{name} does not report text completion; native execution needs a local GGUF \
             completion model"
        )));
    }
    if has("image") {
        return Err(invalid(format!(
            "{name} is an image-generation model; native execution needs a local GGUF completion model"
        )));
    }
    Ok(ShownModel {
        tools: has("tools"),
        thinking: has("thinking"),
        vision: has("vision"),
    })
}

/// The version `GET /api/version` reported, when the version policy accepts it.
pub(crate) fn accepted_server_version(reported: &Value) -> Result<String, ProviderError> {
    let version = reported
        .get("version")
        .and_then(Value::as_str)
        .filter(|text| text.len() <= 64 && !text.chars().any(char::is_control))
        .ok_or_else(|| invalid("the Ollama server did not report its version"))?;
    match ollama_version_refusal(version) {
        Some(refusal) => Err(invalid(refusal)),
        None => Ok(version.to_owned()),
    }
}

/// What the check before connecting and before each inference found.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Verified {
    version: String,
    model: ShownModel,
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
/// How a refused request states its `Retry-After` at the end of the error
/// message: `… (Retry-After: 5 s)`. The Session's retry policy reads it.
pub const RETRY_AFTER_SUFFIX: &str = " (Retry-After: ";

/// A response's `Retry-After`, in whole seconds from now: delta-seconds or
/// an HTTP date (a date in the past is zero).
fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(
        at.duration_since(std::time::SystemTime::now())
            .map_or(0, |wait| {
                wait.as_secs()
                    .saturating_add(u64::from(wait.subsec_nanos() > 0))
            }),
    )
}

/// `error` with the response's `Retry-After` stated, when it is a status
/// error and the response gave one.
fn with_retry_after(error: ProviderError, retry_after: Option<u64>) -> ProviderError {
    match (error, retry_after) {
        (
            ProviderError::ApiError {
                provider,
                status,
                message,
            },
            Some(seconds),
        ) => ProviderError::ApiError {
            provider,
            status,
            message: format!("{message}{RETRY_AFTER_SUFFIX}{seconds} s)"),
        },
        (error, _) => error,
    }
}

/// A transport failure, named `timed out: …` or `connection reset: …` when
/// it was one, so a retry policy can tell them from other failures.
fn transport_error(error: &reqwest::Error) -> ProviderError {
    use std::io::ErrorKind;
    let message = match network_error(error, &[]) {
        ProviderError::Network(message) => message,
        other => return other,
    };
    let mut reset = false;
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        if cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::UnexpectedEof
            )
        }) {
            reset = true;
            break;
        }
        let text = cause.to_string().to_ascii_lowercase();
        if text.contains("connection reset")
            || text.contains("connection closed before message completed")
            || text.contains("broken pipe")
        {
            reset = true;
            break;
        }
        source = cause.source();
    }
    ProviderError::Network(if error.is_timeout() {
        format!("timed out: {message}")
    } else if reset {
        format!("connection reset: {message}")
    } else {
        message
    })
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
    metadata_request_within(client, base_url, path, body, RESPONSE_TIMEOUT).await
}

/// [`metadata_request`] whose answer may take `timeout`: a model load, which
/// Ollama answers once the model is loaded.
async fn metadata_request_within(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    body: Option<Value>,
    timeout: std::time::Duration,
) -> Result<Value, ProviderError> {
    let url = validated_endpoint(base_url, path, PROVIDER)?;
    // The model the request names, for a 404 about it.
    let model = body
        .as_ref()
        .and_then(|body| body.get("model"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let builder = match body {
        Some(body) => client.post(url).json(&body),
        None => client.get(url),
    };
    let mut response = builder
        .timeout(timeout)
        .send()
        .await
        .map_err(|error| network_error(&error, &[]))?;
    let status = response.status();
    if !status.is_success() {
        // Ollama's status and its own message, bounded: a model it does not
        // have, a busy server, a model it could not load.
        let retry_after = retry_after_secs(response.headers());
        let text = axocoatl_llm::transport::read_error_text(response, &[]).await;
        return Err(with_retry_after(
            http_error::status_error(status.as_u16(), model.as_deref(), &text),
            retry_after,
        ));
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
    /// Connect after checking the server (loopback, accepted version,
    /// cloud-disabled mode) and the model. Without a retained observation no
    /// contract probe runs: [`Self::connect_observed`] is the Session path.
    pub async fn connect(config: NativeOllamaConfig) -> Result<Self, ProviderError> {
        Self::connect_verified(config)
            .await
            .map(|(provider, _)| provider)
    }

    /// [`Self::connect`], with the Ollama version the check found.
    async fn connect_verified(config: NativeOllamaConfig) -> Result<(Self, String), ProviderError> {
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
            served: None,
            stream_timeouts: StreamTimeouts::LOCAL,
        };
        let verified = provider.verify().await?;
        provider.tools_supported = verified.model.tools;
        provider.thinking_supported = verified.model.thinking;
        provider.vision_supported = verified.model.vision;
        Ok((provider, verified.version))
    }

    /// Record in `log` the Ollama version checked right before each inference
    /// request this provider sends: the version that served it.
    pub fn with_served_versions(mut self, log: Arc<OllamaServedVersions>) -> Self {
        self.served = Some(log);
        self
    }

    /// Wait for each answer as `timeouts` say (`providers.ollama`'s
    /// `first_token_timeout_secs` and `idle_timeout_secs`).
    pub fn with_stream_timeouts(mut self, timeouts: StreamTimeouts) -> Self {
        self.stream_timeouts = timeouts;
        self
    }

    async fn metadata(&self, path: &str, body: Option<Value>) -> Result<Value, ProviderError> {
        metadata_request(&self.client, &self.config.base_url, path, body).await
    }

    async fn verify(&self) -> Result<Verified, ProviderError> {
        let version = accepted_server_version(&self.metadata("api/version", None).await?)?;
        let status = self.metadata("api/status", None).await?;
        if !reports_cloud_disabled(&status) {
            return Err(invalid(
                "native execution requires server-reported cloud-disabled mode",
            ));
        }
        let shown = self
            .metadata("api/show", Some(json!({"model": self.config.model})))
            .await?;
        Ok(Verified {
            version,
            model: check_shown_model(&self.config.model, &shown)?,
        })
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
        let verified = self.verify().await?;
        if let Some(observation) = &self.observed_context {
            // A model pulled again, or with other tools or thinking, since
            // its Team was reviewed is refused with what to do.
            observed_context::verify_identity(self, observation).await?;
        }
        if verified.model.tools != self.tools_supported
            || verified.model.thinking != self.thinking_supported
            || verified.model.vision != self.vision_supported
        {
            // Changed since this provider connected, when this Agent's work
            // started: the next request connects to the model as it is now.
            return Err(invalid(format!(
                "Ollama reports other capabilities for {} than when this Agent's work \
                 started; send the request again",
                shown_name(&self.config.model)
            )));
        }
        if let Some(observation) = &self.observed_context {
            // An Ollama upgraded since the probes passed is probed again
            // before it serves; the same version is a cached pass.
            observed_context::ensure_probed(self, observation, &verified.version).await?;
        }
        if let Some(log) = &self.served {
            log.record(&verified.version);
        }
        let url = validated_endpoint(&self.config.base_url, "api/chat", PROVIDER)?;
        // Ollama sends its headers with the first token, after loading the
        // model and reading the prompt.
        let mut deadlines = StreamDeadlines::start(self.stream_timeouts);
        let response = deadlines
            .headers(self.client.post(url).json(&body).send(), PROVIDER)
            .await?
            .map_err(|error| transport_error(&error))?;
        let status = response.status().as_u16();
        let retry_after = retry_after_secs(response.headers());
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
                let next = match deadlines.next(&mut bytes, deadline, PROVIDER).await {
                    Ok(value) => value,
                    Err(error) => { yield Err(error); return; }
                };
                let eof = next.is_none();
                if let Some(chunk) = next {
                    let chunk = match chunk { Ok(chunk) => chunk, Err(error) => { yield Err(transport_error(&error)); return; } };
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
                        Err(error) => { yield Err(with_retry_after(error, retry_after)); return; }
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
                yield Err(with_retry_after(ProviderError::ApiError {
                    provider: PROVIDER.into(),
                    status,
                    message: "native Ollama rejected or failed the bounded inference request".into(),
                }, retry_after));
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

// llama-server counts each image's tokens in the prompt before its context
// check (llama.cpp b11669, tools/server/server-common.cpp, the multimodal
// input chunks; Ollama v0.40.6 sends images inline as base64 only).
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

/// Give each text value of a call written as text the type the tool's schema
/// declares for it (integer, number or boolean), as Ollama's Qwen3-Coder parser
/// does for the calls it lifts. A value that does not parse stays text.
fn coerce_text_arguments(arguments: &mut Value, schema: &Value) {
    let (Some(arguments), Some(properties)) = (
        arguments.as_object_mut(),
        schema.get("properties").and_then(Value::as_object),
    ) else {
        return;
    };
    for (key, value) in arguments.iter_mut() {
        let declared: Vec<&str> = match properties.get(key).and_then(|p| p.get("type")) {
            Some(Value::String(kind)) => vec![kind.as_str()],
            Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
            _ => continue,
        };
        if let Some(typed) = value.as_str().and_then(|text| typed_text(text, &declared)) {
            *value = typed;
        }
    }
}

/// `text` as the first declared type it parses as, tried in Ollama's order.
fn typed_text(text: &str, declared: &[&str]) -> Option<Value> {
    let text = text.trim();
    if declared.contains(&"integer") || declared.contains(&"number") {
        if let Ok(number) = text.parse::<i64>() {
            return Some(number.into());
        }
    }
    if declared.contains(&"number") {
        if let Some(number) = text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
        {
            return Some(Value::Number(number));
        }
    }
    if declared.contains(&"boolean") {
        if text.eq_ignore_ascii_case("true") {
            return Some(true.into());
        }
        if text.eq_ignore_ascii_case("false") {
            return Some(false.into());
        }
    }
    if declared.contains(&"null") && text == "null" {
        return Some(Value::Null);
    }
    if declared.contains(&"array") || declared.contains(&"object") {
        if let Ok(json) = serde_json::from_str::<Value>(text) {
            if (json.is_array() && declared.contains(&"array"))
                || (json.is_object() && declared.contains(&"object"))
            {
                return Some(json);
            }
        }
    }
    None
}

/// Calls checked per answer and per run, and the longest answer read for
/// them: bounds on work done inside the stream.
const TEXT_CALL_LIMIT: usize = 128;
const TEXT_CALL_CONTENT_LIMIT: usize = 256 * 1024;

/// A call block's tool name and its parameters, as written.
type TextCall<'a> = (&'a str, Vec<(&'a str, &'a str)>);

/// One `<function=NAME>` block of Qwen3-Coder's call format at the start of
/// `text`: its name, its `<parameter=KEY>VALUE</parameter>` pairs and its
/// length. `None` unless only whitespace and parameter pairs come between the
/// opening tag and `</function>`. A value is text up to `</parameter>`, never
/// read for further calls, and holds no line that opens or closes a tag (a
/// value missing its `</parameter>` would otherwise swallow the next call);
/// one newline on each side is the format's own.
fn function_block(text: &str) -> Option<(TextCall<'_>, usize)> {
    let rest = text.strip_prefix("<function=")?;
    let (name, mut body) = rest.split_once('>')?;
    if !plain_tag_word(name) {
        return None;
    }
    let mut parameters: Vec<(&str, &str)> = Vec::new();
    let mut keys = HashSet::new();
    loop {
        body = body.trim_start();
        if let Some(after) = body.strip_prefix("</function>") {
            return Some(((name, parameters), text.len() - after.len()));
        }
        let (key, after) = body.strip_prefix("<parameter=")?.split_once('>')?;
        let (value, after) = after.split_once("</parameter>")?;
        if !plain_tag_word(key) || !keys.insert(key) || swallows_markup(value) {
            return None;
        }
        let value = value
            .strip_prefix("\r\n")
            .or_else(|| value.strip_prefix('\n'))
            .unwrap_or(value);
        let value = value
            .strip_suffix("\r\n")
            .or_else(|| value.strip_suffix('\n'))
            .unwrap_or(value);
        parameters.push((key, value));
        body = after;
    }
}

/// Whether a parameter value holds markup of the call around it rather than
/// text: a line that opens or closes a tag, another parameter, or a closed
/// call followed by another (a value missing its `</parameter>` takes in
/// what comes after it, on the same line or the next).
fn swallows_markup(value: &str) -> bool {
    let tags = [
        "<function=",
        "</function>",
        "<parameter=",
        "<tool_call>",
        "</tool_call>",
    ];
    value
        .lines()
        .any(|line| tags.iter().any(|tag| line.trim_start().starts_with(tag)))
        || value.contains("<parameter=")
        || value
            .split_once("</function>")
            .is_some_and(|(_, after)| after.contains("<function=") || after.contains("<tool_call>"))
}

/// A tool or parameter name inside a tag: no whitespace or markup.
fn plain_tag_word(word: &str) -> bool {
    !word.is_empty()
        && !word
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | '`'))
}

/// The `<function=…>` blocks `text` consists of, with only whitespace and
/// `<tool_call>` tags around them; `None` if anything else is in it.
fn call_run(mut text: &str) -> Option<Vec<TextCall<'_>>> {
    let mut blocks = Vec::new();
    loop {
        text = text.trim_start();
        if text.is_empty() {
            return (!blocks.is_empty()).then_some(blocks);
        }
        if let Some(after) = text
            .strip_prefix("<tool_call>")
            .or_else(|| text.strip_prefix("</tool_call>"))
        {
            text = after;
            continue;
        }
        let (block, used) = function_block(text)?;
        blocks.push(block);
        if blocks.len() > TEXT_CALL_LIMIT {
            return None;
        }
        text = &text[used..];
    }
}

/// A code fence line as Markdown reads it: up to three spaces, then three or
/// more backticks (with no backtick after them) or tildes. Its character and
/// length, and whether only whitespace follows (so it can close a fence).
fn fence_line(line: &str) -> Option<(char, usize, bool)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let line = &line[indent..];
    let mark = line.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let length = line.len() - line.trim_start_matches(mark).len();
    let info = &line[length..];
    if length < 3 || (mark == '`' && info.contains('`')) {
        return None;
    }
    Some((mark, length, info.trim_matches([' ', '\t']).is_empty()))
}

/// The calls that end `content`, and where they start: the first line that
/// begins with a call tag outside a code fence must begin a run of
/// `<function=…>` blocks ([`call_run`]) that goes to the end. Anything else
/// (a call quoted in a fence, indented, inline, in another call's value or
/// with prose after it, or a malformed block) leaves the text as the answer.
fn trailing_call_run(content: &str) -> Option<(usize, Vec<TextCall<'_>>)> {
    if content.len() > TEXT_CALL_CONTENT_LIMIT {
        return None;
    }
    let mut fence: Option<(char, usize)> = None;
    let mut at = 0usize;
    for line in content.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let text = line.trim_end_matches(['\n', '\r']);
        if let Some((mark, length, bare)) = fence_line(text) {
            fence = match fence {
                None => Some((mark, length)),
                Some((open, need)) if open == mark && length >= need && bare => None,
                open => open,
            };
            continue;
        }
        if fence.is_none() && (line.starts_with("<function=") || line.starts_with("<tool_call>")) {
            // Markdown also ends a line at a lone CR, which these lines do not
            // split on: a fence before the call could hide behind one.
            let lone_cr = content[..start]
                .match_indices('\r')
                .any(|(at, _)| !content[at + 1..].starts_with('\n'));
            if lone_cr {
                return None;
            }
            return call_run(&content[start..]).map(|run| (start, run));
        }
    }
    None
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
    /// How much of `content` has been released as text.
    released: usize,
    /// `content` from a text call marker on is held until `done`.
    holding: bool,
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
            released: 0,
            holding: false,
            thinking: String::new(),
            calls: Vec::new(),
            rejected: None,
        }
    }
    /// The part of `content` that can be shown now: all of it when no tools
    /// were offered. Otherwise text from a `<function=` or `<tool_call>` marker
    /// on is held until `done` shows whether it was a call written as text,
    /// and a short tail is kept so a marker split across records is never
    /// half-shown.
    fn release_text(&mut self) -> Option<String> {
        let end = if self.request.tools.is_empty() {
            self.content.len()
        } else if self.holding {
            self.released
        } else if let Some(at) = TEXT_CALL_MARKERS
            .iter()
            .filter_map(|marker| self.content[self.released..].find(marker))
            .min()
        {
            self.holding = true;
            self.released + at
        } else {
            super::flush_boundary(&self.content, 16).max(self.released)
        };
        if end <= self.released {
            return None;
        }
        let delta = self.content[self.released..end].to_owned();
        self.released = end;
        Some(delta)
    }
    /// Calls Qwen3-Coder wrote as text after sampling a plain `<` instead of
    /// its `<tool_call>` token: Ollama's parser lifts only calls that open with
    /// that token and returns the rest as content, ended by `stop`. Only the
    /// run of call blocks that ends the answer is read ([`trailing_call_run`]),
    /// and only when every block names an offered tool; never bare JSON. Each
    /// call gets an id no call in the history has, and its text values the
    /// types the tool's schema declares, as Ollama's own parser gives them.
    /// `None` (the text stays the answer) otherwise.
    fn recover_text_calls(&self) -> Option<(usize, Vec<ToolCall>)> {
        let (start, run) = trailing_call_run(&self.content)?;
        let used: HashSet<&str> = self
            .request
            .messages
            .iter()
            .flat_map(|message| &message.tool_calls)
            .map(|call| call.id.as_str())
            .collect();
        let mut next = 0usize;
        let mut calls = Vec::with_capacity(run.len());
        for (name, parameters) in run {
            let tool = self.request.tools.iter().find(|tool| tool.name == name)?;
            let mut arguments = Value::Object(
                parameters
                    .into_iter()
                    .map(|(key, value)| (key.to_owned(), Value::String(value.to_owned())))
                    .collect(),
            );
            coerce_text_arguments(&mut arguments, &tool.parameters);
            validate_response_tool_call(PROVIDER, name, &arguments, &self.request.tools).ok()?;
            let id = loop {
                let id = format!("call_text_{next}");
                next += 1;
                if !used.contains(id.as_str()) {
                    break id;
                }
            };
            calls.push(ToolCall {
                id,
                name: name.to_owned(),
                arguments,
                provider_metadata: provider_tool_metadata(PROVIDER),
            });
        }
        Some((start, calls))
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
        if let Some(delta) = self.release_text() {
            events.push(StreamEvent::TextDelta { delta });
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
        let mut reason = self
            .terminal
            .clone()
            .ok_or_else(|| incomplete_stream("EOF before native terminal"))?;
        let mut events = Vec::new();
        let recovered = if matches!(reason, FinishReason::Stop)
            && self.holding
            && self.calls.is_empty()
            && self.request.response_format != Some(ResponseFormat::Json)
        {
            self.recover_text_calls()
        } else {
            None
        };
        let answer_end = recovered
            .as_ref()
            .map_or(self.content.len(), |(start, _)| *start);
        // The held text up to any recovered markup is part of the answer.
        if self.released < answer_end {
            events.push(StreamEvent::TextDelta {
                delta: self.content[self.released..answer_end].to_owned(),
            });
            self.released = answer_end;
        }
        if let Some((_, recovered)) = recovered {
            // The markup is replaced by the calls it wrote, so the transcript
            // keeps the prose before it and each call once.
            for call in recovered {
                events.push(StreamEvent::ToolCallDelta {
                    index: Some(self.calls.len()),
                    id: call.id.clone(),
                    name: Some(call.name.clone()),
                    args_delta: call.arguments.to_string(),
                });
                self.calls.push(call);
            }
            reason = FinishReason::ToolUse;
        }
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
