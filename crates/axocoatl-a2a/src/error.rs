#[derive(Debug, thiserror::Error)]
pub enum A2AError {
    #[error("Discovery failed: {0}")]
    DiscoveryFailed(String),

    #[error("Task submission failed: {0}")]
    TaskFailed(String),

    #[error("Authentication failed: {0}")]
    AuthFailed(String),

    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
