//! Exact-activation provider admission, bounded observation, and settlement.
//!
//! Every provider call, including compaction, crosses this adapter. Executor
//! capabilities must establish actual total spend limits; request estimates do
//! not grant authority. A dropped call retains incomplete observed usage and
//! its conservative charge. No provider completion escapes before settlement.
//!
//! A transient failure (429, 5xx, timeout, connection reset) that arrives
//! before the provider produced anything is retried once, after the
//! provider's `Retry-After` (at most 30 s) or 2 s, on the same executor, so
//! the same pinned model and endpoint ([`crate::provider_retry`]). The
//! failed call keeps its own settlement: an incomplete call keeps its whole
//! reservation. The retry is a new call admitted against the grant like any
//! other; when the grant cannot pay for it the original failure stands. Each
//! retry is recorded on the activation's stream before it is sent.

use std::io::{self, Write};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use axocoatl_core::{MeasuredTokenUsage, TokenUsageStats};
use axocoatl_llm::{
    AccountedChatOutcome, ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderAllowance,
    ProviderCapabilities, ProviderError, ProviderExecutionBounds, StreamEvent,
};
use axocoatl_session::control_authority::{
    ProviderCallClaim, ProviderCallOutcome, ProviderCallTerminal,
};
use axocoatl_session::turn_contract::ActivationRef;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio_stream::{Stream, StreamExt};

use crate::provider_retry::{self, ProviderFailure, RetryDecision};
use crate::session_dispatch::SessionDispatchController;

#[path = "session_dispatch_provider_framing.rs"]
mod framing;

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

pub(crate) struct SessionProvider {
    controller: SessionDispatchController,
    activation: ActivationRef,
    inner: Arc<dyn LlmProvider>,
    expected_provider: String,
    expected_model: String,
    /// The activation's own stream observer, where each retry is recorded.
    retry_observer: Option<Arc<dyn axocoatl_actor::AgentStreamObserver>>,
}

/// One attempt's outcome: what it produced, or a provider failure whose call
/// is already settled and which the policy may retry. An error that ends the
/// whole call (a settlement failure or a boundary violation) is returned as
/// the attempt's own `Err` instead.
enum Attempt<T> {
    Done(T),
    Failed(ProviderError),
}

impl SessionProvider {
    pub(crate) fn new(
        controller: SessionDispatchController,
        activation: ActivationRef,
        inner: Arc<dyn LlmProvider>,
        expected_provider: String,
        expected_model: String,
    ) -> Self {
        Self {
            controller,
            activation,
            inner,
            expected_provider,
            expected_model,
            retry_observer: None,
        }
    }

    /// Record each provider retry on `observer`, the activation's own
    /// stream observer, as a `provider_retry` observation.
    pub(crate) fn with_retry_observer(
        mut self,
        observer: Arc<dyn axocoatl_actor::AgentStreamObserver>,
    ) -> Self {
        self.retry_observer = Some(observer);
        self
    }

    /// How long to wait before retrying `error` after attempt `attempt`, or
    /// `None` when the policy gives up. A retry that cannot be recorded on
    /// the activation is not made.
    fn retry_wait(&self, error: &ProviderError, attempt: u32) -> Option<std::time::Duration> {
        let failure = provider_retry::failure_of(error);
        let RetryDecision::RetryAfter(wait) = provider_retry::decide(&failure, attempt) else {
            return None;
        };
        self.record_retry(&failure, wait).then_some(wait)
    }

    fn record_retry(&self, failure: &ProviderFailure, wait: std::time::Duration) -> bool {
        let note = provider_retry::retry_note(
            failure,
            wait,
            &self.expected_provider,
            &self.expected_model,
        );
        match &self.retry_observer {
            Some(observer) => match observer
                .observe(&axocoatl_actor::AgentStreamChunk::ProviderRetry { reason: note })
            {
                Ok(()) => true,
                Err(reason) => {
                    tracing::warn!(
                        activation = %self.activation.activation_id.as_str(),
                        %reason,
                        "a provider retry could not be recorded; the failure stands"
                    );
                    false
                }
            },
            None => {
                tracing::info!(
                    activation = %self.activation.activation_id.as_str(),
                    %note,
                    "provider retry without an activation stream observer"
                );
                true
            }
        }
    }

    /// Admit the retry of a failed call. When the grant cannot pay for it,
    /// or the activation may no longer call, the original failure stands.
    fn admit_retry(&self, request: &ChatRequest, failure: &ProviderError) -> Option<PendingCall> {
        match self.admit(request) {
            Ok(pending) => Some(pending),
            Err(refused) => {
                tracing::info!(
                    activation = %self.activation.activation_id.as_str(),
                    %refused,
                    %failure,
                    "the provider retry was not admitted; the first failure stands"
                );
                None
            }
        }
    }

    /// One non-streaming call under `pending`, settled exactly as 1.2
    /// settles it. `Failed` carries a provider failure whose call settled,
    /// which may be retried; any other error ends the call.
    async fn accounted_once(
        &self,
        mut pending: PendingCall,
        request: ChatRequest,
        usage: &mut MeasuredTokenUsage,
        cost_microunits: &mut Option<u64>,
    ) -> Result<Attempt<ChatResponse>, ProviderError> {
        let outcome = self.inner.chat_with_accounting(request).await;
        *usage = outcome.usage.clone();
        *cost_microunits = outcome.cost_microunits;
        // Retain both independently authoritative dimensions before refusing
        // either boundary, including a response that exceeds both bounds.
        let usage_result = pending.observe_usage(outcome.usage);
        let cost_result = outcome
            .cost_microunits
            .map(|cost| pending.observe_cost(cost))
            .transpose();
        if let Err(error) = usage_result.and(cost_result.map(|_| ())) {
            *usage = pending.observed_usage(false);
            return Err(pending.violation(error));
        }
        let response = match outcome.response {
            Ok(response) => response,
            Err(error) => {
                // A parser may have received usage before the body became
                // invalid. Preserve its subtotal without treating failure
                // as proof of a complete provider exchange.
                *usage = pending.observed_usage(false);
                pending.finish(ProviderCallTerminal::Failed, false)?;
                return Ok(Attempt::Failed(error));
            }
        };
        if response.usage != usage.usage {
            // Conflicting evidence is not permission to discard the larger
            // observed dimension, nor to release a successful response.
            pending.retain_highwater(&response.usage);
            *usage = pending.observed_usage(false);
            return Err(
                pending.violation("provider response and accounting observations disagree".into())
            );
        }
        if let Err(error) = pending.observe_response(&response) {
            *usage = pending.observed_usage(false);
            return Err(pending.violation(error));
        }
        let failed = matches!(
            response.finish_reason,
            FinishReason::Error | FinishReason::ContentFilter
        );
        pending.finish(
            if failed {
                ProviderCallTerminal::Failed
            } else {
                ProviderCallTerminal::Completed
            },
            true,
        )?;
        if failed {
            return Err(failed_completion(
                &self.expected_provider,
                &response.finish_reason,
            ));
        }
        pending.note_follow_up();
        Ok(Attempt::Done(response))
    }

    /// Open one stream under `pending`. While a retry is still allowed
    /// (`attempt` below the policy's limit), the stream's first item is read
    /// before it is handed on, so a transient failure before the provider
    /// produced anything can be retried; that failure is settled exactly as
    /// the stream would have settled it. Any other first item is handed on
    /// unchanged, and the last attempt's stream is handed on unread.
    async fn stream_once(
        &self,
        mut pending: PendingCall,
        request: ChatRequest,
        attempt: u32,
    ) -> Result<Attempt<ProviderStream>, ProviderError> {
        let mut inner = match self.inner.chat_stream(request).await {
            Ok(stream) => stream,
            Err(error) => {
                pending.finish(ProviderCallTerminal::Failed, false)?;
                return Ok(Attempt::Failed(error));
            }
        };
        let retryable = |error: &ProviderError| {
            matches!(
                provider_retry::decide(&provider_retry::failure_of(error), attempt),
                RetryDecision::RetryAfter(_)
            )
        };
        let first = if attempt < provider_retry::MAX_PROVIDER_RETRIES {
            match inner.next().await {
                Some(Err(error)) if retryable(&error) => {
                    // Nothing was observed before it, so no terminal usage was
                    // reported: the whole reservation stays, as 1.2 keeps it.
                    pending.finish(ProviderCallTerminal::Interrupted, false)?;
                    return Ok(Attempt::Failed(error));
                }
                first => Some(first),
            }
        } else {
            None
        };
        Ok(Attempt::Done(Box::pin(framing::FramedProviderStream::new(
            Box::pin(SessionProviderStream {
                inner,
                pending,
                finished: false,
                first,
                provider: self.expected_provider.clone(),
            }),
        ))))
    }

    fn admit(&self, request: &ChatRequest) -> Result<PendingCall, ProviderError> {
        self.validate_request(request)?;
        let bounds = self.inner.execution_bounds(request).ok_or_else(|| {
            self.invalid("executor does not provide enforced provider spend and response bounds")
        })?;
        if bounds.response_bytes == 0 || bounds.response_bytes > MAX_RESPONSE_BYTES {
            return Err(self.invalid("executor response bound exceeds reserved provider capacity"));
        }
        let (digest, size) = request_identity(request)
            .map_err(|error| self.invalid(format!("request identity failed: {error}")))?;
        let claim = self
            .controller
            .admit_provider(&self.activation, digest, size as u64, bounds)
            .map_err(|error| self.invalid(format!("provider admission failed: {error}")))?
            .map_err(|message| ProviderError::BudgetExhausted {
                provider: self.expected_provider.clone(),
                message,
            })?;
        Ok(PendingCall {
            controller: self.controller.clone(),
            claim: Some(claim),
            bounds,
            observed: None,
            terminal_usage_reported: false,
            observed_cost: None,
            bytes: 0,
            expected_provider: self.expected_provider.clone(),
            expected_model: self.expected_model.clone(),
            follow_up: Some((self.inner.clone(), request.clone())),
            activation: self.activation.clone(),
            tool_calls: false,
        })
    }

    fn invalid(&self, message: impl Into<String>) -> ProviderError {
        ProviderError::InvalidRequest {
            provider: self.expected_provider.clone(),
            message: message.into(),
        }
    }
}

#[async_trait]
impl LlmProvider for SessionProvider {
    fn provider_id(&self) -> &str {
        &self.expected_provider
    }
    fn model_id(&self) -> &str {
        &self.expected_model
    }
    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }
    fn capabilities_for(&self, request: &ChatRequest) -> ProviderCapabilities {
        self.inner.capabilities_for(request)
    }
    fn model_constraints_known(&self, request: &ChatRequest) -> bool {
        self.inner.model_constraints_known(request)
    }
    fn count_tokens(&self, request: &ChatRequest) -> usize {
        self.inner.count_tokens(request)
    }
    fn execution_bounds(&self, request: &ChatRequest) -> Option<ProviderExecutionBounds> {
        self.inner.execution_bounds(request)
    }
    fn follow_up_execution_bounds(
        &self,
        request: &ChatRequest,
        added_prompt_tokens: u64,
    ) -> Option<ProviderExecutionBounds> {
        self.inner
            .follow_up_execution_bounds(request, added_prompt_tokens)
    }
    fn response_tokens(&self, request: &ChatRequest, output: usize) -> usize {
        self.inner.response_tokens(request, output)
    }
    fn remaining_allowance(&self) -> Option<ProviderAllowance> {
        self.controller.agent_allowance(&self.activation)
    }
    fn validate_request(&self, request: &ChatRequest) -> Result<(), ProviderError> {
        if self.inner.provider_id() != self.expected_provider
            || self.inner.model_id() != self.expected_model
            || request
                .model_override
                .as_ref()
                .is_some_and(|model| model != &self.expected_model)
        {
            return Err(self.invalid("provider or model differs from the exact activation profile"));
        }
        axocoatl_llm::validate_provider_request(request, &self.expected_provider)?;
        self.inner.validate_request(request)
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        self.chat_with_accounting(request).await.response
    }

    async fn chat_with_accounting(&self, request: ChatRequest) -> AccountedChatOutcome {
        let mut usage = MeasuredTokenUsage::lower_bound(TokenUsageStats::default());
        let mut cost_microunits = None;
        let response = async {
            let mut pending = self.admit(&request)?;
            let mut attempt = 0;
            loop {
                let error = match self
                    .accounted_once(pending, request.clone(), &mut usage, &mut cost_microunits)
                    .await?
                {
                    Attempt::Done(response) => return Ok(response),
                    Attempt::Failed(error) => error,
                };
                let Some(wait) = self.retry_wait(&error, attempt) else {
                    return Err(error);
                };
                tokio::time::sleep(wait).await;
                attempt += 1;
                pending = match self.admit_retry(&request, &error) {
                    Some(pending) => pending,
                    None => return Err(error),
                };
            }
        }
        .await;
        AccountedChatOutcome {
            response,
            usage,
            cost_microunits,
        }
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ProviderStream, ProviderError> {
        let mut pending = self.admit(&request)?;
        let mut attempt = 0;
        loop {
            let error = match self.stream_once(pending, request.clone(), attempt).await? {
                Attempt::Done(stream) => return Ok(stream),
                Attempt::Failed(error) => error,
            };
            let Some(wait) = self.retry_wait(&error, attempt) else {
                return Err(error);
            };
            tokio::time::sleep(wait).await;
            attempt += 1;
            pending = match self.admit_retry(&request, &error) {
                Some(pending) => pending,
                None => return Err(error),
            };
        }
    }
}

type ProviderStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>;

/// The error for a completion the provider ended as failed: a refusal or a
/// safety stop (`content_filter`) is named as one, so the activation's
/// recorded failure says the provider refused.
fn failed_completion(provider: &str, finish_reason: &FinishReason) -> ProviderError {
    match finish_reason {
        FinishReason::ContentFilter => ProviderError::ContentFiltered {
            provider: provider.to_owned(),
            reason: "the provider stopped the response with a content filter or safety \
                     classifier (finish reason content_filter); nothing from it ran"
                .into(),
        },
        _ => ProviderError::Stream("provider reported a failed completion".into()),
    }
}

struct PendingCall {
    controller: SessionDispatchController,
    claim: Option<ProviderCallClaim>,
    bounds: ProviderExecutionBounds,
    observed: Option<MeasuredTokenUsage>,
    /// The latest explicit usage observation was complete: the provider
    /// reported its terminal accounting for this response.
    terminal_usage_reported: bool,
    observed_cost: Option<u64>,
    bytes: usize,
    expected_provider: String,
    expected_model: String,
    /// The executor and the exact request, to estimate the call that reads
    /// this one's tool results.
    follow_up: Option<(Arc<dyn LlmProvider>, ChatRequest)>,
    activation: ActivationRef,
    /// The response asked for at least one tool.
    tool_calls: bool,
}

impl PendingCall {
    /// After a completed call that asked for tools: what the call reading
    /// their results, a helper's answer among them, can be expected to
    /// reserve. This call's reasoning is sent back, and its returned bytes
    /// join that call's prompt as the assistant turn.
    fn note_follow_up(&mut self) {
        let Some((provider, request)) = self.follow_up.take() else {
            return;
        };
        if !self.tool_calls {
            return;
        }
        let reasoning = self
            .observed
            .as_ref()
            .and_then(|usage| usage.usage.reasoning_tokens)
            .unwrap_or(0) as u64;
        let growth = reasoning
            .saturating_add(self.bytes as u64)
            .saturating_add(crate::session_dispatch::delegate::ANSWER_PROMPT_TOKENS);
        if let Some(bounds) = provider.follow_up_execution_bounds(&request, growth) {
            self.controller.note_follow_up(
                &self.activation,
                axocoatl_session::control_authority::DispatchReservation {
                    tokens: bounds.token_limit,
                    cost_microunits: bounds.cost_microunits,
                },
            );
        }
    }

    fn finish(&mut self, kind: ProviderCallTerminal, terminal: bool) -> Result<(), ProviderError> {
        let Some(claim) = self.claim.take() else {
            return Err(ProviderError::Stream(
                "provider call was already settled".into(),
            ));
        };
        let usage = self.observed_usage(terminal);
        // A valid enforced zero ceiling proves zero provider inference API
        // charge even on interruption. It is not an invoice observation or a
        // claim about hardware/electricity cost. Positive ceilings prove no
        // particular actual charge. A terminal authoritative observation can
        // establish it independently without refunding admission charges.
        let known_zero_api_charge = self.bounds.cost_microunits == 0;
        let outcome = ProviderCallOutcome {
            kind,
            usage,
            cost_microunits: self
                .observed_cost
                .or_else(|| known_zero_api_charge.then_some(0)),
            cost_known: known_zero_api_charge || (terminal && self.observed_cost.is_some()),
        };
        self.controller
            .settle_provider(&claim, &outcome)
            .map_err(|error| {
                let reason = format!("provider settlement failed: {error}");
                self.controller.provider_boundary_failed(reason.clone());
                ProviderError::Stream(reason)
            })
    }

    fn violation(&mut self, reason: String) -> ProviderError {
        // Preserve authoritative observations, including an overrun, before
        // refusing further execution. An invalid response is never completion.
        let settled = self.finish(ProviderCallTerminal::Interrupted, false);
        self.controller.provider_boundary_failed(reason.clone());
        match settled {
            Ok(()) => ProviderError::Stream(reason),
            Err(error) => error,
        }
    }

    fn observe_cost(&mut self, cost: u64) -> Result<(), String> {
        let decreased = self.observed_cost.is_some_and(|previous| cost < previous);
        self.observed_cost = Some(cost.max(self.observed_cost.unwrap_or(0)));
        if cost > self.bounds.cost_microunits {
            return Err("provider exceeded its enforced monetary bound".into());
        }
        if decreased {
            return Err("provider reported decreasing cumulative cost".into());
        }
        Ok(())
    }

    fn check_usage_bound(&self) -> Result<(), String> {
        if let Some(usage) = &self.observed {
            let usage = &usage.usage;
            let total = (usage.input_tokens as u128)
                .saturating_add(usage.output_tokens as u128)
                .saturating_add(usage.reasoning_tokens.unwrap_or(0) as u128);
            if total > self.bounds.token_limit as u128 {
                return Err("provider exceeded its enforced token bound".into());
            }
        }
        Ok(())
    }

    fn observed_usage(&self, terminal: bool) -> MeasuredTokenUsage {
        let mut usage = self
            .observed
            .clone()
            .unwrap_or_else(|| MeasuredTokenUsage::lower_bound(TokenUsageStats::default()));
        usage.complete &= terminal;
        usage
    }

    fn retain_highwater(&mut self, usage: &TokenUsageStats) {
        let previous = self.observed_usage(false).usage;
        self.observed = Some(MeasuredTokenUsage::lower_bound(TokenUsageStats {
            input_tokens: usage.input_tokens.max(previous.input_tokens),
            output_tokens: usage.output_tokens.max(previous.output_tokens),
            reasoning_tokens: match (usage.reasoning_tokens, previous.reasoning_tokens) {
                (None, None) => None,
                (left, right) => Some(left.unwrap_or(0).max(right.unwrap_or(0))),
            },
        }));
    }

    fn observe_usage(&mut self, usage: MeasuredTokenUsage) -> Result<(), String> {
        // Both usage event forms are cumulative snapshots, not paid deltas.
        if let Some(previous) = &self.observed {
            if usage.usage.input_tokens < previous.usage.input_tokens
                || usage.usage.output_tokens < previous.usage.output_tokens
                || usage.usage.reasoning_tokens.unwrap_or(0)
                    < previous.usage.reasoning_tokens.unwrap_or(0)
            {
                self.retain_highwater(&usage.usage);
                return Err("provider reported decreasing or conflicting cumulative usage".into());
            }
        }
        self.observed = Some(usage);
        self.check_usage_bound()
    }

    fn check_route(&self, provider: Option<&str>, model: Option<&str>) -> Result<(), String> {
        if provider.is_some_and(|value| value != self.expected_provider)
            || model.is_some_and(|value| value != self.expected_model)
        {
            return Err("provider response changed the admitted execution route".into());
        }
        Ok(())
    }

    fn check_metadata_route(
        &self,
        metadata: &axocoatl_core::ProviderMetadata,
    ) -> Result<(), String> {
        self.check_route(
            metadata
                .get(axocoatl_llm::TOOL_METADATA_PROVIDER_ID)
                .map(String::as_str),
            None,
        )?;
        self.check_route(
            metadata
                .get(axocoatl_llm::TOOL_METADATA_ROUTE_PROVIDER)
                .map(String::as_str),
            metadata
                .get(axocoatl_llm::TOOL_METADATA_ROUTE_MODEL)
                .map(String::as_str),
        )
    }

    fn observe_response(&mut self, response: &ChatResponse) -> Result<(), String> {
        self.tool_calls = !response.tool_calls.is_empty();
        self.check_usage_bound()?;
        self.check_route(
            (!response.provider.is_empty()).then_some(response.provider.as_str()),
            (!response.model.is_empty()).then_some(response.model.as_str()),
        )?;
        for call in &response.tool_calls {
            self.check_metadata_route(&call.provider_metadata)?;
        }
        #[derive(Serialize)]
        struct Response<'a> {
            content: &'a str,
            tool_calls: &'a [axocoatl_llm::ToolCall],
            finish_reason: &'a FinishReason,
            usage: &'a TokenUsageStats,
            model: &'a str,
            provider: &'a str,
        }
        let ChatResponse {
            content,
            tool_calls,
            finish_reason,
            usage,
            model,
            provider,
        } = response;
        let mut writer = BoundedDigest::new(self.bounds.response_bytes);
        serde_json::to_writer(
            &mut writer,
            &Response {
                content,
                tool_calls,
                finish_reason,
                usage,
                model,
                provider,
            },
        )
        .map_err(|error| format!("provider response exceeded its encoded bound: {error}"))?;
        self.bytes = writer.bytes;
        Ok(())
    }

    fn observe_event(&mut self, event: &StreamEvent) -> Result<(), String> {
        match event {
            StreamEvent::Usage(usage) => {
                self.observe_usage(MeasuredTokenUsage::known(usage.clone()))?;
            }
            StreamEvent::UsageObservation(usage) => {
                self.terminal_usage_reported = false;
                self.observe_usage(usage.clone())?;
                self.terminal_usage_reported = usage.complete;
            }
            StreamEvent::CostObservation { cost_microunits } => {
                self.observe_cost(*cost_microunits)?
            }
            _ => {}
        }
        if matches!(event, StreamEvent::ToolCallDelta { .. }) {
            self.tool_calls = true;
        }
        let metadata = match event {
            StreamEvent::ProviderRoute { metadata }
            | StreamEvent::ToolCallMetadata { metadata, .. } => Some(metadata),
            _ => None,
        };
        if let Some(metadata) = metadata {
            self.check_metadata_route(metadata)?;
        }
        let mut writer = BoundedDigest::new(self.bounds.response_bytes.saturating_sub(self.bytes));
        encode_event(&mut writer, event)
            .map_err(|error| format!("provider stream exceeded its encoded bound: {error}"))?;
        self.bytes = self
            .bytes
            .checked_add(writer.bytes)
            .ok_or_else(|| "provider response byte count overflow".to_string())?;
        Ok(())
    }
}

impl Drop for PendingCall {
    fn drop(&mut self) {
        if self.claim.is_some() {
            // The controller API is synchronous and bounded. There is no
            // detached settlement task that can disappear with the executor.
            if let Err(error) = self.finish(ProviderCallTerminal::Interrupted, false) {
                self.controller.provider_boundary_failed(error.to_string());
            }
        }
    }
}

struct SessionProviderStream {
    inner: Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
    pending: PendingCall,
    finished: bool,
    /// The first item, when it was read before the stream was handed on.
    first: Option<Option<Result<StreamEvent, ProviderError>>>,
    provider: String,
}

impl Stream for SessionProviderStream {
    type Item = Result<StreamEvent, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        let polled = match this.first.take() {
            Some(first) => Poll::Ready(first),
            None => this.inner.as_mut().poll_next(context),
        };
        let event = match polled {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Some(Ok(event))) => event,
            Poll::Ready(Some(Err(error))) => {
                this.finished = true;
                // A response the adapter refused after the provider completed
                // it and reported its usage (a malformed tool call) settles to
                // that usage. Any other failure keeps the whole reservation.
                let refused = matches!(error, ProviderError::RefusedResponse { .. });
                let settled = if refused && this.pending.terminal_usage_reported {
                    this.pending.finish(ProviderCallTerminal::Failed, true)
                } else {
                    this.pending
                        .finish(ProviderCallTerminal::Interrupted, false)
                };
                return Poll::Ready(Some(match settled {
                    Ok(()) => Err(error),
                    Err(error) => Err(error),
                }));
            }
            Poll::Ready(None) => {
                this.finished = true;
                let settled = this
                    .pending
                    .finish(ProviderCallTerminal::Interrupted, false);
                return Poll::Ready(Some(Err(settled.err().unwrap_or_else(|| {
                    ProviderError::Stream("provider stream ended without durable completion".into())
                }))));
            }
        };
        if let Err(error) = this.pending.observe_event(&event) {
            this.finished = true;
            return Poll::Ready(Some(Err(this.pending.violation(error))));
        }
        if let StreamEvent::Done { finish_reason } = &event {
            this.finished = true;
            let failed = matches!(
                finish_reason,
                FinishReason::Error | FinishReason::ContentFilter
            );
            if let Err(error) = this.pending.finish(
                if failed {
                    ProviderCallTerminal::Failed
                } else {
                    ProviderCallTerminal::Completed
                },
                true,
            ) {
                return Poll::Ready(Some(Err(error)));
            }
            if failed {
                return Poll::Ready(Some(Err(failed_completion(&this.provider, finish_reason))));
            }
            this.pending.note_follow_up();
        }
        Poll::Ready(Some(Ok(event)))
    }
}

struct BoundedDigest {
    digest: Sha256,
    bytes: usize,
    maximum: usize,
}

impl BoundedDigest {
    fn new(maximum: usize) -> Self {
        Self {
            digest: Sha256::new(),
            bytes: 0,
            maximum,
        }
    }
}
impl Write for BoundedDigest {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.maximum.saturating_sub(self.bytes) {
            return Err(io::Error::other(
                "encoded provider data exceeds reserved capacity",
            ));
        }
        self.digest.update(bytes);
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn request_identity(request: &ChatRequest) -> Result<(String, usize), serde_json::Error> {
    // Exhaustive destructuring forces new request fields to join the identity.
    let ChatRequest {
        messages,
        tools,
        max_tokens,
        temperature,
        top_p,
        response_format,
        stop_sequences,
        provider_options,
        model_override,
    } = request;
    #[derive(Serialize)]
    struct Request<'a> {
        messages: &'a [axocoatl_core::ChatMessage],
        tools: &'a [axocoatl_llm::ToolDefinition],
        max_tokens: &'a Option<usize>,
        temperature: &'a Option<f32>,
        top_p: &'a Option<f32>,
        response_format: &'a Option<axocoatl_core::ResponseFormat>,
        stop_sequences: &'a [String],
        provider_options: &'a Option<serde_json::Value>,
        model_override: &'a Option<String>,
    }
    let mut writer = BoundedDigest::new(MAX_REQUEST_BYTES);
    serde_json::to_writer(
        &mut writer,
        &Request {
            messages,
            tools,
            max_tokens,
            temperature,
            top_p,
            response_format,
            stop_sequences,
            provider_options,
            model_override,
        },
    )?;
    Ok((format!("{:x}", writer.digest.finalize()), writer.bytes))
}

fn encode_event(writer: &mut impl Write, event: &StreamEvent) -> Result<(), serde_json::Error> {
    #[derive(Serialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum Event<'a> {
        ProviderRoute {
            metadata: &'a axocoatl_core::ProviderMetadata,
        },
        TextDelta {
            delta: &'a str,
        },
        ReasoningDelta {
            delta: &'a str,
        },
        ToolCallDelta {
            index: &'a Option<usize>,
            id: &'a str,
            name: &'a Option<String>,
            args_delta: &'a str,
        },
        ToolCallMetadata {
            index: &'a Option<usize>,
            id: &'a str,
            metadata: &'a axocoatl_core::ProviderMetadata,
        },
        Usage {
            usage: &'a TokenUsageStats,
        },
        UsageObservation {
            usage: &'a MeasuredTokenUsage,
        },
        CostObservation {
            cost_microunits: &'a u64,
        },
        Done {
            finish_reason: &'a FinishReason,
        },
    }
    let event = match event {
        StreamEvent::ProviderRoute { metadata } => Event::ProviderRoute { metadata },
        StreamEvent::TextDelta { delta } => Event::TextDelta { delta },
        StreamEvent::ReasoningDelta { delta } => Event::ReasoningDelta { delta },
        StreamEvent::ToolCallDelta {
            index,
            id,
            name,
            args_delta,
        } => Event::ToolCallDelta {
            index,
            id,
            name,
            args_delta,
        },
        StreamEvent::ToolCallMetadata {
            index,
            id,
            metadata,
        } => Event::ToolCallMetadata {
            index,
            id,
            metadata,
        },
        StreamEvent::Usage(usage) => Event::Usage { usage },
        StreamEvent::UsageObservation(usage) => Event::UsageObservation { usage },
        StreamEvent::CostObservation { cost_microunits } => {
            Event::CostObservation { cost_microunits }
        }
        StreamEvent::Done { finish_reason } => Event::Done { finish_reason },
    };
    serde_json::to_writer(writer, &event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_request_identity_covers_every_request_field_and_tool_metadata() {
        let base = ChatRequest::simple("input");
        let original = request_identity(&base).unwrap();
        for field in [
            "messages",
            "tools",
            "max_tokens",
            "temperature",
            "top_p",
            "response_format",
            "stop_sequences",
            "provider_options",
            "model_override",
            "native_metadata",
        ] {
            let mut changed = base.clone();
            match field {
                "messages" => changed
                    .messages
                    .push(axocoatl_core::ChatMessage::assistant("history")),
                "tools" => changed.tools.push(axocoatl_llm::ToolDefinition {
                    name: "effect".into(),
                    description: "exact tool".into(),
                    parameters: serde_json::json!({"type":"object"}),
                    concurrency: Default::default(),
                }),
                "max_tokens" => changed.max_tokens = Some(1),
                "temperature" => changed.temperature = Some(0.0),
                "top_p" => changed.top_p = Some(0.5),
                "response_format" => {
                    changed.response_format = Some(axocoatl_core::ResponseFormat::Json)
                }
                "stop_sequences" => changed.stop_sequences.push("stop".into()),
                "provider_options" => {
                    changed.provider_options = Some(serde_json::json!({"reasoning":"high"}))
                }
                "model_override" => changed.model_override = Some("exact-model".into()),
                "native_metadata" => changed.messages[0].tool_calls.push(axocoatl_llm::ToolCall {
                    id: "native".into(),
                    name: "effect".into(),
                    arguments: serde_json::json!({}),
                    provider_metadata: axocoatl_core::ProviderMetadata::from([(
                        "signature".into(),
                        "exact".into(),
                    )]),
                }),
                _ => unreachable!(),
            }
            assert_ne!(request_identity(&changed).unwrap().0, original.0, "{field}");
        }
        assert_eq!(request_identity(&base).unwrap(), original);
    }

    #[test]
    fn encoded_provider_bound_counts_escaped_bytes_without_buffering_payload() {
        let event = StreamEvent::ReasoningDelta {
            delta: "\n".repeat(64),
        };
        let mut writer = BoundedDigest::new(100);
        assert!(encode_event(&mut writer, &event).is_err());
        assert!(writer.bytes <= 100);
    }
}
