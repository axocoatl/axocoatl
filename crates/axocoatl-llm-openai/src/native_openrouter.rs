//! Audited OpenRouter text/tool execution. The retained exact endpoint and wire
//! ceilings are verified before each single inference request; no retry/fallback.
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
    http_client, network_error, next_stream_item, read_error_text, read_json, validated_endpoint,
    SseDecoder, MAX_STREAM_BYTES, RESPONSE_TIMEOUT, STREAM_IDLE_TIMEOUT, STREAM_TOTAL_TIMEOUT,
};
use axocoatl_llm::{
    provider_tool_metadata, validate_chat_response, validate_response_tool_call,
    AccountedChatOutcome, ChatRequest, ChatResponse, FinishReason, LlmProvider,
    ProviderCapabilities, ProviderError, ProviderExecutionBounds, StreamEvent, ToolCall,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, pin::Pin};
use tokio_stream::{Stream, StreamExt};
#[path = "native_openrouter/money.rs"]
mod money;
#[path = "native_openrouter/observation.rs"]
mod observation;
#[path = "native_openrouter/stream.rs"]
mod stream;
use observation::CallShape;
pub use observation::{
    observe_native_openrouter_profiles, NativeOpenRouterEfforts, NativeOpenRouterObservation,
    NativeOpenRouterReasoning, NativeOpenRouterReasoningRequest, MINIMUM_REASONING_ALLOWANCE,
    PROMPT_TEMPLATE_ALLOWANCE, STREAMED_BYTES_PER_RESPONSE_TOKEN,
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
fn protocol(reason: impl Into<String>) -> ProviderError {
    ProviderError::Stream(format!("native OpenRouter: {}", reason.into()))
}

pub struct NativeOpenRouterProvider {
    inner: OpenAiProvider,
    observation: NativeOpenRouterObservation,
    max_output_tokens: usize,
    max_response_bytes: usize,
    reasoning: Option<NativeOpenRouterReasoningRequest>,
}
impl NativeOpenRouterProvider {
    pub async fn connect_observed(
        observation: NativeOpenRouterObservation,
        api_key: &str,
        max_output_tokens: usize,
        max_response_bytes: usize,
        reasoning: Option<NativeOpenRouterReasoningRequest>,
    ) -> Result<Self, ProviderError> {
        observation.validate()?;
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
        let this = Self {
            inner: OpenAiProvider::with_base_url(
                api_key,
                observation.model.clone(),
                &observation.base_url,
            )
            .with_provider_id(PROVIDER),
            observation,
            max_output_tokens,
            max_response_bytes,
            reasoning,
        };
        this.verify().await?;
        Ok(this)
    }
    async fn verify(&self) -> Result<(), ProviderError> {
        let profiles = observe_native_openrouter_profiles(
            &self.observation.base_url,
            &self.inner.api_key,
            &self.observation.model,
        )
        .await?;
        if !profiles
            .iter()
            .any(|profile| self.observation.same_contract(profile))
        {
            return Err(invalid("the retained OpenRouter endpoint, capabilities, price ceiling or token limits changed; review a new profile"));
        }
        Ok(())
    }
    /// The exact request body and its bounds. Every byte of the body counts
    /// toward the prompt bound: the provider renders the messages and tools,
    /// and each token of a byte-level tokenizer covers at least one byte.
    fn body(&self, request: &ChatRequest) -> Result<(Value, CallShape), ProviderError> {
        self.validate_request(request)?;
        let output = request.max_tokens.unwrap_or(self.max_output_tokens);
        let response_tokens = self.observation.response_allowance(output, self.reasoning);
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
                "only": [self.observation.endpoint_tag],
                "order": [self.observation.endpoint_tag],
                "allow_fallbacks": false,
                "require_parameters": true,
                // OpenRouter filters endpoints on their listed image and
                // audio prices even for a text-only request, so those caps
                // are left out: the request sends neither.
                "max_price": {
                    "prompt": self.observation.prompt_price_per_million,
                    "completion": self.observation.completion_price_per_million,
                    "request": self.observation.request_price.as_deref().unwrap_or("0"),
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
            .min(self.observation.prompt_limit());
        Ok((
            body,
            CallShape {
                prompt_tokens,
                response_tokens,
            },
        ))
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
    fn provider_id(&self) -> &str {
        PROVIDER
    }
    fn model_id(&self) -> &str {
        &self.observation.model
    }
    fn capabilities(&self) -> ProviderCapabilities {
        // Context fitting reserves the visible output; leave the reasoning
        // allowance and the endpoint's own prompt limit out of the window.
        let response = self
            .observation
            .response_allowance(self.max_output_tokens, self.reasoning);
        let reasoning = response - self.max_output_tokens;
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            structured_output: self
                .observation
                .supported_parameters
                .iter()
                .any(|p| p == "response_format"),
            vision: false,
            reasoning: reasoning > 0,
            embeddings: false,
            max_context_tokens: (self.observation.context_tokens.saturating_sub(reasoning)).min(
                self.observation
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
        axocoatl_llm::validate_provider_request(request, PROVIDER)?;
        if request
            .model_override
            .as_ref()
            .is_some_and(|model| model != &self.observation.model)
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
            if present
                && !self
                    .observation
                    .supported_parameters
                    .iter()
                    .any(|p| p == needed)
            {
                return Err(invalid(format!(
                    "retained OpenRouter endpoint does not support {needed}"
                )));
            }
        }
        Ok(())
    }
    fn execution_bounds(&self, request: &ChatRequest) -> Option<ProviderExecutionBounds> {
        let (_, shape) = self.body(request).ok()?;
        self.observation
            .call_bounds(shape, self.max_response_bytes)
            .ok()
    }
    fn follow_up_execution_bounds(
        &self,
        request: &ChatRequest,
        added_prompt_tokens: u64,
    ) -> Option<ProviderExecutionBounds> {
        let (_, mut shape) = self.body(request).ok()?;
        shape.prompt_tokens = shape
            .prompt_tokens
            .saturating_add(usize::try_from(added_prompt_tokens).unwrap_or(usize::MAX))
            .min(self.observation.prompt_limit());
        self.observation
            .call_bounds(shape, self.max_response_bytes)
            .ok()
    }
    fn response_tokens(&self, _request: &ChatRequest, output: usize) -> usize {
        self.observation.response_allowance(output, self.reasoning)
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
        stream::collect(self, request).await
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        let (body, shape) = self.body(&request)?;
        let bounds = self
            .observation
            .call_bounds(shape, self.max_response_bytes)?;
        self.verify().await?;
        let response = tokio::time::timeout(
            RESPONSE_TIMEOUT,
            self.inner
                .client
                .post(self.inner.endpoint()?)
                .bearer_auth(&self.inner.api_key)
                .header("HTTP-Referer", "https://axocoatl.ai")
                .header("X-OpenRouter-Title", "Axocoatl")
                .json(&body)
                .send(),
        )
        .await
        .map_err(|_| protocol("response headers timed out"))?
        .map_err(|e| network_error(&e, &[&self.inner.api_key]))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let detail = read_error_text(response, &[&self.inner.api_key]).await;
            return Err(ProviderError::ApiError {
                provider: PROVIDER.into(),
                status,
                message: detail,
            });
        }
        Ok(stream::decode(
            response,
            request,
            self.observation.clone(),
            shape,
            bounds,
            self.inner.api_key.clone(),
        ))
    }
}

#[cfg(test)]
#[path = "native_openrouter/tests.rs"]
mod tests;
