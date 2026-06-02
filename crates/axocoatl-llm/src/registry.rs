use std::collections::HashMap;
use std::sync::Arc;

use crate::error::ProviderError;
use crate::provider::{ChatRequest, ChatResponse, LlmProvider};

/// Registry of available providers with fallback chain support.
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
    fallback_chains: HashMap<String, Vec<String>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            fallback_chains: HashMap::new(),
        }
    }

    /// Register a provider. Uses `provider_id()` as the key.
    pub fn register(&mut self, provider: Arc<dyn LlmProvider>) {
        self.providers
            .insert(provider.provider_id().to_string(), provider);
    }

    /// Set a fallback chain: when `primary` is rate-limited, try `fallbacks` in order.
    pub fn set_fallback_chain(&mut self, primary: &str, fallbacks: Vec<String>) {
        self.fallback_chains.insert(primary.to_string(), fallbacks);
    }

    /// Get a provider by ID.
    pub fn get(&self, provider_id: &str) -> Option<&Arc<dyn LlmProvider>> {
        self.providers.get(provider_id)
    }

    /// List all registered provider IDs.
    pub fn provider_ids(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    /// Execute a chat request with automatic fallback on rate limiting.
    pub async fn chat_with_fallback(
        &self,
        primary_provider: &str,
        request: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let chain = std::iter::once(primary_provider.to_string()).chain(
            self.fallback_chains
                .get(primary_provider)
                .cloned()
                .unwrap_or_default()
                .into_iter(),
        );

        let mut last_err = None;
        for provider_id in chain {
            match self.providers.get(&provider_id) {
                Some(p) => match p.chat(request.clone()).await {
                    Ok(resp) => return Ok(resp),
                    Err(ProviderError::RateLimited { .. }) => {
                        tracing::warn!(provider = %provider_id, "Rate limited, trying fallback");
                        last_err = Some(ProviderError::RateLimited {
                            provider: provider_id,
                            retry_after_secs: None,
                        });
                        continue;
                    }
                    Err(e) => return Err(e),
                },
                None => {
                    tracing::error!(provider = %provider_id, "Provider not registered");
                    continue;
                }
            }
        }

        Err(last_err.unwrap_or(ProviderError::ProviderNotFound(
            primary_provider.to_string(),
        )))
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatResponse, FinishReason, ProviderCapabilities, StreamEvent};
    use axocoatl_core::TokenUsageStats;
    use std::pin::Pin;
    use tokio_stream::Stream;

    /// A mock provider for testing.
    struct MockProvider {
        id: String,
        response: Result<ChatResponse, ProviderError>,
    }

    impl MockProvider {
        fn ok(id: &str, content: &str) -> Arc<dyn LlmProvider> {
            Arc::new(Self {
                id: id.to_string(),
                response: Ok(ChatResponse {
                    content: content.to_string(),
                    tool_calls: vec![],
                    finish_reason: FinishReason::Stop,
                    usage: TokenUsageStats::new(10, 5),
                    model: "test-model".to_string(),
                    provider: id.to_string(),
                }),
            })
        }

        fn rate_limited(id: &str) -> Arc<dyn LlmProvider> {
            Arc::new(Self {
                id: id.to_string(),
                response: Err(ProviderError::RateLimited {
                    provider: id.to_string(),
                    retry_after_secs: Some(5),
                }),
            })
        }

        fn auth_error(id: &str) -> Arc<dyn LlmProvider> {
            Arc::new(Self {
                id: id.to_string(),
                response: Err(ProviderError::AuthError {
                    provider: id.to_string(),
                }),
            })
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for MockProvider {
        fn provider_id(&self) -> &str {
            &self.id
        }
        fn model_id(&self) -> &str {
            "mock-model"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
            match &self.response {
                Ok(r) => Ok(r.clone()),
                Err(ProviderError::RateLimited {
                    provider,
                    retry_after_secs,
                }) => Err(ProviderError::RateLimited {
                    provider: provider.clone(),
                    retry_after_secs: *retry_after_secs,
                }),
                Err(ProviderError::AuthError { provider }) => Err(ProviderError::AuthError {
                    provider: provider.clone(),
                }),
                Err(_) => Err(ProviderError::ApiError {
                    provider: self.id.clone(),
                    status: 500,
                    message: "mock error".to_string(),
                }),
            }
        }
        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            Err(ProviderError::Stream(
                "streaming not implemented in mock".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn register_and_get_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register(MockProvider::ok("openai", "hello"));
        assert!(registry.get("openai").is_some());
        assert!(registry.get("anthropic").is_none());
    }

    #[tokio::test]
    async fn list_provider_ids() {
        let mut registry = ProviderRegistry::new();
        registry.register(MockProvider::ok("openai", "hi"));
        registry.register(MockProvider::ok("anthropic", "hi"));
        let mut ids = registry.provider_ids();
        ids.sort();
        assert_eq!(ids, vec!["anthropic", "openai"]);
    }

    #[tokio::test]
    async fn chat_with_fallback_primary_succeeds() {
        let mut registry = ProviderRegistry::new();
        registry.register(MockProvider::ok("openai", "response from openai"));
        registry.register(MockProvider::ok("anthropic", "response from anthropic"));
        registry.set_fallback_chain("openai", vec!["anthropic".to_string()]);

        let result = registry
            .chat_with_fallback("openai", ChatRequest::simple("test"))
            .await;
        let resp = result.unwrap();
        assert_eq!(resp.provider, "openai");
        assert_eq!(resp.content, "response from openai");
    }

    #[tokio::test]
    async fn chat_with_fallback_primary_rate_limited() {
        let mut registry = ProviderRegistry::new();
        registry.register(MockProvider::rate_limited("openai"));
        registry.register(MockProvider::ok("anthropic", "fallback response"));
        registry.set_fallback_chain("openai", vec!["anthropic".to_string()]);

        let result = registry
            .chat_with_fallback("openai", ChatRequest::simple("test"))
            .await;
        let resp = result.unwrap();
        assert_eq!(resp.provider, "anthropic");
        assert_eq!(resp.content, "fallback response");
    }

    #[tokio::test]
    async fn chat_with_fallback_all_rate_limited() {
        let mut registry = ProviderRegistry::new();
        registry.register(MockProvider::rate_limited("openai"));
        registry.register(MockProvider::rate_limited("anthropic"));
        registry.set_fallback_chain("openai", vec!["anthropic".to_string()]);

        let result = registry
            .chat_with_fallback("openai", ChatRequest::simple("test"))
            .await;
        assert!(matches!(result, Err(ProviderError::RateLimited { .. })));
    }

    #[tokio::test]
    async fn chat_with_fallback_non_rate_limit_error_not_retried() {
        let mut registry = ProviderRegistry::new();
        registry.register(MockProvider::auth_error("openai"));
        registry.register(MockProvider::ok("anthropic", "should not reach"));
        registry.set_fallback_chain("openai", vec!["anthropic".to_string()]);

        let result = registry
            .chat_with_fallback("openai", ChatRequest::simple("test"))
            .await;
        // Auth errors are NOT retried — they propagate immediately
        assert!(matches!(result, Err(ProviderError::AuthError { .. })));
    }

    #[tokio::test]
    async fn chat_with_fallback_no_provider() {
        let registry = ProviderRegistry::new();
        let result = registry
            .chat_with_fallback("nonexistent", ChatRequest::simple("test"))
            .await;
        assert!(matches!(result, Err(ProviderError::ProviderNotFound(_))));
    }

    #[tokio::test]
    async fn chat_with_fallback_no_chain() {
        let mut registry = ProviderRegistry::new();
        registry.register(MockProvider::ok("openai", "direct response"));
        // No fallback chain set — should still work with just the primary
        let result = registry
            .chat_with_fallback("openai", ChatRequest::simple("test"))
            .await;
        assert_eq!(result.unwrap().content, "direct response");
    }
}
