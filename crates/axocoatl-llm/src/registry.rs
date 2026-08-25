use std::collections::HashMap;
use std::sync::Arc;

use crate::provider::LlmProvider;

/// Registry of available LLM providers, keyed by provider id.
///
/// Rate-limit fallback is handled per-agent by [`crate::FallbackProvider`],
/// which wraps a primary provider with a backup — see the daemon's agent
/// spawn path. The registry itself is a plain lookup table.
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    /// Register a provider. Uses `provider_id()` as the key.
    pub fn register(&mut self, provider: Arc<dyn LlmProvider>) {
        self.providers
            .insert(provider.provider_id().to_string(), provider);
    }

    /// Get a provider by ID.
    pub fn get(&self, provider_id: &str) -> Option<&Arc<dyn LlmProvider>> {
        self.providers.get(provider_id)
    }

    /// List all registered provider IDs.
    pub fn provider_ids(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
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
    use crate::error::ProviderError;
    use crate::provider::{ChatRequest, ChatResponse, ProviderCapabilities, StreamEvent};
    use std::pin::Pin;
    use tokio_stream::Stream;

    struct MockProvider {
        id: String,
    }

    impl MockProvider {
        /// Returns the mock already boxed as a provider (not `Self`).
        fn arc(id: &str) -> Arc<dyn LlmProvider> {
            Arc::new(Self { id: id.to_string() })
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
            unimplemented!("registry tests exercise lookup only")
        }
        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            unimplemented!("registry tests exercise lookup only")
        }
    }

    #[test]
    fn register_and_get_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register(MockProvider::arc("openai"));
        assert!(registry.get("openai").is_some());
        assert!(registry.get("anthropic").is_none());
    }

    #[test]
    fn list_provider_ids() {
        let mut registry = ProviderRegistry::new();
        registry.register(MockProvider::arc("openai"));
        registry.register(MockProvider::arc("anthropic"));
        let mut ids = registry.provider_ids();
        ids.sort();
        assert_eq!(ids, vec!["anthropic", "openai"]);
    }
}
