mod convert;

use std::pin::Pin;

use tokio_stream::Stream;

use axocoatl_core::TokenUsageStats;
use axocoatl_llm::{
    provider_tool_metadata,
    transport::{
        bounded_redacted, http_client, network_error, next_stream_item, read_error_text, read_json,
        validated_endpoint, SseDecoder, RESPONSE_TIMEOUT, STREAM_IDLE_TIMEOUT,
        STREAM_TOTAL_TIMEOUT,
    },
    validate_chat_response, validate_provider_request, validate_required_stream_tool_call_ids,
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};

const OPENAI_API_BASE: &str = "https://api.openai.com/v1";

/// OpenAI LLM provider using async-openai 0.41.3 request/response types and a
/// bounded reqwest transport.
///
/// Reused by OpenAI-compatible vendors such as OpenRouter — point at their base URL and override the
/// `provider_id` so the registry keys it under their name.
pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    openrouter_attribution: bool,
    model: String,
    provider_id: String,
    /// Only the first-party endpoint has model ids whose constraints can be
    /// matched against Axocoatl's small, explicit verified registry.
    trusted_model_namespace: bool,
}

impl OpenAiProvider {
    fn capabilities_for_model(model: &str) -> ProviderCapabilities {
        let mut capabilities = ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            structured_output: true,
            // These are adapter protocol features for unknown ids. Callers
            // must consult `model_constraints_known` before locally rejecting
            // a request based on model-specific facts.
            vision: true,
            reasoning: false,
            embeddings: false,
            max_context_tokens: 0,
            max_output_tokens: 0,
        };
        if model == "gpt-4o" {
            capabilities.max_context_tokens = 128_000;
            capabilities.max_output_tokens = 16_384;
        }
        capabilities
    }

    fn model_constraints_known_for(model: &str) -> bool {
        model == "gpt-4o"
    }

    /// Create a new OpenAI provider with an API key and model name.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: http_client(),
            api_key: api_key.into(),
            base_url: OPENAI_API_BASE.to_string(),
            openrouter_attribution: false,
            model: model.into(),
            provider_id: "openai".to_string(),
            trusted_model_namespace: true,
        }
    }

    /// Create with a custom OpenAI-compatible base URL.
    pub fn with_base_url(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into();
        // OpenRouter app attribution (https://openrouter.ai/docs/app-attribution):
        // identifies Axocoatl in OpenRouter's app rankings. Set only for the
        // OpenRouter endpoint; other OpenAI-compatible vendors do not receive
        // headers they did not ask for.
        let openrouter_attribution = base_url.contains("openrouter.ai");
        Self {
            client: http_client(),
            api_key: api_key.into(),
            base_url: base_url.trim_end_matches('/').to_string(),
            openrouter_attribution,
            model: model.into(),
            provider_id: "openai".to_string(),
            trusted_model_namespace: false,
        }
    }

    /// Override the provider id so the registry keys this instance under a
    /// non-"openai" name (e.g., "openrouter"). Chainable.
    pub fn with_provider_id(mut self, id: impl Into<String>) -> Self {
        self.provider_id = id.into();
        self
    }

    fn endpoint(&self) -> Result<String, ProviderError> {
        validated_endpoint(&self.base_url, "chat/completions", &self.provider_id)
    }

    fn request_builder(
        &self,
        request: &async_openai::types::chat::CreateChatCompletionRequest,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let mut builder = self
            .client
            .post(self.endpoint()?)
            .bearer_auth(&self.api_key)
            .json(request);
        if self.openrouter_attribution {
            builder = builder
                .header("HTTP-Referer", "https://axocoatl.ai")
                .header("X-Title", "Axocoatl");
        }
        Ok(builder)
    }

    async fn checked_response(
        &self,
        response: reqwest::Response,
        model: &str,
    ) -> Result<reqwest::Response, ProviderError> {
        let status = response.status();
        if status == 429 {
            let retry_after_secs = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok());
            return Err(ProviderError::RateLimited {
                provider: self.provider_id.clone(),
                retry_after_secs,
            });
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ProviderError::AuthError {
                provider: self.provider_id.clone(),
            });
        }
        if status.as_u16() == 404 {
            return Err(ProviderError::ModelNotFound {
                provider: self.provider_id.clone(),
                model: model.to_string(),
            });
        }
        if !status.is_success() {
            let message = read_error_text(response, &[&self.api_key]).await;
            return Err(ProviderError::ApiError {
                provider: self.provider_id.clone(),
                status: status.as_u16(),
                message,
            });
        }
        Ok(response)
    }

    /// Build the async-openai chat request shared by `chat` and `chat_stream`.
    ///
    /// Critically this attaches `request.tools` so the model receives the tool
    /// definitions and can emit tool calls. Both entry points go through here so
    /// the two paths can never drift on what gets sent.
    fn build_chat_request(
        &self,
        request: &ChatRequest,
    ) -> Result<async_openai::types::chat::CreateChatCompletionRequest, ProviderError> {
        use async_openai::types::chat::{CreateChatCompletionRequestArgs, StopConfiguration};

        validate_provider_request(request, &self.provider_id)?;

        let openai_messages = convert::to_openai_messages(&request.messages)?;

        let mut req_builder = CreateChatCompletionRequestArgs::default();
        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);
        req_builder.model(model_for_call).messages(openai_messages);

        if let Some(max) = request.max_tokens {
            let max = u32::try_from(max).map_err(|_| ProviderError::InvalidRequest {
                provider: self.provider_id.clone(),
                message: "max_tokens exceeds the OpenAI-compatible integer range".to_string(),
            })?;
            req_builder.max_completion_tokens(max);
        }
        if let Some(temp) = request.temperature {
            req_builder.temperature(temp);
        }
        if let Some(top_p) = request.top_p {
            req_builder.top_p(top_p);
        }
        if request.response_format == Some(axocoatl_core::ResponseFormat::Json) {
            req_builder.response_format(async_openai::types::chat::ResponseFormat::JsonObject);
        }
        if !request.tools.is_empty() {
            req_builder.tools(convert::to_openai_tools(&request.tools));
        }
        if !request.stop_sequences.is_empty() {
            req_builder.stop(StopConfiguration::StringArray(
                request.stop_sequences.clone(),
            ));
        }

        req_builder
            .build()
            .map_err(|error| ProviderError::InvalidRequest {
                provider: self.provider_id.clone(),
                message: format!("failed to build OpenAI-compatible request: {error}"),
            })
    }
}

#[async_trait::async_trait]
impl LlmProvider for OpenAiProvider {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> ProviderCapabilities {
        Self::capabilities_for_model(&self.model)
    }

    fn capabilities_for(&self, request: &ChatRequest) -> ProviderCapabilities {
        Self::capabilities_for_model(request.model_override.as_deref().unwrap_or(&self.model))
    }

    fn model_constraints_known(&self, request: &ChatRequest) -> bool {
        self.trusted_model_namespace
            && Self::model_constraints_known_for(
                request.model_override.as_deref().unwrap_or(&self.model),
            )
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let openai_request = self.build_chat_request(&request)?;
        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);

        let response = self
            .request_builder(&openai_request)?
            .timeout(RESPONSE_TIMEOUT)
            .send()
            .await
            .map_err(|error| network_error(&error, &[&self.api_key]))?;
        let response = self.checked_response(response, model_for_call).await?;
        let response: async_openai::types::chat::CreateChatCompletionResponse =
            read_json(response, &self.provider_id).await?;

        if response.choices.len() != 1 || response.choices[0].index != 0 {
            return Err(ProviderError::ApiError {
                provider: self.provider_id.clone(),
                status: 200,
                message: "provider response must contain exactly choice index 0".to_string(),
            });
        }
        let choice = response
            .choices
            .into_iter()
            .next()
            .expect("length checked above");

        let mut tool_calls =
            convert::extract_tool_calls(&choice, &request.tools, &self.provider_id)?;
        for call in &mut tool_calls {
            call.provider_metadata = provider_tool_metadata(&self.provider_id);
        }
        let finish_reason = convert::map_finish_reason(&choice, &self.provider_id)?;

        let normalized = ChatResponse {
            content: choice.message.content.unwrap_or_default(),
            tool_calls,
            finish_reason,
            usage: TokenUsageStats {
                input_tokens: response
                    .usage
                    .as_ref()
                    .map(|u| u.prompt_tokens as usize)
                    .unwrap_or(0),
                output_tokens: response
                    .usage
                    .as_ref()
                    .map(|u| u.completion_tokens as usize)
                    .unwrap_or(0),
                reasoning_tokens: None,
            },
            model: response.model,
            provider: self.provider_id.clone(),
        };
        validate_chat_response(&self.provider_id, &normalized)?;
        Ok(normalized)
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        let mut openai_request = self.build_chat_request(&request)?;
        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);
        openai_request.stream = Some(true);
        openai_request.stream_options =
            Some(async_openai::types::chat::ChatCompletionStreamOptions {
                include_usage: Some(true),
                include_obfuscation: None,
            });

        let response = tokio::time::timeout(
            RESPONSE_TIMEOUT,
            self.request_builder(&openai_request)?.send(),
        )
        .await
        .map_err(|_| {
            ProviderError::Network(format!("{} response headers timed out", self.provider_id))
        })?
        .map_err(|error| network_error(&error, &[&self.api_key]))?;
        let response = self.checked_response(response, model_for_call).await?;
        let mut bytes = response.bytes_stream();
        let api_key = self.api_key.clone();
        let provider_id = self.provider_id.clone();

        let stream = async_stream::try_stream! {
            let mut decoder = SseDecoder::provider_default();
            let mut pending_finish = None;
            let mut saw_sentinel = false;
            let mut tool_call_ids = std::collections::BTreeMap::<usize, String>::new();
            let total_deadline = tokio::time::Instant::now() + STREAM_TOTAL_TIMEOUT;

            'response: loop {
                let next = next_stream_item(
                    &mut bytes,
                    total_deadline,
                    STREAM_IDLE_TIMEOUT,
                    &provider_id,
                )
                .await?;
                let reached_eof = next.is_none();
                let events = match next {
                    Some(chunk) => {
                        let chunk = chunk.map_err(|error| {
                            ProviderError::Stream(bounded_redacted(&error.to_string(), 8 * 1024, &[&api_key]))
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
                    let response: async_openai::types::chat::CreateChatCompletionStreamResponse =
                        serde_json::from_str(&event.data).map_err(|error| {
                            ProviderError::Stream(format!("invalid {provider_id} SSE JSON: {error}"))
                        })?;

                    if response.choices.len() > 1
                        || response.choices.first().is_some_and(|choice| choice.index != 0)
                    {
                        Err(ProviderError::Stream(format!(
                            "{provider_id} stream returned multiple alternatives or a nonzero choice index"
                        )))?;
                    }
                    if response.choices.is_empty() && response.usage.is_none() {
                        Err(ProviderError::Stream(format!(
                            "{provider_id} stream returned an empty non-usage frame"
                        )))?;
                    }

                    for choice in &response.choices {
                            // Text content deltas
                            if let Some(ref content) = choice.delta.content {
                                yield StreamEvent::TextDelta {
                                    delta: content.clone(),
                                };
                            }

                            // Tool call deltas. The `id` arrives only on the
                            // first chunk; later argument fragments are keyed by
                            // `index`, which we forward for correct accumulation.
                            if let Some(ref tool_calls) = choice.delta.tool_calls {
                                for tc in tool_calls {
                                    let index = tc.index as usize;
                                    let id = tc.id.clone().unwrap_or_default();
                                    let known_id = tool_call_ids.entry(index).or_default();
                                    if !id.is_empty() {
                                        if !known_id.is_empty() && known_id != &id {
                                            Err(ProviderError::Stream(format!(
                                                "{provider_id} changed a tool-call id for index {index}"
                                            )))?;
                                        }
                                        *known_id = id.clone();
                                    }
                                    let name = tc.function.as_ref().and_then(|f| f.name.clone());
                                    let args_delta = tc.function.as_ref()
                                        .and_then(|f| f.arguments.clone())
                                        .unwrap_or_default();
                                    yield StreamEvent::ToolCallDelta {
                                        index: Some(index),
                                        id: id.clone(),
                                        name,
                                        args_delta,
                                    };
                                    if tc.id.is_some()
                                        || tc.function.as_ref().and_then(|function| function.name.as_ref()).is_some()
                                    {
                                        yield StreamEvent::ToolCallMetadata {
                                            index: Some(index),
                                            id,
                                            metadata: provider_tool_metadata(&provider_id),
                                        };
                                    }
                                }
                            }

                            // Finish reason
                            if let Some(ref reason) = choice.finish_reason {
                                use async_openai::types::chat::FinishReason as OaiReason;
                                let finish = match reason {
                                    OaiReason::Stop => FinishReason::Stop,
                                    OaiReason::ToolCalls => FinishReason::ToolUse,
                                    OaiReason::Length => FinishReason::MaxTokens,
                                    OaiReason::ContentFilter => FinishReason::ContentFilter,
                                    OaiReason::FunctionCall => FinishReason::ToolUse,
                                };
                                pending_finish = Some(finish);
                            }
                        }

                    // Usage usually arrives after the finish_reason chunk. It
                    // must be yielded before Done because the actor stops at
                    // Done by contract.
                    if let Some(ref usage) = response.usage {
                        yield StreamEvent::Usage(TokenUsageStats {
                            input_tokens: usage.prompt_tokens as usize,
                            output_tokens: usage.completion_tokens as usize,
                            reasoning_tokens: None,
                        });
                    }

                }
                if reached_eof {
                    break;
                }
            }

            if !saw_sentinel {
                Err(ProviderError::Stream(format!("{provider_id} stream ended without the [DONE] terminal sentinel")))?;
            }
            let finish_reason = pending_finish.ok_or_else(|| {
                ProviderError::Stream(format!("{provider_id} stream terminated without a finish reason"))
            })?;
            validate_required_stream_tool_call_ids(
                &provider_id,
                &finish_reason,
                tool_call_ids.values().map(String::as_str),
            )?;
            yield StreamEvent::Done { finish_reason };
        };

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_llm::ToolDefinition;

    fn weather_tool() -> ToolDefinition {
        ToolDefinition {
            name: "get_weather".to_string(),
            description: "Get current weather".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "location": { "type": "string" } },
                "required": ["location"]
            }),
            concurrency: Default::default(),
        }
    }

    #[test]
    fn build_chat_request_attaches_tools() {
        let provider = OpenAiProvider::new("test-key", "gpt-4o");
        let mut request = ChatRequest::simple("What's the weather in NYC?");
        request.tools = vec![weather_tool()];

        let built = provider.build_chat_request(&request).unwrap();
        let json = serde_json::to_value(&built).unwrap();

        // Regression: the tool definitions must reach the outbound request.
        assert!(json["tools"].is_array(), "tools must be sent to the model");
        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "get_weather");
    }

    #[test]
    fn build_chat_request_omits_tools_when_none() {
        let provider = OpenAiProvider::new("test-key", "gpt-4o");
        let request = ChatRequest::simple("Hello");

        let built = provider.build_chat_request(&request).unwrap();
        let json = serde_json::to_value(&built).unwrap();

        assert!(json.get("tools").is_none() || json["tools"].is_null());
    }

    #[test]
    fn build_chat_request_forwards_max_tokens_and_stop_sequences() {
        let provider = OpenAiProvider::new("test-key", "gpt-4o");
        let mut request = ChatRequest::simple("Hello");
        request.max_tokens = Some(321);
        request.stop_sequences = vec!["END".to_string(), "STOP".to_string()];

        let json = serde_json::to_value(provider.build_chat_request(&request).unwrap()).unwrap();
        assert_eq!(json["max_completion_tokens"], 321);
        assert_eq!(json["stop"], serde_json::json!(["END", "STOP"]));
    }

    #[tokio::test]
    async fn nonstream_tool_calls_fail_closed_on_malformed_arguments() {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let response = serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-test",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "get_weather", "arguments": "{" }
                    }]
                },
                "finish_reason": "tool_calls",
                "logprobs": null
            }],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 1,
                "total_tokens": 2
            }
        });
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::with_base_url("key", "gpt-test", format!("{}/v1", server.uri()));
        let mut request = ChatRequest::simple("weather?");
        request.tools = vec![weather_tool()];
        assert!(matches!(
            provider.chat(request).await,
            Err(ProviderError::ApiError { message, .. })
                if message.contains("malformed tool-call arguments")
        ));
    }

    #[tokio::test]
    async fn nonstream_tool_calls_fail_closed_on_empty_native_id() {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let response = serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-test",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "",
                        "type": "function",
                        "function": { "name": "get_weather", "arguments": "{}" }
                    }]
                },
                "finish_reason": "tool_calls",
                "logprobs": null
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        });
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::with_base_url("key", "gpt-test", format!("{}/v1", server.uri()));
        let mut request = ChatRequest::simple("weather?");
        request.tools = vec![weather_tool()];
        assert!(matches!(
            provider.chat(request).await,
            Err(ProviderError::ApiError { message, .. }) if message.contains("empty id")
        ));
    }

    #[tokio::test]
    async fn nonstream_mixed_function_and_custom_calls_fail_closed() {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let response = serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-test",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "get_weather", "arguments": "{}" }
                        },
                        {
                            "id": "custom_1",
                            "type": "custom",
                            "custom_tool": { "name": "unavailable", "input": "opaque" }
                        }
                    ]
                },
                "finish_reason": "tool_calls",
                "logprobs": null
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        });
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::with_base_url("key", "gpt-test", format!("{}/v1", server.uri()));
        let mut request = ChatRequest::simple("weather?");
        request.tools = vec![weather_tool()];
        assert!(matches!(
            provider.chat(request).await,
            Err(ProviderError::ApiError { message, .. }) if message.contains("unsupported custom")
        ));
    }

    #[tokio::test]
    async fn stream_tool_calls_fail_closed_on_empty_native_id() {
        use tokio_stream::StreamExt;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"get_weather\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}],\"created\":1,\"model\":\"gpt-test\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"created\":1,\"model\":\"gpt-test\",\"object\":\"chat.completion.chunk\"}\n\n",
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

        let provider =
            OpenAiProvider::with_base_url("key", "gpt-test", format!("{}/v1", server.uri()));
        let mut request = ChatRequest::simple("weather?");
        request.tools = vec![weather_tool()];
        let events = provider
            .chat_stream(request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            Err(ProviderError::Stream(message)) if message.contains("empty id")
        )));
        assert!(!events
            .iter()
            .any(|event| matches!(event, Ok(StreamEvent::Done { .. }))));
    }

    #[tokio::test]
    async fn stream_multiple_choice_alternatives_never_merge_into_parallel_calls() {
        use tokio_stream::StreamExt;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"id\":\"x\",\"choices\":[",
            "{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{}\"}}]},\"finish_reason\":null},",
            "{\"index\":1,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_b\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}],",
            "\"created\":1,\"model\":\"gpt-test\",\"object\":\"chat.completion.chunk\"}\n\n",
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

        let provider =
            OpenAiProvider::with_base_url("key", "gpt-test", format!("{}/v1", server.uri()));
        let mut request = ChatRequest::simple("weather?");
        request.tools = vec![weather_tool()];
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

    #[test]
    fn model_override_drives_exact_openai_capabilities() {
        let provider = OpenAiProvider::new("key", "gpt-3.5-turbo");
        let base_request = ChatRequest::simple("look");
        assert!(!provider.model_constraints_known(&base_request));
        assert_eq!(provider.capabilities().max_context_tokens, 0);
        let mut request = ChatRequest::simple("look");
        request.model_override = Some("gpt-4o".to_string());
        let capabilities = provider.capabilities_for(&request);
        assert!(capabilities.vision);
        assert_eq!(capabilities.max_context_tokens, 128_000);
        assert_eq!(capabilities.max_output_tokens, 16_384);
        assert!(provider.model_constraints_known(&request));
    }

    #[test]
    fn compatible_endpoint_never_claims_known_model_constraints() {
        let provider =
            OpenAiProvider::with_base_url("key", "gpt-4o", "https://compatible.invalid/v1");
        let request = ChatRequest::simple("look");
        assert!(!provider.model_constraints_known(&request));
    }

    #[tokio::test]
    async fn stream_yields_usage_before_done_and_requires_terminal_sentinel() {
        use tokio_stream::StreamExt;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}],\"created\":1,\"model\":\"gpt-test\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"created\":1,\"model\":\"gpt-test\",\"object\":\"chat.completion.chunk\"}\n\n",
            "data: {\"id\":\"x\",\"choices\":[],\"created\":1,\"model\":\"gpt-test\",\"object\":\"chat.completion.chunk\",\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}}\n\n",
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

        let provider =
            OpenAiProvider::with_base_url("test-key", "gpt-test", format!("{}/v1", server.uri()));
        let events: Vec<_> = provider
            .chat_stream(ChatRequest::simple("hi"))
            .await
            .unwrap()
            .collect()
            .await;
        let usage_index = events
            .iter()
            .position(|event| matches!(event, Ok(StreamEvent::Usage(_))))
            .unwrap();
        let done_index = events
            .iter()
            .position(|event| matches!(event, Ok(StreamEvent::Done { .. })))
            .unwrap();
        assert!(usage_index < done_index);
        assert!(events.iter().all(Result::is_ok));
    }

    #[tokio::test]
    async fn partial_stream_without_terminal_sentinel_fails_explicitly() {
        use tokio_stream::StreamExt;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let partial = "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}],\"created\":1,\"model\":\"gpt-test\",\"object\":\"chat.completion.chunk\"}\n\n";
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(partial),
            )
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::with_base_url("test-key", "gpt-test", format!("{}/v1", server.uri()));
        let events: Vec<_> = provider
            .chat_stream(ChatRequest::simple("hi"))
            .await
            .unwrap()
            .collect()
            .await;
        assert!(events.iter().any(
            |event| matches!(event, Ok(StreamEvent::TextDelta { delta }) if delta == "partial")
        ));
        assert!(events.iter().any(|event| {
            matches!(event, Err(ProviderError::Stream(message)) if message.contains("without the [DONE]"))
        }));
        assert!(!events
            .iter()
            .any(|event| matches!(event, Ok(StreamEvent::Done { .. }))));
    }

    #[tokio::test]
    async fn api_error_body_is_bounded_and_redacts_the_key() {
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let api_key = "launch-secret-key";
        let body = format!("server echoed {api_key} {}", "x".repeat(70 * 1024));
        Mock::given(wiremock::matchers::method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string(body))
            .mount(&server)
            .await;

        let provider =
            OpenAiProvider::with_base_url(api_key, "gpt-test", format!("{}/v1", server.uri()));
        let error = provider.chat(ChatRequest::simple("hi")).await.unwrap_err();
        let ProviderError::ApiError { message, .. } = error else {
            panic!("expected bounded API error");
        };
        assert!(!message.contains(api_key));
        assert!(message.contains("[REDACTED]"));
        assert!(message.contains("truncated"));
        assert!(message.len() <= axocoatl_llm::transport::MAX_ERROR_BODY_BYTES);
    }
}
