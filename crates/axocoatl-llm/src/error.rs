#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("Invalid request for {provider}: {message}")]
    InvalidRequest { provider: String, message: String },

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

    /// The stream closed before the provider's own completion record, so no
    /// tool call from it was released and its usage is unknown.
    #[error("{provider} stream ended without a complete response: {message}")]
    IncompleteStream { provider: String, message: String },

    #[error("Provider not found: {0}")]
    ProviderNotFound(String),

    /// The host's budget for this caller cannot admit the call. `message`
    /// names the limit in plain words; it is not a provider fault.
    #[error("{message}")]
    BudgetExhausted { provider: String, message: String },
}
