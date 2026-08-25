//! Rate-limit fallback wrapper.
//!
//! [`FallbackProvider`] wraps a primary provider with an optional backup. When
//! the primary returns [`ProviderError::RateLimited`] — which providers return
//! at request time, before any token has streamed — the request is retried once
//! on the backup, rewritten to use the backup's model. Every other error, and
//! every successful call, passes through the primary untouched.
//!
//! Plain-text-only histories remain independently retryable. Once either route
//! returns a tool call, reserved metadata pins every request that retains that
//! native transaction to the exact route and effective model. This includes
//! later user turns and restored sessions: native ids, signatures, and thinking
//! blocks are never replayed to a different API. Removing the complete native
//! transaction during history compression removes the pin.
//! Known model constraints are checked only when a route is selected; an
//! operator-defined model namespace keeps unknown limits unknown and lets its
//! endpoint validate them. It is opt-in: with no target this wrapper is a
//! transparent pass-through apart from route evidence on tool calls.

use std::pin::Pin;
use std::sync::Arc;

use tokio_stream::Stream;
use tokio_stream::StreamExt;

use axocoatl_core::{
    ContentPart, MessageContent, MessageRole, ProviderMetadata, ResponseFormat, ToolCall,
};

use crate::error::ProviderError;
use crate::provider::{
    validate_chat_response, ChatRequest, ChatResponse, LlmProvider, ProviderCapabilities,
    StreamEvent, TOOL_METADATA_ROUTE_MODEL, TOOL_METADATA_ROUTE_PROVIDER, TOOL_METADATA_ROUTE_SLOT,
};

const PRIMARY_SLOT: &str = "primary";
const FALLBACK_SLOT: &str = "fallback";

#[derive(Clone, Debug, PartialEq, Eq)]
struct SelectedRoute {
    slot: String,
    provider: String,
    model: String,
}

impl SelectedRoute {
    fn metadata(&self) -> ProviderMetadata {
        ProviderMetadata::from([
            (TOOL_METADATA_ROUTE_SLOT.to_string(), self.slot.clone()),
            (
                TOOL_METADATA_ROUTE_PROVIDER.to_string(),
                self.provider.clone(),
            ),
            (TOOL_METADATA_ROUTE_MODEL.to_string(), self.model.clone()),
        ])
    }
}

/// A backup provider and the model to use on it.
pub struct FallbackTarget {
    pub provider: Arc<dyn LlmProvider>,
    pub model: String,
}

/// A provider that falls back to a backup on rate-limit. See the module docs.
pub struct FallbackProvider {
    primary: Arc<dyn LlmProvider>,
    fallback: Option<FallbackTarget>,
}

impl FallbackProvider {
    pub fn new(primary: Arc<dyn LlmProvider>, fallback: Option<FallbackTarget>) -> Self {
        Self { primary, fallback }
    }

    /// Point a request at the backup's model. `model_override` is the only
    /// model field on a request and is what every provider reads.
    fn retarget(mut request: ChatRequest, model: &str) -> ChatRequest {
        request.model_override = Some(model.to_string());
        request
    }

    fn primary_route(&self, request: &ChatRequest) -> SelectedRoute {
        SelectedRoute {
            slot: PRIMARY_SLOT.to_string(),
            provider: self.primary.provider_id().to_string(),
            model: request
                .model_override
                .clone()
                .unwrap_or_else(|| self.primary.model_id().to_string()),
        }
    }

    fn fallback_route(&self, target: &FallbackTarget) -> SelectedRoute {
        SelectedRoute {
            slot: FALLBACK_SLOT.to_string(),
            provider: target.provider.provider_id().to_string(),
            model: target.model.clone(),
        }
    }

    fn invalid_route(&self, message: impl Into<String>) -> ProviderError {
        ProviderError::InvalidRequest {
            provider: self.primary.provider_id().to_string(),
            message: message.into(),
        }
    }

    fn retained_call_route(&self, call: &ToolCall) -> Result<Option<SelectedRoute>, ProviderError> {
        let slot = call.provider_metadata.get(TOOL_METADATA_ROUTE_SLOT);
        let provider = call.provider_metadata.get(TOOL_METADATA_ROUTE_PROVIDER);
        let model = call.provider_metadata.get(TOOL_METADATA_ROUTE_MODEL);
        match (slot, provider, model) {
            (Some(slot), Some(provider), Some(model))
                if !slot.is_empty() && !provider.is_empty() && !model.is_empty() =>
            {
                Ok(Some(SelectedRoute {
                    slot: slot.clone(),
                    provider: provider.clone(),
                    model: model.clone(),
                }))
            }
            (None, None, None) => Ok(None),
            _ => Err(self.invalid_route(
                "retained tool-bearing history has an incomplete selected provider route",
            )),
        }
    }

    /// Validate every retained provider-native assistant/tool transaction
    /// before route selection. History is untrusted at this boundary: partial,
    /// interrupted, duplicated, or mismatched groups must never reach either
    /// provider. Non-empty ids correlate OpenAI/Anthropic/Mistral calls; id-less
    /// calls correlate by name and occurrence so parallel same-name Gemini
    /// calls remain replayable without inventing ids.
    fn validate_history_transactions(&self, request: &ChatRequest) -> Result<(), ProviderError> {
        let mut index = 0_usize;
        while index < request.messages.len() {
            let message = &request.messages[index];
            match message.role {
                MessageRole::Assistant if !message.tool_calls.is_empty() => {
                    let mut nonempty_ids = std::collections::HashSet::new();
                    for call in &message.tool_calls {
                        let retained_route = self.retained_call_route(call)?;
                        if call.name.is_empty() {
                            return Err(self.invalid_route(
                                "retained tool transaction contains an empty tool name",
                            ));
                        }
                        if !call.arguments.is_object() {
                            return Err(self.invalid_route(
                                "retained tool transaction contains non-object call arguments",
                            ));
                        }
                        if call.id.is_empty()
                            && retained_route
                                .as_ref()
                                .is_some_and(|route| route.provider != "gemini")
                        {
                            return Err(self.invalid_route(
                                "retained non-Gemini tool transaction contains an empty call id",
                            ));
                        }
                        if !call.id.is_empty() && !nonempty_ids.insert(call.id.as_str()) {
                            return Err(self.invalid_route(
                                "retained tool transaction contains duplicate non-empty call ids",
                            ));
                        }
                    }

                    let mut matched = vec![false; message.tool_calls.len()];
                    for result_offset in 0..message.tool_calls.len() {
                        let Some(result) = request.messages.get(index + 1 + result_offset) else {
                            return Err(self.invalid_route(
                                "retained tool transaction is missing one or more results",
                            ));
                        };
                        if result.role != MessageRole::Tool {
                            return Err(self.invalid_route(
                                "retained tool transaction is interrupted before all results",
                            ));
                        }
                        if !result.tool_calls.is_empty() {
                            return Err(self.invalid_route(
                                "retained Tool result message contains nested assistant tool calls",
                            ));
                        }
                        let Some(result_name) = result.name.as_deref() else {
                            return Err(self.invalid_route(
                                "retained tool result has no matching function name",
                            ));
                        };
                        let result_id = result.tool_call_id.as_deref().unwrap_or_default();
                        let occurrence =
                            message
                                .tool_calls
                                .iter()
                                .enumerate()
                                .position(|(call_index, call)| {
                                    !matched[call_index]
                                        && call.name == result_name
                                        && if call.id.is_empty() {
                                            result_id.is_empty()
                                        } else {
                                            call.id == result_id
                                        }
                                });
                        let Some(call_index) = occurrence else {
                            return Err(self.invalid_route(
                                "retained tool result is unmatched, duplicated, or mismatched",
                            ));
                        };
                        matched[call_index] = true;
                    }
                    if matched.iter().any(|matched| !matched) {
                        return Err(self.invalid_route(
                            "retained tool transaction is missing one or more results",
                        ));
                    }
                    index = index.saturating_add(1 + message.tool_calls.len());
                }
                MessageRole::Tool => {
                    return Err(self.invalid_route(
                        "retained history contains an orphan or extra tool result",
                    ));
                }
                _ => index = index.saturating_add(1),
            }
        }
        Ok(())
    }

    /// Find the one route compatible with all retained provider-native tool
    /// transactions. A completed transaction remains protocol-specific: its
    /// ids and opaque replay metadata cannot safely be sent to a different
    /// provider merely because another User message followed it.
    fn history_route(&self, request: &ChatRequest) -> Result<Option<SelectedRoute>, ProviderError> {
        self.validate_history_transactions(request)?;
        let mut selected: Option<SelectedRoute> = None;
        let mut saw_native_exchange = false;

        for message in &request.messages {
            if matches!(message.role, MessageRole::Tool) {
                saw_native_exchange = true;
            }
            if !matches!(message.role, MessageRole::Assistant) || message.tool_calls.is_empty() {
                continue;
            }
            saw_native_exchange = true;
            for call in &message.tool_calls {
                let route = match self.retained_call_route(call)? {
                    Some(route) => route,
                    None => {
                        return Err(self.invalid_route(
                            "retained legacy tool-bearing history has no selected provider route; start a fresh session or remove the complete legacy tool transaction before retrying",
                        ));
                    }
                };
                if selected.as_ref().is_some_and(|existing| existing != &route) {
                    return Err(self.invalid_route(
                        "retained tool-bearing history contains conflicting provider routes",
                    ));
                }
                selected = Some(route);
            }
        }

        if saw_native_exchange && selected.is_none() {
            return Err(self.invalid_route(
                "retained tool-bearing history has no replayable selected provider route",
            ));
        }
        let Some(route) = selected else {
            return Ok(None);
        };

        match route.slot.as_str() {
            PRIMARY_SLOT => {
                let expected = self.primary_route(request);
                if route != expected {
                    return Err(self.invalid_route(
                        "retained primary tool route no longer matches the configured provider and model",
                    ));
                }
            }
            FALLBACK_SLOT => {
                let Some(target) = &self.fallback else {
                    return Err(self.invalid_route(
                        "retained fallback tool route is unavailable in the current configuration",
                    ));
                };
                if route != self.fallback_route(target) {
                    return Err(self.invalid_route(
                        "retained fallback tool route no longer matches the configured provider and model",
                    ));
                }
            }
            _ => {
                return Err(self.invalid_route(
                    "retained tool-bearing history names an unknown provider route slot",
                ));
            }
        }
        Ok(Some(route))
    }

    fn annotate_response(
        &self,
        mut response: ChatResponse,
        route: &SelectedRoute,
    ) -> Result<ChatResponse, ProviderError> {
        validate_chat_response(&route.provider, &response)?;
        let metadata = route.metadata();
        for call in &mut response.tool_calls {
            if [
                TOOL_METADATA_ROUTE_SLOT,
                TOOL_METADATA_ROUTE_PROVIDER,
                TOOL_METADATA_ROUTE_MODEL,
            ]
            .iter()
            .any(|key| call.provider_metadata.contains_key(*key))
            {
                return Err(self.invalid_route(
                    "provider response attempted to set reserved fallback route metadata",
                ));
            }
            call.provider_metadata.extend(metadata.clone());
        }
        Ok(response)
    }

    fn wrap_stream(
        &self,
        stream: Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
        route: &SelectedRoute,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>> {
        let provider = self.primary.provider_id().to_string();
        let guarded = stream.map(move |event| match event {
            Ok(StreamEvent::ToolCallMetadata { ref metadata, .. })
                if [
                    TOOL_METADATA_ROUTE_SLOT,
                    TOOL_METADATA_ROUTE_PROVIDER,
                    TOOL_METADATA_ROUTE_MODEL,
                ]
                .iter()
                .any(|key| metadata.contains_key(*key)) =>
            {
                Err(ProviderError::InvalidRequest {
                    provider: provider.clone(),
                    message: "provider stream attempted to set reserved fallback route metadata"
                        .to_string(),
                })
            }
            other => other,
        });
        let selected = StreamEvent::ProviderRoute {
            metadata: route.metadata(),
        };
        Box::pin(tokio_stream::once(Ok(selected)).chain(guarded))
    }

    fn validate_capability_route(
        &self,
        request: &ChatRequest,
        provider: &dyn LlmProvider,
        streaming: bool,
        route_label: &str,
    ) -> Result<(), ProviderError> {
        provider.validate_request(request)?;
        // Open-ended model namespaces (OpenRouter, Ollama and arbitrary model
        // overrides) cannot be described honestly by a hard-coded table. In
        // that case retain provider protocol validation but let the selected
        // endpoint decide model-specific features and token ceilings.
        if !provider.model_constraints_known(request) {
            return Ok(());
        }
        let caps = provider.capabilities_for(request);
        let invalid = if streaming && !caps.streaming {
            Some("streaming")
        } else if !request.tools.is_empty() && !caps.tool_calling {
            Some("tool calling")
        } else if request.response_format == Some(ResponseFormat::Json) && !caps.structured_output {
            Some("structured JSON output")
        } else if request.messages.iter().any(|message| {
            matches!(
                &message.content,
                MessageContent::Parts(parts)
                    if parts.iter().any(|part| matches!(part, ContentPart::Image { .. }))
            )
        }) && !caps.vision
        {
            Some("vision input")
        } else {
            None
        };
        if let Some(capability) = invalid {
            return Err(self.invalid_route(format!(
                "configured {route_label} route does not support requested {capability}"
            )));
        }
        if let Some(max_tokens) = request.max_tokens {
            if caps.max_output_tokens > 0 && max_tokens > caps.max_output_tokens {
                return Err(self.invalid_route(format!(
                    "requested max_tokens exceeds the configured {route_label} route output limit"
                )));
            }
        }
        let estimated = provider.count_tokens(request);
        let output_headroom = request.max_tokens.unwrap_or(caps.max_output_tokens);
        if caps.max_context_tokens > 0
            && estimated.saturating_add(output_headroom) > caps.max_context_tokens
        {
            return Err(self.invalid_route(format!(
                "request plus output headroom exceeds the configured {route_label} route context limit"
            )));
        }
        Ok(())
    }

    fn validate_initial_request(
        &self,
        request: &ChatRequest,
        streaming: bool,
    ) -> Result<(), ProviderError> {
        self.validate_capability_route(request, self.primary.as_ref(), streaming, PRIMARY_SLOT)
    }
}

#[async_trait::async_trait]
impl LlmProvider for FallbackProvider {
    fn provider_id(&self) -> &str {
        self.primary.provider_id()
    }

    fn model_id(&self) -> &str {
        self.primary.model_id()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.primary.capabilities()
    }

    fn validate_request(&self, request: &ChatRequest) -> Result<(), ProviderError> {
        match self.history_route(request)? {
            Some(route) if route.slot == FALLBACK_SLOT => {
                let target = self
                    .fallback
                    .as_ref()
                    .expect("history_route validated fallback slot");
                target
                    .provider
                    .validate_request(&Self::retarget(request.clone(), &route.model))
            }
            Some(route) => self
                .primary
                .validate_request(&Self::retarget(request.clone(), &route.model)),
            None => self.primary.validate_request(request),
        }
    }

    fn capabilities_for(&self, request: &ChatRequest) -> ProviderCapabilities {
        match self.history_route(request) {
            Ok(Some(route)) if route.slot == FALLBACK_SLOT => {
                let target = self
                    .fallback
                    .as_ref()
                    .expect("history_route validated fallback slot");
                target
                    .provider
                    .capabilities_for(&Self::retarget(request.clone(), &route.model))
            }
            _ => self.primary.capabilities_for(request),
        }
    }

    fn model_constraints_known(&self, request: &ChatRequest) -> bool {
        match self.history_route(request) {
            Ok(Some(route)) if route.slot == FALLBACK_SLOT => {
                let target = self
                    .fallback
                    .as_ref()
                    .expect("history_route validated fallback slot");
                let routed = Self::retarget(request.clone(), &route.model);
                target.provider.model_constraints_known(&routed)
            }
            Ok(Some(route)) => {
                let routed = Self::retarget(request.clone(), &route.model);
                self.primary.model_constraints_known(&routed)
            }
            Ok(None) | Err(_) => self.primary.model_constraints_known(request),
        }
    }

    fn count_tokens(&self, request: &ChatRequest) -> usize {
        match self.history_route(request) {
            Ok(Some(route)) if route.slot == FALLBACK_SLOT => {
                let target = self
                    .fallback
                    .as_ref()
                    .expect("history_route validated fallback slot");
                target
                    .provider
                    .count_tokens(&Self::retarget(request.clone(), &route.model))
            }
            Ok(Some(route)) => self
                .primary
                .count_tokens(&Self::retarget(request.clone(), &route.model)),
            Ok(None) | Err(_) => self.fallback.as_ref().map_or_else(
                || self.primary.count_tokens(request),
                |fallback| {
                    let fallback_request = Self::retarget(request.clone(), &fallback.model);
                    self.primary
                        .count_tokens(request)
                        .max(fallback.provider.count_tokens(&fallback_request))
                },
            ),
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        if let Some(route) = self.history_route(&request)? {
            let selected_provider: &dyn LlmProvider = match route.slot.as_str() {
                PRIMARY_SLOT => self.primary.as_ref(),
                FALLBACK_SLOT => self
                    .fallback
                    .as_ref()
                    .expect("history_route validated fallback slot")
                    .provider
                    .as_ref(),
                _ => unreachable!("history_route validates slots"),
            };
            let routed_request = Self::retarget(request.clone(), &route.model);
            self.validate_capability_route(
                &routed_request,
                selected_provider,
                false,
                route.slot.as_str(),
            )?;
            let response = match route.slot.as_str() {
                PRIMARY_SLOT => {
                    self.primary
                        .chat(Self::retarget(request, &route.model))
                        .await?
                }
                FALLBACK_SLOT => {
                    self.fallback
                        .as_ref()
                        .expect("history_route validated fallback slot")
                        .provider
                        .chat(Self::retarget(request, &route.model))
                        .await?
                }
                _ => unreachable!("history_route validates slots"),
            };
            return self.annotate_response(response, &route);
        }

        self.validate_initial_request(&request, false)?;
        let primary_route = self.primary_route(&request);
        match self.primary.chat(request.clone()).await {
            Ok(response) => self.annotate_response(response, &primary_route),
            Err(ProviderError::RateLimited { provider, .. }) if self.fallback.is_some() => {
                let fb = self.fallback.as_ref().expect("guarded above");
                tracing::warn!(
                    primary = %provider,
                    fallback = %fb.provider.provider_id(),
                    model = %fb.model,
                    "primary rate-limited; falling back",
                );
                let route = self.fallback_route(fb);
                let fallback_request = Self::retarget(request, &fb.model);
                self.validate_capability_route(
                    &fallback_request,
                    fb.provider.as_ref(),
                    false,
                    FALLBACK_SLOT,
                )?;
                let response = fb.provider.chat(fallback_request).await?;
                self.annotate_response(response, &route)
            }
            Err(error) => Err(error),
        }
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        if let Some(route) = self.history_route(&request)? {
            let selected_provider: &dyn LlmProvider = match route.slot.as_str() {
                PRIMARY_SLOT => self.primary.as_ref(),
                FALLBACK_SLOT => self
                    .fallback
                    .as_ref()
                    .expect("history_route validated fallback slot")
                    .provider
                    .as_ref(),
                _ => unreachable!("history_route validates slots"),
            };
            let routed_request = Self::retarget(request.clone(), &route.model);
            self.validate_capability_route(
                &routed_request,
                selected_provider,
                true,
                route.slot.as_str(),
            )?;
            let stream = match route.slot.as_str() {
                PRIMARY_SLOT => {
                    self.primary
                        .chat_stream(Self::retarget(request, &route.model))
                        .await?
                }
                FALLBACK_SLOT => {
                    self.fallback
                        .as_ref()
                        .expect("history_route validated fallback slot")
                        .provider
                        .chat_stream(Self::retarget(request, &route.model))
                        .await?
                }
                _ => unreachable!("history_route validates slots"),
            };
            return Ok(self.wrap_stream(stream, &route));
        }

        self.validate_initial_request(&request, true)?;
        let primary_route = self.primary_route(&request);
        match self.primary.chat_stream(request.clone()).await {
            Ok(stream) => Ok(self.wrap_stream(stream, &primary_route)),
            Err(ProviderError::RateLimited { provider, .. }) if self.fallback.is_some() => {
                let fb = self.fallback.as_ref().expect("guarded above");
                tracing::warn!(
                    primary = %provider,
                    fallback = %fb.provider.provider_id(),
                    model = %fb.model,
                    "primary rate-limited; falling back",
                );
                let route = self.fallback_route(fb);
                let fallback_request = Self::retarget(request, &fb.model);
                self.validate_capability_route(
                    &fallback_request,
                    fb.provider.as_ref(),
                    true,
                    FALLBACK_SLOT,
                )?;
                let stream = fb.provider.chat_stream(fallback_request).await?;
                Ok(self.wrap_stream(stream, &route))
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FinishReason;
    use axocoatl_core::{ChatMessage, TokenUsageStats, ToolCall};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    enum Behavior {
        Ok(String),
        RateLimited,
        Auth,
    }

    /// A provider that behaves as configured and records the `model_override`
    /// of the last request it received (so tests can assert the model swap).
    struct Mock {
        id: String,
        behavior: Behavior,
        seen_model: Arc<Mutex<Option<String>>>,
    }

    impl Mock {
        fn build(
            id: &str,
            behavior: Behavior,
        ) -> (Arc<dyn LlmProvider>, Arc<Mutex<Option<String>>>) {
            let seen = Arc::new(Mutex::new(None));
            let provider: Arc<dyn LlmProvider> = Arc::new(Self {
                id: id.to_string(),
                behavior,
                seen_model: seen.clone(),
            });
            (provider, seen)
        }

        fn record(&self, request: &ChatRequest) {
            *self.seen_model.lock().unwrap() = request.model_override.clone();
        }

        fn response(&self, content: &str) -> ChatResponse {
            ChatResponse {
                content: content.to_string(),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: TokenUsageStats::new(1, 1),
                model: "mock-model".to_string(),
                provider: self.id.clone(),
            }
        }

        fn error(&self) -> ProviderError {
            match self.behavior {
                Behavior::RateLimited => ProviderError::RateLimited {
                    provider: self.id.clone(),
                    retry_after_secs: None,
                },
                Behavior::Auth => ProviderError::AuthError {
                    provider: self.id.clone(),
                },
                Behavior::Ok(_) => unreachable!(),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for Mock {
        fn provider_id(&self) -> &str {
            &self.id
        }
        fn model_id(&self) -> &str {
            "mock-model"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                structured_output: true,
                vision: true,
                reasoning: true,
                embeddings: false,
                max_context_tokens: 100_000,
                max_output_tokens: 10_000,
            }
        }
        async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
            self.record(&request);
            match &self.behavior {
                Behavior::Ok(content) => Ok(self.response(content)),
                _ => Err(self.error()),
            }
        }
        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.record(&request);
            match &self.behavior {
                Behavior::Ok(content) => {
                    let events = vec![
                        Ok(StreamEvent::TextDelta {
                            delta: content.clone(),
                        }),
                        Ok(StreamEvent::Done {
                            finish_reason: FinishReason::Stop,
                        }),
                    ];
                    Ok(Box::pin(tokio_stream::iter(events)))
                }
                _ => Err(self.error()),
            }
        }
    }

    fn target(provider: Arc<dyn LlmProvider>, model: &str) -> FallbackTarget {
        FallbackTarget {
            provider,
            model: model.to_string(),
        }
    }

    #[tokio::test]
    async fn primary_success_does_not_touch_fallback() {
        let (primary, _) = Mock::build("openai", Behavior::Ok("primary".into()));
        let (backup, backup_seen) = Mock::build("anthropic", Behavior::Ok("backup".into()));
        let fp = FallbackProvider::new(primary, Some(target(backup, "claude-x")));

        let resp = fp.chat(ChatRequest::simple("hi")).await.unwrap();
        assert_eq!(resp.content, "primary");
        assert!(
            backup_seen.lock().unwrap().is_none(),
            "backup must not be called when the primary succeeds"
        );
    }

    #[tokio::test]
    async fn rate_limited_falls_back_with_backup_model() {
        let (primary, _) = Mock::build("openai", Behavior::RateLimited);
        let (backup, backup_seen) = Mock::build("anthropic", Behavior::Ok("backup".into()));
        let fp = FallbackProvider::new(primary, Some(target(backup, "claude-x")));

        let resp = fp.chat(ChatRequest::simple("hi")).await.unwrap();
        assert_eq!(resp.content, "backup");
        assert_eq!(
            backup_seen.lock().unwrap().as_deref(),
            Some("claude-x"),
            "the backup must be called with its own model"
        );
    }

    #[tokio::test]
    async fn rate_limited_without_fallback_propagates() {
        let (primary, _) = Mock::build("openai", Behavior::RateLimited);
        let fp = FallbackProvider::new(primary, None);
        assert!(matches!(
            fp.chat(ChatRequest::simple("hi")).await,
            Err(ProviderError::RateLimited { .. })
        ));
    }

    #[tokio::test]
    async fn non_rate_limit_error_is_not_retried() {
        let (primary, _) = Mock::build("openai", Behavior::Auth);
        let (backup, backup_seen) = Mock::build("anthropic", Behavior::Ok("backup".into()));
        let fp = FallbackProvider::new(primary, Some(target(backup, "claude-x")));

        assert!(matches!(
            fp.chat(ChatRequest::simple("hi")).await,
            Err(ProviderError::AuthError { .. })
        ));
        assert!(
            backup_seen.lock().unwrap().is_none(),
            "a non-rate-limit error must not fall back"
        );
    }

    #[tokio::test]
    async fn streaming_falls_back_on_rate_limit() {
        use tokio_stream::StreamExt;
        let (primary, _) = Mock::build("openai", Behavior::RateLimited);
        let (backup, backup_seen) = Mock::build("anthropic", Behavior::Ok("streamed".into()));
        let fp = FallbackProvider::new(primary, Some(target(backup, "claude-x")));

        let mut stream = fp.chat_stream(ChatRequest::simple("hi")).await.unwrap();
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            if let Ok(StreamEvent::TextDelta { delta }) = ev {
                text.push_str(&delta);
            }
        }
        assert_eq!(text, "streamed");
        assert_eq!(backup_seen.lock().unwrap().as_deref(), Some("claude-x"));
    }

    #[derive(Clone)]
    enum ScriptedOutcome {
        Text(&'static str),
        Tool(&'static str),
        ToolWithReservedRoute,
        RateLimited,
    }

    struct ScriptedMock {
        id: String,
        model: String,
        constraints_known: bool,
        capabilities_override: Option<ProviderCapabilities>,
        outcomes: Mutex<VecDeque<ScriptedOutcome>>,
        seen: Arc<Mutex<Vec<Option<String>>>>,
    }

    type SeenModels = Arc<Mutex<Vec<Option<String>>>>;
    type BuiltScriptedMock = (Arc<dyn LlmProvider>, SeenModels);

    impl ScriptedMock {
        fn build(
            id: &str,
            model: &str,
            outcomes: impl IntoIterator<Item = ScriptedOutcome>,
        ) -> BuiltScriptedMock {
            Self::build_with_constraints(id, model, true, outcomes)
        }

        fn build_with_constraints(
            id: &str,
            model: &str,
            constraints_known: bool,
            outcomes: impl IntoIterator<Item = ScriptedOutcome>,
        ) -> BuiltScriptedMock {
            let seen = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    id: id.to_string(),
                    model: model.to_string(),
                    constraints_known,
                    capabilities_override: None,
                    outcomes: Mutex::new(outcomes.into_iter().collect()),
                    seen: seen.clone(),
                }),
                seen,
            )
        }

        fn build_with_capabilities(
            id: &str,
            model: &str,
            capabilities: ProviderCapabilities,
            outcomes: impl IntoIterator<Item = ScriptedOutcome>,
        ) -> BuiltScriptedMock {
            let seen = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    id: id.to_string(),
                    model: model.to_string(),
                    constraints_known: true,
                    capabilities_override: Some(capabilities),
                    outcomes: Mutex::new(outcomes.into_iter().collect()),
                    seen: seen.clone(),
                }),
                seen,
            )
        }

        fn next(&self, request: &ChatRequest) -> ScriptedOutcome {
            self.seen
                .lock()
                .unwrap()
                .push(request.model_override.clone());
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("test supplied enough provider outcomes")
        }

        fn response(&self, outcome: ScriptedOutcome) -> Result<ChatResponse, ProviderError> {
            match outcome {
                ScriptedOutcome::Text(content) => Ok(ChatResponse {
                    content: content.to_string(),
                    tool_calls: Vec::new(),
                    finish_reason: FinishReason::Stop,
                    usage: TokenUsageStats::new(1, 1),
                    model: self.model.clone(),
                    provider: self.id.clone(),
                }),
                ScriptedOutcome::Tool(name) => Ok(ChatResponse {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: format!("{}_call", self.id),
                        name: name.to_string(),
                        arguments: serde_json::json!({}),
                        provider_metadata: Default::default(),
                    }],
                    finish_reason: FinishReason::ToolUse,
                    usage: TokenUsageStats::new(1, 1),
                    model: self.model.clone(),
                    provider: self.id.clone(),
                }),
                ScriptedOutcome::ToolWithReservedRoute => Ok(ChatResponse {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: format!("{}_call", self.id),
                        name: "lookup".to_string(),
                        arguments: serde_json::json!({}),
                        provider_metadata: ProviderMetadata::from([(
                            TOOL_METADATA_ROUTE_SLOT.to_string(),
                            "forged".to_string(),
                        )]),
                    }],
                    finish_reason: FinishReason::ToolUse,
                    usage: TokenUsageStats::new(1, 1),
                    model: self.model.clone(),
                    provider: self.id.clone(),
                }),
                ScriptedOutcome::RateLimited => Err(ProviderError::RateLimited {
                    provider: self.id.clone(),
                    retry_after_secs: None,
                }),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for ScriptedMock {
        fn provider_id(&self) -> &str {
            &self.id
        }

        fn model_id(&self) -> &str {
            &self.model
        }

        fn capabilities(&self) -> ProviderCapabilities {
            if let Some(capabilities) = &self.capabilities_override {
                return capabilities.clone();
            }
            if !self.constraints_known {
                return ProviderCapabilities {
                    streaming: false,
                    tool_calling: false,
                    structured_output: false,
                    vision: false,
                    reasoning: false,
                    embeddings: false,
                    max_context_tokens: 1,
                    max_output_tokens: 1,
                };
            }
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                structured_output: true,
                vision: true,
                reasoning: true,
                embeddings: false,
                max_context_tokens: 100_000,
                max_output_tokens: 10_000,
            }
        }

        fn model_constraints_known(&self, _request: &ChatRequest) -> bool {
            self.constraints_known
        }

        async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
            self.response(self.next(&request))
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let response = self.response(self.next(&request))?;
            let mut events = Vec::new();
            for (index, call) in response.tool_calls.into_iter().enumerate() {
                let id = call.id;
                events.push(Ok(StreamEvent::ToolCallDelta {
                    index: Some(index),
                    id: id.clone(),
                    name: Some(call.name),
                    args_delta: call.arguments.to_string(),
                }));
                if !call.provider_metadata.is_empty() {
                    events.push(Ok(StreamEvent::ToolCallMetadata {
                        index: Some(index),
                        id,
                        metadata: call.provider_metadata,
                    }));
                }
            }
            if !response.content.is_empty() {
                events.push(Ok(StreamEvent::TextDelta {
                    delta: response.content,
                }));
            }
            events.push(Ok(StreamEvent::Done {
                finish_reason: response.finish_reason,
            }));
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    fn followup_request(calls: Vec<ToolCall>) -> ChatRequest {
        let mut request = ChatRequest::simple("start current turn");
        request
            .messages
            .push(ChatMessage::assistant_with_tool_calls("", calls.clone()));
        for call in calls {
            request
                .messages
                .push(ChatMessage::tool_result("{}", call.name, call.id));
        }
        request
    }

    #[tokio::test]
    async fn primary_tool_exchange_never_falls_back_on_followup() {
        let (primary, primary_seen) = ScriptedMock::build(
            "openai",
            "gpt-primary",
            [
                ScriptedOutcome::Tool("lookup"),
                ScriptedOutcome::RateLimited,
            ],
        );
        let (backup, backup_seen) =
            ScriptedMock::build("gemini", "gemini-backup", [ScriptedOutcome::Text("bad")]);
        let provider = FallbackProvider::new(primary, Some(target(backup, "gemini-backup")));

        let first = provider
            .chat(ChatRequest::simple("start current turn"))
            .await
            .unwrap();
        assert_eq!(
            first.tool_calls[0]
                .provider_metadata
                .get(TOOL_METADATA_ROUTE_SLOT)
                .map(String::as_str),
            Some(PRIMARY_SLOT)
        );
        let error = provider
            .chat(followup_request(first.tool_calls))
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderError::RateLimited { .. }));
        assert_eq!(primary_seen.lock().unwrap().len(), 2);
        assert!(backup_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn primary_only_tool_route_survives_restart_and_rejects_model_switch() {
        let (first_primary, first_seen) =
            ScriptedMock::build("openai", "model-a", [ScriptedOutcome::Tool("lookup")]);
        let first_provider = FallbackProvider::new(first_primary, None);
        let first = first_provider
            .chat(ChatRequest::simple("start on model A"))
            .await
            .unwrap();
        assert_eq!(first.tool_calls.len(), 1);
        assert_eq!(
            first.tool_calls[0]
                .provider_metadata
                .get(TOOL_METADATA_ROUTE_SLOT)
                .map(String::as_str),
            Some(PRIMARY_SLOT)
        );
        assert_eq!(
            first.tool_calls[0]
                .provider_metadata
                .get(TOOL_METADATA_ROUTE_MODEL)
                .map(String::as_str),
            Some("model-a")
        );
        assert_eq!(first_seen.lock().unwrap().len(), 1);

        // Persist and restore only the request history. A new wrapper instance
        // has no hidden route state, so success proves the evidence itself pins
        // replay to the same primary/model after restart or supplied-history use.
        let retained = followup_request(first.tool_calls);
        let encoded = serde_json::to_vec(&retained.messages).unwrap();
        let restored_messages: Vec<ChatMessage> = serde_json::from_slice(&encoded).unwrap();

        let (model_b, model_b_seen) =
            ScriptedMock::build("openai", "model-b", [ScriptedOutcome::Text("unsafe")]);
        let model_b_provider = FallbackProvider::new(model_b, None);
        let switched = ChatRequest {
            messages: restored_messages.clone(),
            model_override: Some("model-b".to_string()),
            ..ChatRequest::simple("unused")
        };
        assert!(matches!(
            model_b_provider.chat(switched).await,
            Err(ProviderError::InvalidRequest { message, .. })
                if message.contains("no longer matches")
        ));
        assert!(
            model_b_seen.lock().unwrap().is_empty(),
            "model B must be rejected before provider dispatch"
        );

        let (restored_a, restored_a_seen) =
            ScriptedMock::build("openai", "model-a", [ScriptedOutcome::Text("safe")]);
        let restored_provider = FallbackProvider::new(restored_a, None);
        let same_route = ChatRequest {
            messages: restored_messages,
            model_override: Some("model-a".to_string()),
            ..ChatRequest::simple("unused")
        };
        assert_eq!(
            restored_provider.chat(same_route).await.unwrap().content,
            "safe"
        );
        assert_eq!(restored_a_seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn fallback_tool_exchange_bypasses_primary_on_followup() {
        let (primary, primary_seen) =
            ScriptedMock::build("openai", "gpt-primary", [ScriptedOutcome::RateLimited]);
        let (backup, backup_seen) = ScriptedMock::build(
            "gemini",
            "gemini-backup",
            [
                ScriptedOutcome::Tool("lookup"),
                ScriptedOutcome::Text("done"),
            ],
        );
        let provider = FallbackProvider::new(primary, Some(target(backup, "gemini-backup")));

        let first = provider
            .chat(ChatRequest::simple("start current turn"))
            .await
            .unwrap();
        assert_eq!(
            first.tool_calls[0]
                .provider_metadata
                .get(TOOL_METADATA_ROUTE_SLOT)
                .map(String::as_str),
            Some(FALLBACK_SLOT)
        );
        let second = provider
            .chat(followup_request(first.tool_calls))
            .await
            .unwrap();
        assert_eq!(second.content, "done");
        assert_eq!(primary_seen.lock().unwrap().len(), 1);
        assert_eq!(backup_seen.lock().unwrap().len(), 2);
        assert_eq!(
            backup_seen.lock().unwrap()[1].as_deref(),
            Some("gemini-backup")
        );
    }

    #[tokio::test]
    async fn same_provider_fallback_stays_pinned_to_its_distinct_model() {
        let (primary, primary_seen) =
            ScriptedMock::build("openai", "large-primary", [ScriptedOutcome::RateLimited]);
        let (backup, backup_seen) = ScriptedMock::build(
            "openai",
            "small-backup",
            [
                ScriptedOutcome::Tool("lookup"),
                ScriptedOutcome::Text("done"),
            ],
        );
        let provider = FallbackProvider::new(primary, Some(target(backup, "small-backup")));

        let first = provider.chat(ChatRequest::simple("start")).await.unwrap();
        assert_eq!(
            first.tool_calls[0]
                .provider_metadata
                .get(TOOL_METADATA_ROUTE_SLOT)
                .map(String::as_str),
            Some(FALLBACK_SLOT)
        );
        assert_eq!(
            first.tool_calls[0]
                .provider_metadata
                .get(TOOL_METADATA_ROUTE_MODEL)
                .map(String::as_str),
            Some("small-backup")
        );
        let response = provider
            .chat(followup_request(first.tool_calls))
            .await
            .unwrap();
        assert_eq!(response.content, "done");
        assert_eq!(primary_seen.lock().unwrap().len(), 1);
        assert_eq!(
            backup_seen.lock().unwrap().as_slice(),
            &[
                Some("small-backup".to_string()),
                Some("small-backup".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn conflicting_or_stale_retained_routes_fail_closed() {
        let (primary, primary_seen) =
            ScriptedMock::build("openai", "gpt-primary", [ScriptedOutcome::Text("unused")]);
        let (backup, backup_seen) =
            ScriptedMock::build("gemini", "gemini-backup", [ScriptedOutcome::Text("unused")]);
        let provider = FallbackProvider::new(primary, Some(target(backup, "gemini-backup")));

        let mut primary_call = ToolCall {
            id: "a".to_string(),
            name: "lookup".to_string(),
            arguments: serde_json::json!({}),
            provider_metadata: SelectedRoute {
                slot: PRIMARY_SLOT.to_string(),
                provider: "openai".to_string(),
                model: "gpt-primary".to_string(),
            }
            .metadata(),
        };
        let mut fallback_call = primary_call.clone();
        fallback_call.id = "b".to_string();
        fallback_call.provider_metadata = SelectedRoute {
            slot: FALLBACK_SLOT.to_string(),
            provider: "gemini".to_string(),
            model: "gemini-backup".to_string(),
        }
        .metadata();
        let conflict = followup_request(vec![primary_call.clone(), fallback_call]);
        assert!(matches!(
            provider.chat(conflict).await,
            Err(ProviderError::InvalidRequest { message, .. }) if message.contains("conflicting")
        ));

        primary_call.provider_metadata.insert(
            TOOL_METADATA_ROUTE_MODEL.to_string(),
            "removed-model".to_string(),
        );
        assert!(matches!(
            provider.chat(followup_request(vec![primary_call])).await,
            Err(ProviderError::InvalidRequest { message, .. }) if message.contains("no longer matches")
        ));
        assert!(primary_seen.lock().unwrap().is_empty());
        assert!(backup_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn completed_primary_tool_turn_remains_pinned_across_user_turns() {
        let (primary, primary_seen) =
            ScriptedMock::build("openai", "gpt-primary", [ScriptedOutcome::RateLimited]);
        let (backup, backup_seen) =
            ScriptedMock::build("gemini", "gemini-backup", [ScriptedOutcome::Text("fresh")]);
        let provider = FallbackProvider::new(primary, Some(target(backup, "gemini-backup")));
        let old_call = ToolCall {
            id: "old".to_string(),
            name: "lookup".to_string(),
            arguments: serde_json::json!({}),
            provider_metadata: SelectedRoute {
                slot: PRIMARY_SLOT.to_string(),
                provider: "openai".to_string(),
                model: "gpt-primary".to_string(),
            }
            .metadata(),
        };
        let mut request = followup_request(vec![old_call]);
        request.messages.push(ChatMessage::user("new turn"));

        assert!(matches!(
            provider.chat(request).await,
            Err(ProviderError::RateLimited { .. })
        ));
        assert_eq!(primary_seen.lock().unwrap().len(), 1);
        assert!(backup_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn completed_fallback_tool_turn_stays_pinned_after_restore_and_new_user() {
        let (first_primary, first_primary_seen) =
            ScriptedMock::build("mistral", "mistral-primary", [ScriptedOutcome::RateLimited]);
        let (first_backup, first_backup_seen) = ScriptedMock::build(
            "openai",
            "openai-backup",
            [
                ScriptedOutcome::Tool("lookup"),
                ScriptedOutcome::Text("turn one complete"),
            ],
        );
        let first_provider =
            FallbackProvider::new(first_primary, Some(target(first_backup, "openai-backup")));

        let first = first_provider
            .chat(ChatRequest::simple("start turn one"))
            .await
            .unwrap();
        assert_eq!(first.tool_calls[0].id, "openai_call");
        let mut turn_one_history = followup_request(first.tool_calls);
        let final_response = first_provider.chat(turn_one_history.clone()).await.unwrap();
        assert_eq!(final_response.content, "turn one complete");
        turn_one_history
            .messages
            .push(ChatMessage::assistant(final_response.content));
        turn_one_history
            .messages
            .push(ChatMessage::user("start turn two"));
        assert_eq!(first_primary_seen.lock().unwrap().len(), 1);
        assert_eq!(first_backup_seen.lock().unwrap().len(), 2);

        // Simulate a restart/checkpoint round-trip rather than relying on any
        // state held by the first wrapper instance.
        let encoded = serde_json::to_string(&turn_one_history.messages).unwrap();
        let restored_messages = serde_json::from_str(&encoded).unwrap();
        let (restored_primary, restored_primary_seen) = ScriptedMock::build(
            "mistral",
            "mistral-primary",
            [ScriptedOutcome::Text("unsafe")],
        );
        let (restored_backup, restored_backup_seen) = ScriptedMock::build(
            "openai",
            "openai-backup",
            [ScriptedOutcome::Text("turn two stays on OpenAI")],
        );
        let restored_provider = FallbackProvider::new(
            restored_primary,
            Some(target(restored_backup, "openai-backup")),
        );
        let restored_request = ChatRequest {
            messages: restored_messages,
            ..ChatRequest::simple("unused")
        };

        let response = restored_provider.chat(restored_request).await.unwrap();
        assert_eq!(response.content, "turn two stays on OpenAI");
        assert!(restored_primary_seen.lock().unwrap().is_empty());
        assert_eq!(restored_backup_seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn restored_foreign_tool_ids_never_cross_into_mistral_primary() {
        for (provider_id, fallback_model, tool_id) in [
            ("openai", "openai-backup", "call_not_nine_chars"),
            ("anthropic", "claude-backup", "toolu_01_native_identifier"),
            ("gemini", "gemini-backup", ""),
        ] {
            let route = SelectedRoute {
                slot: FALLBACK_SLOT.to_string(),
                provider: provider_id.to_string(),
                model: fallback_model.to_string(),
            };
            let call = ToolCall {
                id: tool_id.to_string(),
                name: "lookup".to_string(),
                arguments: serde_json::json!({}),
                provider_metadata: route.metadata(),
            };
            let mut request = followup_request(vec![call]);
            request
                .messages
                .push(ChatMessage::assistant("completed native turn"));
            request.messages.push(ChatMessage::user("next turn"));

            let encoded = serde_json::to_string(&request.messages).unwrap();
            request.messages = serde_json::from_str(&encoded).unwrap();
            let (primary, primary_seen) = ScriptedMock::build(
                "mistral",
                "mistral-primary",
                [ScriptedOutcome::Text("unsafe cross-protocol replay")],
            );
            let (backup, backup_seen) = ScriptedMock::build(
                provider_id,
                fallback_model,
                [ScriptedOutcome::Text("safe native replay")],
            );
            let provider = FallbackProvider::new(primary, Some(target(backup, fallback_model)));

            let response = provider.chat(request).await.unwrap();
            assert_eq!(response.content, "safe native replay");
            assert!(primary_seen.lock().unwrap().is_empty());
            assert_eq!(backup_seen.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn orphaned_assistant_tool_call_fails_before_any_route_dispatch() {
        let route = SelectedRoute {
            slot: FALLBACK_SLOT.to_string(),
            provider: "gemini".to_string(),
            model: "gemini-backup".to_string(),
        };
        let call = ToolCall {
            id: String::new(),
            name: "lookup".to_string(),
            arguments: serde_json::json!({}),
            provider_metadata: route.metadata(),
        };
        let mut request = ChatRequest::simple("earlier turn");
        request
            .messages
            .push(ChatMessage::assistant_with_tool_calls("", vec![call]));
        request.messages.push(ChatMessage::user("later turn"));

        let (primary, primary_seen) = ScriptedMock::build(
            "mistral",
            "mistral-primary",
            [ScriptedOutcome::Text("unsafe")],
        );
        let (backup, backup_seen) = ScriptedMock::build(
            "gemini",
            "gemini-backup",
            [ScriptedOutcome::Text("still pinned")],
        );
        let provider = FallbackProvider::new(primary, Some(target(backup, "gemini-backup")));

        assert!(matches!(
            provider.chat(request).await,
            Err(ProviderError::InvalidRequest { message, .. })
                if message.contains("interrupted")
        ));
        assert!(primary_seen.lock().unwrap().is_empty());
        assert!(backup_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn orphaned_tool_result_fails_closed_instead_of_unlocking_route() {
        let mut request = ChatRequest::simple("earlier turn");
        request
            .messages
            .push(ChatMessage::tool_result("{}", "lookup", "orphan"));
        request.messages.push(ChatMessage::user("later turn"));
        let (primary, primary_seen) =
            ScriptedMock::build("openai", "gpt-primary", [ScriptedOutcome::Text("unsafe")]);
        let (backup, backup_seen) =
            ScriptedMock::build("gemini", "gemini-backup", [ScriptedOutcome::Text("unsafe")]);
        let provider = FallbackProvider::new(primary, Some(target(backup, "gemini-backup")));

        assert!(matches!(
            provider.chat(request).await,
            Err(ProviderError::InvalidRequest { message, .. })
                if message.contains("orphan")
        ));
        assert!(primary_seen.lock().unwrap().is_empty());
        assert!(backup_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_native_transactions_reject_chat_and_stream_before_dispatch() {
        let route = SelectedRoute {
            slot: FALLBACK_SLOT.to_string(),
            provider: "gemini".to_string(),
            model: "gemini-backup".to_string(),
        };
        let call = ToolCall {
            id: "call-a".to_string(),
            name: "lookup".to_string(),
            arguments: serde_json::json!({}),
            provider_metadata: route.metadata(),
        };
        let assistant = ChatMessage::assistant_with_tool_calls("", vec![call.clone()]);
        let primary_route = SelectedRoute {
            slot: PRIMARY_SLOT.to_string(),
            provider: "openai".to_string(),
            model: "gpt-primary".to_string(),
        };
        let openai_empty_id = ToolCall {
            id: String::new(),
            name: "lookup".to_string(),
            arguments: serde_json::json!({}),
            provider_metadata: primary_route.metadata(),
        };
        let scalar_arguments = ToolCall {
            arguments: serde_json::json!("not an object"),
            ..call.clone()
        };
        let mut malformed_tool_result = ChatMessage::tool_result("{}", "lookup", "call-a");
        malformed_tool_result.tool_calls = vec![call.clone()];
        let malformed = vec![
            vec![ChatMessage::user("start"), assistant.clone()],
            vec![
                ChatMessage::user("start"),
                assistant.clone(),
                ChatMessage::user("interrupt"),
            ],
            vec![
                ChatMessage::user("start"),
                assistant.clone(),
                ChatMessage::tool_result("{}", "lookup", "wrong-id"),
            ],
            vec![
                ChatMessage::user("start"),
                assistant.clone(),
                ChatMessage::tool_result("{}", "lookup", "call-a"),
                ChatMessage::tool_result("duplicate", "lookup", "call-a"),
            ],
            vec![
                ChatMessage::user("start"),
                ChatMessage::assistant_with_tool_calls("", vec![call.clone(), call.clone()]),
                ChatMessage::tool_result("{}", "lookup", "call-a"),
                ChatMessage::tool_result("{}", "lookup", "call-a"),
            ],
            vec![
                ChatMessage::user("start"),
                ChatMessage::assistant_with_tool_calls("", vec![openai_empty_id]),
                ChatMessage::tool_result("{}", "lookup", ""),
            ],
            vec![
                ChatMessage::user("start"),
                ChatMessage::assistant_with_tool_calls("", vec![scalar_arguments]),
                ChatMessage::tool_result("{}", "lookup", "call-a"),
            ],
            vec![ChatMessage::user("start"), assistant, malformed_tool_result],
        ];

        for messages in malformed {
            for streaming in [false, true] {
                let (primary, primary_seen) =
                    ScriptedMock::build("openai", "gpt-primary", Vec::<ScriptedOutcome>::new());
                let (backup, backup_seen) =
                    ScriptedMock::build("gemini", "gemini-backup", Vec::<ScriptedOutcome>::new());
                let provider =
                    FallbackProvider::new(primary, Some(target(backup, "gemini-backup")));
                let request = ChatRequest {
                    messages: messages.clone(),
                    ..ChatRequest::simple("unused")
                };

                let rejected = if streaming {
                    provider.chat_stream(request).await.map(|_| ())
                } else {
                    provider.chat(request).await.map(|_| ())
                };
                assert!(matches!(
                    rejected,
                    Err(ProviderError::InvalidRequest { .. })
                ));
                assert!(primary_seen.lock().unwrap().is_empty());
                assert!(backup_seen.lock().unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn idless_parallel_same_name_results_pair_by_occurrence() {
        let route = SelectedRoute {
            slot: FALLBACK_SLOT.to_string(),
            provider: "gemini".to_string(),
            model: "gemini-backup".to_string(),
        };
        let calls = ["first", "second"]
            .into_iter()
            .map(|slot| ToolCall {
                id: String::new(),
                name: "lookup".to_string(),
                arguments: serde_json::json!({"slot": slot}),
                provider_metadata: route.metadata(),
            })
            .collect();
        let request = followup_request(calls);
        let (primary, primary_seen) =
            ScriptedMock::build("openai", "gpt-primary", Vec::<ScriptedOutcome>::new());
        let (backup, backup_seen) =
            ScriptedMock::build("gemini", "gemini-backup", [ScriptedOutcome::Text("paired")]);
        let provider = FallbackProvider::new(primary, Some(target(backup, "gemini-backup")));

        let response = provider.chat(request).await.unwrap();

        assert_eq!(response.content, "paired");
        assert!(primary_seen.lock().unwrap().is_empty());
        assert_eq!(backup_seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn legacy_tool_history_without_route_metadata_has_an_explicit_repair_error() {
        let legacy_call = ToolCall {
            id: "legacy_call".to_string(),
            name: "lookup".to_string(),
            arguments: serde_json::json!({}),
            provider_metadata: ProviderMetadata::default(),
        };
        let request = followup_request(vec![legacy_call]);
        let (primary, primary_seen) =
            ScriptedMock::build("openai", "gpt-primary", [ScriptedOutcome::Text("unsafe")]);
        let (backup, backup_seen) =
            ScriptedMock::build("gemini", "gemini-backup", [ScriptedOutcome::Text("unsafe")]);
        let provider = FallbackProvider::new(primary, Some(target(backup, "gemini-backup")));

        assert!(matches!(
            provider.chat(request).await,
            Err(ProviderError::InvalidRequest { message, .. })
                if message.contains("legacy tool-bearing history")
                    && message.contains("fresh session")
                    && message.contains("complete legacy tool transaction")
        ));
        assert!(primary_seen.lock().unwrap().is_empty());
        assert!(backup_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn streaming_announces_exact_selected_route_before_content() {
        let (primary, _) =
            ScriptedMock::build("openai", "gpt-primary", [ScriptedOutcome::Text("hello")]);
        let provider = FallbackProvider::new(primary, None);
        let mut stream = provider
            .chat_stream(ChatRequest::simple("hi"))
            .await
            .unwrap();
        let first = stream.next().await.unwrap().unwrap();
        assert!(matches!(
            first,
            StreamEvent::ProviderRoute { metadata }
                if metadata.get(TOOL_METADATA_ROUTE_SLOT).map(String::as_str) == Some(PRIMARY_SLOT)
                    && metadata.get(TOOL_METADATA_ROUTE_PROVIDER).map(String::as_str) == Some("openai")
                    && metadata.get(TOOL_METADATA_ROUTE_MODEL).map(String::as_str) == Some("gpt-primary")
        ));
        assert!(matches!(
            stream.next().await.unwrap().unwrap(),
            StreamEvent::TextDelta { .. }
        ));
    }

    #[tokio::test]
    async fn nonstream_provider_cannot_forge_reserved_route_metadata() {
        let (primary, _) = ScriptedMock::build(
            "openai",
            "gpt-primary",
            [ScriptedOutcome::ToolWithReservedRoute],
        );
        let provider = FallbackProvider::new(primary, None);
        assert!(matches!(
            provider.chat(ChatRequest::simple("hi")).await,
            Err(ProviderError::InvalidRequest { message, .. })
                if message.contains("reserved fallback route")
        ));
    }

    #[tokio::test]
    async fn stream_provider_cannot_forge_reserved_route_metadata() {
        let (primary, _) = ScriptedMock::build(
            "openai",
            "gpt-primary",
            [ScriptedOutcome::ToolWithReservedRoute],
        );
        let provider = FallbackProvider::new(primary, None);
        let events = provider
            .chat_stream(ChatRequest::simple("hi"))
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            Err(ProviderError::InvalidRequest { message, .. })
                if message.contains("reserved fallback route")
        )));
    }

    #[tokio::test]
    async fn mixed_known_and_unknown_routes_never_fabricate_fallback_constraints() {
        let (primary, _) =
            ScriptedMock::build("openai", "known-primary", [ScriptedOutcome::RateLimited]);
        let (backup, backup_seen) = ScriptedMock::build_with_constraints(
            "compatible",
            "operator-model",
            false,
            [
                ScriptedOutcome::Tool("lookup"),
                ScriptedOutcome::Text("done"),
            ],
        );
        let provider = FallbackProvider::new(primary, Some(target(backup, "operator-model")));

        let mut request = ChatRequest::simple("look up the value");
        request.tools = vec![crate::ToolDefinition {
            name: "lookup".to_string(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object"}),
            concurrency: Default::default(),
        }];
        request.response_format = Some(ResponseFormat::Json);
        request.max_tokens = Some(5_000);

        // Before selection the wrapper reports the primary's known facts; it
        // never turns an unknown fallback into a fabricated conservative min.
        assert!(provider.model_constraints_known(&request));
        assert_eq!(
            provider.capabilities_for(&request).max_context_tokens,
            100_000
        );

        let first = provider.chat(request).await.unwrap();
        let followup = followup_request(first.tool_calls);
        assert!(!provider.model_constraints_known(&followup));
        assert_eq!(provider.capabilities_for(&followup).max_context_tokens, 1);

        // The deliberately false/1-token placeholder facts are ignored once
        // the operator-defined route is selected; protocol validation still
        // runs and the exact configured model is retained.
        let second = provider.chat(followup).await.unwrap();
        assert_eq!(second.content, "done");
        assert_eq!(
            backup_seen.lock().unwrap().as_slice(),
            &[
                Some("operator-model".to_string()),
                Some("operator-model".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn selection_of_a_smaller_known_fallback_fails_before_the_backup_call() {
        let (primary, _) =
            ScriptedMock::build("openai", "large-primary", [ScriptedOutcome::RateLimited]);
        let (backup, backup_seen) = ScriptedMock::build_with_capabilities(
            "anthropic",
            "tiny-backup",
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                structured_output: true,
                vision: true,
                reasoning: true,
                embeddings: false,
                max_context_tokens: 4,
                max_output_tokens: 10_000,
            },
            [ScriptedOutcome::Text("must not run")],
        );
        let provider = FallbackProvider::new(primary, Some(target(backup, "tiny-backup")));

        let error = provider
            .chat(ChatRequest::simple(
                "this request is deliberately longer than four tokens",
            ))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ProviderError::InvalidRequest { message, .. }
                if message.contains("fallback route context limit")
        ));
        assert!(backup_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn selected_fallback_accounts_for_requested_output_headroom() {
        let (primary, _) =
            ScriptedMock::build("openai", "large-primary", [ScriptedOutcome::RateLimited]);
        let (backup, backup_seen) = ScriptedMock::build_with_capabilities(
            "anthropic",
            "small-backup",
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                structured_output: true,
                vision: true,
                reasoning: true,
                embeddings: false,
                max_context_tokens: 100,
                max_output_tokens: 100,
            },
            [ScriptedOutcome::Text("must not run")],
        );
        let provider = FallbackProvider::new(primary, Some(target(backup, "small-backup")));
        let mut request = ChatRequest::simple("x");
        request.max_tokens = Some(99);

        let error = provider.chat(request).await.unwrap_err();
        assert!(matches!(
            error,
            ProviderError::InvalidRequest { message, .. }
                if message.contains("output headroom")
        ));
        assert!(backup_seen.lock().unwrap().is_empty());
    }
}
