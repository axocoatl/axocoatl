#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("Rate limited by {provider}. Retry after {retry_after_secs:?}s")]
    RateLimited {
        provider: String,
        retry_after_secs: Option<u64>,
    },

    #[error("Context length exceeded: {tokens_used} tokens, limit {limit}")]
    ContextLengthExceeded { tokens_used: usize, limit: usize },

    #[error("Content filtered by {provider}: {reason}")]
    ContentFiltered { provider: String, reason: String },

    #[error("Authentication failed for {provider}")]
    AuthError { provider: String },

    #[error("Model not found: {model} on {provider}")]
    ModelNotFound { provider: String, model: String },

    #[error("Network error: {0}")]
    Network(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("{provider} API error: {status} - {message}")]
    ApiError {
        provider: String,
        status: u16,
        message: String,
    },

    #[error("Streaming error: {0}")]
    Stream(String),

    #[error("Provider not found: {0}")]
    ProviderNotFound(String),
}
