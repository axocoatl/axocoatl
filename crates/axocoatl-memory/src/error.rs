#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization failed: {0}")]
    Serialization(String),

    #[error("Deserialization failed: {0}")]
    Deserialization(String),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Vector database error: {0}")]
    VectorDb(String),

    #[error("Embedding generation failed: {0}")]
    Embedding(String),

    #[error("Memory not initialized")]
    NotInitialized,

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid: {0}")]
    Invalid(String),

    #[error("Consolidation LLM call failed: {0}")]
    ConsolidationLlmFailed(String),
}
