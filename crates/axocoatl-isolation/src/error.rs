#[derive(Debug, thiserror::Error)]
pub enum IsolationError {
    #[error("OCI container setup failed: {0}")]
    OciSetupFailed(String),

    #[error("OCI container execution failed: {0}")]
    OciContainerFailed(String),

    #[error("Tool execution timed out after {0:?}")]
    Timeout(std::time::Duration),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("E2B sandbox error: {0}")]
    E2b(String),
}
