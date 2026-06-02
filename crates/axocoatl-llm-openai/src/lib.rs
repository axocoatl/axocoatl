mod convert;

use std::pin::Pin;

use async_openai::config::OpenAIConfig;
use async_openai::Client;
use tokio_stream::Stream;

use axocoatl_core::TokenUsageStats;
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};

/// OpenAI LLM provider using async-openai 0.33.
///
/// Reused by the OpenAI-compatible vendors (OpenRouter, Azure OpenAI,
/// LM Studio, etc.) — point at their base URL and override the
/// `provider_id` so the registry keys it under their name.
pub struct OpenAiProvider {
    client: Client<OpenAIConfig>,
    model: String,
    provider_id: String,
}

impl OpenAiProvider {
    /// Create a new OpenAI provider with an API key and model name.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let config = OpenAIConfig::new().with_api_key(api_key);
        Self {
            client: Client::with_config(config),
            model: model.into(),
            provider_id: "openai".to_string(),
        }
    }

    /// Create with a custom base URL (for Azure OpenAI, LM Studio, or compatible APIs).
    pub fn with_base_url(
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        let config = OpenAIConfig::new()
            .with_api_key(api_key)
            .with_api_base(base_url);
        Self {
            client: Client::with_config(config),
            model: model.into(),
            provider_id: "openai".to_string(),
        }
    }

    /// Override the provider id so the registry keys this instance under a
    /// non-"openai" name (e.g., "openrouter"). Chainable.
    pub fn with_provider_id(mut self, id: impl Into<String>) -> Self {
        self.provider_id = id.into();
        self
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
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            structured_output: true,
            vision: self.model.contains("4o") || self.model.contains("vision"),
            reasoning: self.model.starts_with("o1") || self.model.starts_with("o3"),
            embeddings: false,
            max_context_tokens: match self.model.as_str() {
                m if m.contains("128k") => 128_000,
                "gpt-4o" | "gpt-4o-mini" => 128_000,
                _ => 8_192,
            },
            max_output_tokens: 4_096,
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        use async_openai::types::chat::CreateChatCompletionRequestArgs;

        let openai_messages = convert::to_openai_messages(&request.messages)?;

        let mut req_builder = CreateChatCompletionRequestArgs::default();
        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);
        req_builder.model(model_for_call).messages(openai_messages);

        if let Some(max) = request.max_tokens {
            req_builder.max_completion_tokens(max as u32);
        }
        if let Some(temp) = request.temperature {
            req_builder.temperature(temp);
        }

        let openai_request = req_builder.build().map_err(|e| ProviderError::ApiError {
            provider: "openai".to_string(),
            status: 0,
            message: format!("Failed to build request: {e}"),
        })?;

        let response = self
            .client
            .chat()
            .create(openai_request)
            .await
            .map_err(convert::map_openai_error)?;

        let choice =
            response
                .choices
                .into_iter()
                .next()
                .ok_or_else(|| ProviderError::ApiError {
                    provider: "openai".to_string(),
                    status: 200,
                    message: "No choices in response".to_string(),
                })?;

        let tool_calls = convert::extract_tool_calls(&choice);
        let finish_reason = convert::map_finish_reason(&choice);

        Ok(ChatResponse {
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
            provider: "openai".to_string(),
        })
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        use async_openai::types::chat::CreateChatCompletionRequestArgs;
        use tokio_stream::StreamExt;

        let openai_messages = convert::to_openai_messages(&request.messages)?;

        let mut req_builder = CreateChatCompletionRequestArgs::default();
        let model_for_call = request.model_override.as_deref().unwrap_or(&self.model);
        req_builder.model(model_for_call).messages(openai_messages);

        if let Some(max) = request.max_tokens {
            req_builder.max_completion_tokens(max as u32);
        }
        if let Some(temp) = request.temperature {
            req_builder.temperature(temp);
        }

        let openai_request = req_builder.build().map_err(|e| ProviderError::ApiError {
            provider: "openai".to_string(),
            status: 0,
            message: format!("Failed to build request: {e}"),
        })?;

        let mut openai_stream = self
            .client
            .chat()
            .create_stream(openai_request)
            .await
            .map_err(convert::map_openai_error)?;

        // Convert async-openai stream → Axocoatl StreamEvent stream
        let stream = async_stream::try_stream! {
            while let Some(result) = openai_stream.next().await {
                match result {
                    Ok(response) => {
                        for choice in &response.choices {
                            // Text content deltas
                            if let Some(ref content) = choice.delta.content {
                                yield StreamEvent::TextDelta {
                                    delta: content.clone(),
                                };
                            }

                            // Tool call deltas
                            if let Some(ref tool_calls) = choice.delta.tool_calls {
                                for tc in tool_calls {
                                    let id = tc.id.clone().unwrap_or_default();
                                    let name = tc.function.as_ref().and_then(|f| f.name.clone());
                                    let args_delta = tc.function.as_ref()
                                        .and_then(|f| f.arguments.clone())
                                        .unwrap_or_default();
                                    yield StreamEvent::ToolCallDelta {
                                        id,
                                        name,
                                        args_delta,
                                    };
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
                                yield StreamEvent::Done { finish_reason: finish };
                            }
                        }

                        // Usage stats (available when stream_options.include_usage is set)
                        if let Some(ref usage) = response.usage {
                            yield StreamEvent::Usage(TokenUsageStats {
                                input_tokens: usage.prompt_tokens as usize,
                                output_tokens: usage.completion_tokens as usize,
                                reasoning_tokens: None,
                            });
                        }
                    }
                    Err(e) => {
                        Err(ProviderError::Stream(e.to_string()))?;
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }
}
