use crate::error::ProviderError;
use crate::provider::{ChatRequest, ChatResponse, LlmProvider};

/// Extension trait for providers with reasoning / extended-thinking models.
#[async_trait::async_trait]
pub trait ReasoningProvider: LlmProvider {
    /// Chat with a reasoning budget — returns both the response and the reasoning trace.
    async fn chat_with_reasoning(
        &self,
        request: ChatRequest,
        reasoning_budget: usize,
    ) -> Result<(ChatResponse, String), ProviderError>;
}

/// Extension trait for providers with embedding generation.
#[async_trait::async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Generate embeddings for a batch of texts.
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, ProviderError>;

    /// Dimensionality of the embedding vectors.
    fn embedding_dimensions(&self) -> usize;
}
