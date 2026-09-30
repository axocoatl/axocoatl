//! Audited OpenRouter text/tool execution. The retained exact endpoint and wire
//! ceilings are verified before each single inference request; no retry/fallback.
//! Requires an operator-declared OpenRouter-credit billing configuration. BYOK
//! is unsupported; enabling it externally invalidates this execution contract.
use crate::OpenAiProvider;
use axocoatl_core::{MeasuredTokenUsage, TokenUsageStats};
use axocoatl_llm::transport::{
    http_client, network_error, next_stream_item, read_error_text, read_json, validated_endpoint,
    SseDecoder, RESPONSE_TIMEOUT, STREAM_IDLE_TIMEOUT, STREAM_TOTAL_TIMEOUT,
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
pub use observation::{observe_native_openrouter_profiles, NativeOpenRouterObservation};

const PROVIDER: &str = "openrouter";
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
    bounds: ProviderExecutionBounds,
}
impl NativeOpenRouterProvider {
    pub async fn connect_observed(
        observation: NativeOpenRouterObservation,
        api_key: &str,
        max_output_tokens: usize,
        max_response_bytes: usize,
    ) -> Result<Self, ProviderError> {
        observation.validate()?;
        if api_key.is_empty()
            || max_output_tokens == 0
            || max_output_tokens > observation.max_output_tokens
            || max_output_tokens >= observation.context_tokens
            || !(4096..=16 * 1024 * 1024).contains(&max_response_bytes)
        {
            return Err(invalid(
                "unsupported exact OpenRouter execution bounds or missing credential",
            ));
        }
        let bounds = observation.execution_bounds(max_output_tokens, max_response_bytes)?;
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
            bounds,
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
            .any(|profile| profile.same_contract(&self.observation))
        {
            return Err(invalid("the retained OpenRouter endpoint, capabilities, price ceiling or token limits changed; review a new profile"));
        }
        Ok(())
    }
    fn body(&self, request: &ChatRequest) -> Result<Value, ProviderError> {
        self.validate_request(request)?;
        let requested_output = request.max_tokens.unwrap_or(self.max_output_tokens);
        let mut normalized = request.clone();
        normalized.max_tokens = Some(requested_output);
        let mut body = serde_json::to_value(self.inner.build_chat_request(&normalized)?)
            .map_err(|_| invalid("request serialization failed"))?;
        body.as_object_mut()
            .unwrap()
            .remove("max_completion_tokens");
        body["max_tokens"] = json!(requested_output);
        body["stream"] = json!(true);
        body["provider"] = json!({"only":[self.observation.endpoint_tag],"order":[self.observation.endpoint_tag],"allow_fallbacks":false,"require_parameters":true,"max_price":{"prompt":self.observation.prompt_price_per_million,"completion":self.observation.completion_price_per_million,"request":"0","image":"0","audio":"0"}});
        body["transforms"] = json!([]);
        body["plugins"] = json!([]);
        Ok(body)
    }
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
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            structured_output: self
                .observation
                .supported_parameters
                .iter()
                .any(|p| p == "response_format"),
            vision: false,
            reasoning: false,
            embeddings: false,
            max_context_tokens: self.observation.context_tokens,
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
        self.validate_request(request).ok().map(|_| self.bounds)
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
        let body = self.body(&request)?;
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
            self.max_response_bytes,
            self.max_output_tokens,
            self.inner.api_key.clone(),
        ))
    }
}

#[cfg(test)]
#[path = "native_openrouter/tests.rs"]
mod tests;
