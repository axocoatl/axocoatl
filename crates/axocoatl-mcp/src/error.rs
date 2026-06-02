#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("Tool not found: {0}")]
    ToolNotFound(String),

    #[error("Server not found: {0}")]
    ServerNotFound(String),

    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    #[error("Tool call failed: {0}")]
    CallFailed(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
