//! Audited OpenRouter text/tool execution. The retained exact endpoint and wire
//! ceilings are verified before each single inference request; each request
//! pins one endpoint with no OpenRouter fallback, and this provider never
//! retries. It may hold other endpoints of the same model, each retained
//! exactly, that a Session moves a call to after the current one failed it
//! before producing anything ([`EndpointFailover`]); calls stay there. It
//! states what a Session's retry policy needs: the status and `Retry-After`
//! of a refused request, whether a transport failure was a timeout or a reset
//! connection, and whether a failed call is known to have cost nothing.
//! Requires an operator-declared OpenRouter-credit billing configuration. BYOK
//! is unsupported; enabling it externally invalidates this execution contract.
//!
//! Each call reserves a bound of its own exact request, not the endpoint's
//! context window: the request's bytes plus a template allowance (and the
//! recorded reasoning tokens it replays) for the prompt, plus its `max_tokens`
//! (visible output and reasoning allowance), at the highest input and output
//! rates the endpoint lists. The provider's terminal usage and cost settle it.
use crate::OpenAiProvider;
use axocoatl_core::{MeasuredTokenUsage, TokenUsageStats};
use axocoatl_llm::transport::{
    http_client, network_error, read_error_text, read_json, validated_endpoint, SseDecoder,
    StreamDeadlines, StreamTimeouts, MAX_STREAM_BYTES, RESPONSE_TIMEOUT,
};
use axocoatl_llm::{
    provider_tool_metadata, validate_chat_response, validate_response_tool_call,
    AccountedChatOutcome, ChatRequest, ChatResponse, EndpointFailover, FailedCall, FailureFacts,
    FinishReason, LlmProvider, ProviderCapabilities, ProviderError, ProviderExecutionBounds,
    StreamEvent, ToolCall,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::{collections::BTreeMap, pin::Pin};
use tokio_stream::{Stream, StreamExt};
#[path = "native_openrouter/failover.rs"]
mod failover;
#[path = "native_openrouter/money.rs"]
mod money;
#[path = "native_openrouter/observation.rs"]
mod observation;
#[path = "native_openrouter/stream.rs"]
mod stream;
pub use failover::{NativeOpenRouterServed, ServedOpenRouterEndpoint};
use observation::CallShape;
pub use observation::{
    catalog_call_floor, observe_native_openrouter_endpoints, observe_native_openrouter_profiles,
    CatalogCallFloor, NativeOpenRouterEfforts, NativeOpenRouterEndpoints,
    NativeOpenRouterObservation, NativeOpenRouterReasoning, NativeOpenRouterReasoningRequest,
    UnavailableOpenRouterEndpoint, MINIMUM_REASONING_ALLOWANCE, PROMPT_TEMPLATE_ALLOWANCE,
    STREAMED_BYTES_PER_RESPONSE_TOKEN,
};

const PROVIDER: &str = "openrouter";
/// The exact reasoning blocks of a tool-calling response, as OpenRouter
/// returned them, kept on its first tool call to be sent back unmodified.
pub const REASONING_DETAILS_METADATA: &str = "axocoatl.openrouter.reasoning_details";
/// The reasoning tokens that response reported. Replaying its reasoning can
/// bill them again as input, so the next prompt bound includes them.
pub const REASONING_TOKENS_METADATA: &str = "axocoatl.openrouter.reasoning_tokens";

fn invalid(reason: impl Into<String>) -> ProviderError {
    ProviderError::InvalidRequest {
        provider: PROVIDER.into(),
        message: reason.into(),
    }
}
/// How this provider's own account of a malformed or refused answer
/// begins.
const PROTOCOL_PREFIX: &str = "native OpenRouter: ";
fn protocol(reason: impl Into<String>) -> ProviderError {
    ProviderError::Stream(format!("{PROTOCOL_PREFIX}{}", reason.into()))
}

/// How a refused request states its `Retry-After` at the end of the error
/// message: `… (Retry-After: 5 s)`. The Session's retry policy reads it.
pub const RETRY_AFTER_SUFFIX: &str = " (Retry-After: ";

/// A response's `Retry-After`, in whole seconds from now: delta-seconds or
/// an HTTP date (a date in the past is zero).
pub fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
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

/// `message` with `retry_after` appended as [`RETRY_AFTER_SUFFIX`] states it.
pub fn with_retry_after(message: String, retry_after: Option<u64>) -> String {
    match retry_after {
        Some(seconds) => format!("{message}{RETRY_AFTER_SUFFIX}{seconds} s)"),
        None => message,
    }
}

/// A transport failure, named `timed out: …` or `connection reset: …` when
/// it was one, so a retry policy can tell them from other failures. Never
/// carries a secret.
pub(crate) fn transport_error(error: &reqwest::Error, secrets: &[&str]) -> ProviderError {
    let ProviderError::Network(message) = network_error(error, secrets) else {
        return network_error(error, secrets);
    };
    let kind = if error.is_timeout() {
        Some("timed out")
    } else if connection_reset(error) {
        Some("connection reset")
    } else {
        None
    };
    ProviderError::Network(match kind {
        Some(kind) => format!("{kind}: {message}"),
        None => message,
    })
}

/// Whether a request failed because the peer reset or closed the connection.
fn connection_reset(error: &reqwest::Error) -> bool {
    use std::io::ErrorKind;
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::UnexpectedEof
            ) {
                return true;
            }
        }
        let text = cause.to_string().to_ascii_lowercase();
        if text.contains("connection reset")
            || text.contains("connection closed before message completed")
            || text.contains("broken pipe")
        {
            return true;
        }
        source = cause.source();
    }
    false
}

pub struct NativeOpenRouterProvider {
    inner: OpenAiProvider,
    /// The endpoints calls may go to, each retained exactly: the one Team &
    /// budget selected, then those retained for failover, cheapest first.
    /// They serve one model under one reasoning setting and output limit.
    endpoints: Vec<NativeOpenRouterObservation>,
    /// The endpoint calls go to now: the first, until a Session moves them.
    active: AtomicUsize,
    max_output_tokens: usize,
    max_response_bytes: usize,
    reasoning: Option<NativeOpenRouterReasoningRequest>,
    /// Where the endpoints that answered, and those calls moved away from,
    /// are recorded, when something reads them.
    served: Option<Arc<NativeOpenRouterServed>>,
}
impl NativeOpenRouterProvider {
    pub async fn connect_observed(
        observation: NativeOpenRouterObservation,
        api_key: &str,
        max_output_tokens: usize,
        max_response_bytes: usize,
        reasoning: Option<NativeOpenRouterReasoningRequest>,
    ) -> Result<Self, ProviderError> {
        Self::connect_observed_endpoints(
            vec![observation],
            api_key,
            max_output_tokens,
            max_response_bytes,
            reasoning,
        )
        .await
    }

    /// A provider over `endpoints`: the selected endpoint first, then the
    /// others retained for failover. Each must hold the same execution
    /// bounds; at least one must match OpenRouter's catalog now. Calls start
    /// on the first.
    pub async fn connect_observed_endpoints(
        endpoints: Vec<NativeOpenRouterObservation>,
        api_key: &str,
        max_output_tokens: usize,
        max_response_bytes: usize,
        reasoning: Option<NativeOpenRouterReasoningRequest>,
    ) -> Result<Self, ProviderError> {
        let Some(first) = endpoints.first() else {
            return Err(invalid("no retained OpenRouter endpoint"));
        };
        for (index, observation) in endpoints.iter().enumerate() {
            observation.validate()?;
            if observation.model != first.model
                || observation.base_url != first.base_url
                || endpoints[..index]
                    .iter()
                    .any(|earlier| earlier.endpoint_tag == observation.endpoint_tag)
            {
                return Err(invalid(
                    "retained OpenRouter endpoints must serve one model, each once",
                ));
            }
            if !observation.accepts_reasoning(reasoning) {
                return Err(invalid(format!(
                    "the retained reasoning setting is not one {} accepts",
                    observation.model
                )));
            }
            if api_key.is_empty()
                || max_output_tokens == 0
                || max_output_tokens > observation.max_output_tokens
                || observation.response_allowance(max_output_tokens, reasoning)
                    >= observation.context_tokens
                || !(4096..=16 * 1024 * 1024).contains(&max_response_bytes)
                || observation
                    .response_refusal(max_output_tokens, reasoning, max_response_bytes)
                    .is_some()
            {
                return Err(invalid(
                    "unsupported exact OpenRouter execution bounds or missing credential",
                ));
            }
            observation.minimum_call_bounds(max_output_tokens, reasoning, max_response_bytes)?;
        }
        let this = Self {
            inner: OpenAiProvider::with_base_url(api_key, first.model.clone(), &first.base_url)
                .with_provider_id(PROVIDER),
            endpoints,
            active: AtomicUsize::new(0),
            max_output_tokens,
            max_response_bytes,
            reasoning,
            served: None,
        };
        this.verify_any().await?;
        Ok(this)
    }
    /// Wait for each answer as `timeouts` say (`providers.openrouter`'s
    /// `first_token_timeout_secs` and `idle_timeout_secs`).
    pub fn with_stream_timeouts(mut self, timeouts: StreamTimeouts) -> Self {
        self.inner = self.inner.with_stream_timeouts(timeouts);
        self
    }
    /// Record each endpoint that answers a call in `served`.
    pub fn with_served_endpoints(mut self, served: Arc<NativeOpenRouterServed>) -> Self {
        self.served = Some(served);
        self
    }
    /// The endpoint calls go to now.
    fn active(&self) -> usize {
        self.active
            .load(Ordering::Acquire)
            .min(self.endpoints.len() - 1)
    }
    fn endpoint(&self) -> &NativeOpenRouterObservation {
        &self.endpoints[self.active()]
    }
    /// The endpoint Team & budget selected. The Agent's context is fitted
    /// to it; every endpoint retained for failover holds at least as much.
    fn primary(&self) -> &NativeOpenRouterObservation {
        &self.endpoints[0]
    }
    /// What OpenRouter's catalog says of this model's endpoints now. A
    /// failure is OpenRouter's, before anything was sent.
    async fn live_endpoints(&self) -> Result<NativeOpenRouterEndpoints, FailedCall> {
        observe_native_openrouter_endpoints(
            &self.primary().base_url,
            &self.inner.api_key,
            &self.primary().model,
        )
        .await
        .map_err(failover::catalog_unreadable)
    }
    /// Whether `observation` still matches `live` and OpenRouter reports it
    /// healthy; why not, in plain words, when it does not. Nothing was sent:
    /// it costs nothing.
    fn contract_now(
        &self,
        observation: &NativeOpenRouterObservation,
        live: &NativeOpenRouterEndpoints,
    ) -> Result<(), FailedCall> {
        if live
            .profiles
            .iter()
            .any(|profile| observation.same_contract(profile))
        {
            return Ok(());
        }
        // The endpoint is the same, but OpenRouter reports it unavailable
        // or degraded right now: said as that, not as a changed contract.
        if let Some(down) = live
            .unavailable
            .iter()
            .find(|endpoint| observation.same_contract(&endpoint.profile))
        {
            return Err(failover::unsent(
                ProviderError::ApiError {
                    provider: PROVIDER.into(),
                    status: 503,
                    message: format!(
                        "OpenRouter reports the endpoint this Session uses for {} as \
                         unavailable or degraded right now ({}). This is OpenRouter's provider \
                         status: try again later, or choose another model; {}",
                        observation.model,
                        down.describe(),
                        failover::NOTHING_SENT
                    ),
                },
                true,
                "reported unavailable or degraded right now",
            ));
        }
        // No endpoint of the model can be used now: every other one would
        // say the same.
        if let Some(refusal) = live.refusal(&observation.model) {
            let refusal = match &refusal {
                ProviderError::ApiError { message, .. }
                | ProviderError::InvalidRequest { message, .. }
                    if message.contains(failover::NOTHING_SENT) =>
                {
                    refusal
                }
                _ => failover::with_words(refusal, &format!("; {}", failover::NOTHING_SENT)),
            };
            return Err(failover::unsent(refusal, false, ""));
        }
        Err(failover::unsent(
            invalid(format!(
                "the retained OpenRouter endpoint {} of {} {}: its capabilities, price ceiling \
                 or token limits differ now; apply Team & budget again to review a new profile; \
                 {}",
                failover::describe(observation),
                observation.model,
                failover::CONTRACT_CHANGED,
                failover::NOTHING_SENT
            )),
            true,
            "its contract changed since Team & budget reviewed it",
        ))
    }
    /// Check the current endpoint against the catalog before a call.
    async fn verify(&self, observation: &NativeOpenRouterObservation) -> Result<(), FailedCall> {
        let live = self.live_endpoints().await?;
        self.contract_now(observation, &live)
    }
    /// At least one retained endpoint can be used now: the first that
    /// cannot says why when none can, or, with several, each one does.
    async fn verify_any(&self) -> Result<(), ProviderError> {
        let live = self.live_endpoints().await.map_err(|failed| failed.error)?;
        let mut failures = Vec::new();
        for (index, observation) in self.endpoints.iter().enumerate() {
            match self.contract_now(observation, &live) {
                Ok(()) => return Ok(()),
                Err(failed) => failures.push((index, failed)),
            }
        }
        Err(match failures.len() {
            1 => failures.remove(0).1.error,
            _ => failover::exhausted(&self.endpoints, failures),
        })
    }
    /// Whether `request` fits endpoint `index`'s retained contract.
    fn carries(&self, index: usize, request: &ChatRequest) -> bool {
        let observation = &self.endpoints[index];
        self.body_for(observation, request)
            .and_then(|(_, shape)| observation.call_bounds(shape, self.max_response_bytes))
            .is_ok()
    }
    fn body(&self, request: &ChatRequest) -> Result<(Value, CallShape), ProviderError> {
        self.body_for(self.endpoint(), request)
    }
    /// The exact request body to `observation` and its bounds. Every byte of
    /// the body counts toward the prompt bound: the provider renders the
    /// messages and tools, and each token of a byte-level tokenizer covers at
    /// least one byte.
    fn body_for(
        &self,
        observation: &NativeOpenRouterObservation,
        request: &ChatRequest,
    ) -> Result<(Value, CallShape), ProviderError> {
        self.validate_request_for(observation, request)?;
        let output = request.max_tokens.unwrap_or(self.max_output_tokens);
        let response_tokens = observation.response_allowance(output, self.reasoning);
        let mut normalized = request.clone();
        normalized.max_tokens = Some(output);
        let mut body = serde_json::to_value(self.inner.build_chat_request(&normalized)?)
            .map_err(|_| invalid("request serialization failed"))?;
        let object = body
            .as_object_mut()
            .ok_or_else(|| invalid("request serialization failed"))?;
        object.remove("max_completion_tokens");
        object.insert("max_tokens".into(), json!(response_tokens));
        object.insert("stream".into(), json!(true));
        object.insert(
            "provider".into(),
            json!({
                "only": [observation.endpoint_tag],
                "order": [observation.endpoint_tag],
                "allow_fallbacks": false,
                "require_parameters": true,
                // OpenRouter filters endpoints on their listed image and
                // audio prices even for a text-only request, so those caps
                // are left out: the request sends neither.
                "max_price": {
                    "prompt": observation.prompt_price_per_million,
                    "completion": observation.completion_price_per_million,
                    "request": observation.request_price.as_deref().unwrap_or("0"),
                },
            }),
        );
        object.insert("transforms".into(), json!([]));
        // An account can turn the web plugin or context compression on for
        // every request; an explicit disable overrides that default. The
        // model must see the exact conversation the Session recorded,
        // including replayed reasoning, never a compressed one.
        object.insert(
            "plugins".into(),
            json!([
                {"id": "web", "enabled": false},
                {"id": "context-compression", "enabled": false},
            ]),
        );
        if let Some(reasoning) = self.reasoning {
            object.insert("reasoning".into(), reasoning.wire());
        }
        if object
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| tools.iter().any(|tool| tool["type"] != "function"))
        {
            return Err(invalid("native OpenRouter sends function tools only"));
        }
        let replayed = replay_reasoning(request, &mut body)?;
        let bytes = serde_json::to_vec(&body)
            .map_err(|_| invalid("request serialization failed"))?
            .len();
        let prompt_tokens = PROMPT_TEMPLATE_ALLOWANCE
            .saturating_add(bytes)
            .saturating_add(replayed)
            .min(observation.prompt_limit());
        Ok((
            body,
            CallShape {
                prompt_tokens,
                response_tokens,
            },
        ))
    }
    /// Whether `request` is one `observation` accepts as retained.
    fn validate_request_for(
        &self,
        observation: &NativeOpenRouterObservation,
        request: &ChatRequest,
    ) -> Result<(), ProviderError> {
        axocoatl_llm::validate_provider_request(request, PROVIDER)?;
        if request
            .model_override
            .as_ref()
            .is_some_and(|model| model != &observation.model)
            || request
                .max_tokens
                .is_some_and(|limit| limit == 0 || limit > self.max_output_tokens)
            || request.provider_options.is_some()
        {
            return Err(invalid(
                "request differs from retained OpenRouter route or output ceiling",
            ));
        }
        for message in &request.messages {
            if !matches!(message.content, axocoatl_core::MessageContent::Text(_)) {
                return Err(invalid(
                    "this audited OpenRouter profile accepts only text and local tool messages",
                ));
            }
        }
        for (needed, present) in [
            ("temperature", request.temperature.is_some()),
            ("top_p", request.top_p.is_some()),
            ("stop", !request.stop_sequences.is_empty()),
            ("response_format", request.response_format.is_some()),
        ] {
            if present && !observation.supported_parameters.iter().any(|p| p == needed) {
                return Err(invalid(format!(
                    "retained OpenRouter endpoint does not support {needed}"
                )));
            }
        }
        Ok(())
    }
}

impl EndpointFailover for NativeOpenRouterProvider {
    fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }
    fn current_endpoint(&self) -> usize {
        self.active()
    }
    fn candidates(&self, request: &ChatRequest, excluded: &[usize]) -> Vec<usize> {
        let from = self.active();
        let count = self.endpoints.len();
        (1..count)
            .map(|step| (from + step) % count)
            .filter(|index| !excluded.contains(index) && self.carries(*index, request))
            .collect()
    }
    fn execution_bounds_on(
        &self,
        index: usize,
        request: &ChatRequest,
    ) -> Option<ProviderExecutionBounds> {
        let observation = self.endpoints.get(index)?;
        let (_, shape) = self.body_for(observation, request).ok()?;
        observation.call_bounds(shape, self.max_response_bytes).ok()
    }
    fn describe_move(&self, from: usize, to: usize) -> String {
        let name = |index: usize| {
            self.endpoints
                .get(index)
                .map(failover::describe)
                .unwrap_or_else(|| format!("{index}"))
        };
        format!("from endpoint {} to endpoint {}", name(from), name(to))
    }
    fn move_to(&self, to: usize, failed: bool) {
        let from = self.active();
        if to == from || to >= self.endpoints.len() {
            return;
        }
        self.active.store(to, Ordering::Release);
        if let (true, Some(served)) = (failed, &self.served) {
            served.moved_from(&self.endpoints[from]);
        }
    }
    fn exhausted(&self, failures: Vec<(usize, FailedCall)>) -> ProviderError {
        failover::exhausted(&self.endpoints, failures)
    }
}

/// The reasoning tokens a request sends back: what its tool-calling turns
/// reported. The provider can bill them again as input.
fn replayed_reasoning_tokens(request: &ChatRequest) -> usize {
    request
        .messages
        .iter()
        .filter_map(|message| {
            message.tool_calls.iter().find_map(|call| {
                call.provider_metadata
                    .get(REASONING_TOKENS_METADATA)
                    .and_then(|tokens| tokens.parse::<usize>().ok())
            })
        })
        .fold(0, usize::saturating_add)
}

/// Send each tool-calling assistant turn's reasoning back unmodified, as
/// OpenRouter documents for tool use, and return the reasoning tokens those
/// turns reported. Replayed reasoning can be billed as input at its full
/// reasoning length, which a summary's bytes do not bound.
fn replay_reasoning(request: &ChatRequest, body: &mut Value) -> Result<usize, ProviderError> {
    let mut replayed = 0usize;
    let messages = body["messages"]
        .as_array_mut()
        .filter(|messages| messages.len() == request.messages.len())
        .ok_or_else(|| invalid("request messages do not map one to one"))?;
    for (message, wire) in request.messages.iter().zip(messages) {
        let mut details = None;
        let mut tokens = None;
        for call in &message.tool_calls {
            for (key, slot) in [
                (REASONING_DETAILS_METADATA, &mut details),
                (REASONING_TOKENS_METADATA, &mut tokens),
            ] {
                if let Some(value) = call.provider_metadata.get(key) {
                    if slot.is_some_and(|previous: &String| previous != value) {
                        return Err(invalid("one assistant turn carries conflicting reasoning"));
                    }
                    *slot = Some(value);
                }
            }
        }
        let Some(details) = details else {
            if tokens.is_some() {
                return Err(invalid("replayed reasoning tokens lack their reasoning"));
            }
            continue;
        };
        let tokens = tokens
            .and_then(|tokens| tokens.parse::<usize>().ok())
            .ok_or_else(|| invalid("replayed reasoning lacks its recorded token count"))?;
        let details: Value = serde_json::from_str(details)
            .map_err(|_| invalid("replayed reasoning is not valid JSON"))?;
        if !details.as_array().is_some_and(|details| {
            details.iter().all(|detail| {
                detail["type"]
                    .as_str()
                    .is_some_and(stream::replayable_reasoning)
            })
        }) {
            return Err(invalid("replayed reasoning has an unknown shape"));
        }
        wire["reasoning_details"] = details;
        replayed = replayed.saturating_add(tokens);
    }
    Ok(replayed)
}

#[async_trait::async_trait]
impl LlmProvider for NativeOpenRouterProvider {
    fn stream_timeouts(&self) -> Option<StreamTimeouts> {
        Some(self.inner.stream_timeouts)
    }

    fn provider_id(&self) -> &str {
        PROVIDER
    }
    fn model_id(&self) -> &str {
        &self.primary().model
    }
    /// The selected endpoint's: every endpoint retained for failover holds
    /// at least its context and prompt limit, so a request fitted to it fits
    /// each of them.
    fn capabilities(&self) -> ProviderCapabilities {
        let primary = self.primary();
        // Context fitting reserves the visible output; leave the reasoning
        // allowance and the endpoint's own prompt limit out of the window.
        let response = primary.response_allowance(self.max_output_tokens, self.reasoning);
        let reasoning = response - self.max_output_tokens;
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            structured_output: primary
                .supported_parameters
                .iter()
                .any(|p| p == "response_format"),
            vision: false,
            reasoning: reasoning > 0,
            embeddings: false,
            max_context_tokens: (primary.context_tokens.saturating_sub(reasoning)).min(
                primary
                    .prompt_limit()
                    .saturating_add(self.max_output_tokens),
            ),
            max_output_tokens: self.max_output_tokens,
        }
    }
    fn model_constraints_known(&self, _: &ChatRequest) -> bool {
        true
    }
    fn validate_request(&self, request: &ChatRequest) -> Result<(), ProviderError> {
        self.validate_request_for(self.endpoint(), request)
    }
    fn execution_bounds(&self, request: &ChatRequest) -> Option<ProviderExecutionBounds> {
        let (_, shape) = self.body(request).ok()?;
        self.endpoint()
            .call_bounds(shape, self.max_response_bytes)
            .ok()
    }
    fn follow_up_execution_bounds(
        &self,
        request: &ChatRequest,
        added_prompt_tokens: u64,
    ) -> Option<ProviderExecutionBounds> {
        let endpoint = self.endpoint();
        let (_, mut shape) = self.body_for(endpoint, request).ok()?;
        shape.prompt_tokens = shape
            .prompt_tokens
            .saturating_add(usize::try_from(added_prompt_tokens).unwrap_or(usize::MAX))
            .min(endpoint.prompt_limit());
        endpoint.call_bounds(shape, self.max_response_bytes).ok()
    }
    fn response_tokens(&self, _request: &ChatRequest, output: usize) -> usize {
        self.endpoint().response_allowance(output, self.reasoning)
    }
    fn endpoint_failover(&self) -> Option<&dyn EndpointFailover> {
        (self.endpoints.len() > 1).then_some(self as &dyn EndpointFailover)
    }
    fn failed_answer(&self, error: ProviderError) -> FailedCall {
        failover::answer_failure(error)
    }
    /// The shared estimate plus the reasoning tokens the request replays,
    /// which its text does not show.
    fn count_tokens(&self, request: &ChatRequest) -> usize {
        axocoatl_llm::approximate_request_tokens(request)
            .saturating_add(replayed_reasoning_tokens(request))
    }
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        self.chat_with_accounting(request).await.response
    }
    async fn chat_with_accounting(&self, request: ChatRequest) -> AccountedChatOutcome {
        stream::collect(self, request).await.0
    }
    async fn chat_with_accounting_and_facts(
        &self,
        request: ChatRequest,
    ) -> (AccountedChatOutcome, FailureFacts) {
        stream::collect(self, request).await
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        self.open(request).await.map_err(|failed| failed.error)
    }
    async fn open_chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, FailedCall>
    {
        self.open(request).await
    }
}

impl NativeOpenRouterProvider {
    /// Send `request` to the current endpoint and open its answer. A
    /// failure says what is known of it where it happened: nothing sent (a
    /// request the endpoint's contract refuses, a catalog that cannot be
    /// read, an endpoint reported unavailable now or whose contract changed),
    /// a transport failure (after the request may have left, unless no
    /// connection was made), or OpenRouter's refusal with a status.
    async fn open(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, FailedCall>
    {
        // One endpoint for the whole call, whatever moves later calls.
        let observation = self.endpoint().clone();
        let refused = |error| failover::unsent(error, false, "");
        let (body, shape) = self.body_for(&observation, &request).map_err(refused)?;
        let bounds = observation
            .call_bounds(shape, self.max_response_bytes)
            .map_err(refused)?;
        self.verify(&observation).await?;
        let url = self.inner.endpoint().map_err(refused)?;
        let deadlines = StreamDeadlines::start(self.inner.stream_timeouts);
        let response = deadlines
            .headers(
                self.inner
                    .client
                    .post(url)
                    .bearer_auth(&self.inner.api_key)
                    .header("HTTP-Referer", "https://axocoatl.ai")
                    .header("X-OpenRouter-Title", "Axocoatl")
                    .json(&body)
                    .send(),
                PROVIDER,
            )
            .await
            // The answer did not start in time.
            .map_err(|error| failover::transport(error, false, true))?
            .map_err(|error| {
                failover::transport(
                    transport_error(&error, &[&self.inner.api_key]),
                    error.is_connect(),
                    error.is_timeout() || connection_reset(&error),
                )
            })?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let retry_after = retry_after_secs(response.headers());
            let detail = read_error_text(response, &[&self.inner.api_key]).await;
            return Err(failover::refusal(
                &observation,
                status,
                &detail,
                retry_after,
            ));
        }
        Ok(stream::decode(
            response,
            deadlines,
            request,
            observation,
            shape,
            bounds,
            self.inner.api_key.clone(),
            self.served.clone(),
        ))
    }
}

#[cfg(test)]
#[path = "native_openrouter/tests.rs"]
mod tests;
