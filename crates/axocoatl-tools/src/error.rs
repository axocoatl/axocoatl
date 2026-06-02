#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("Tool not found: {0}")]
    NotFound(String),

    #[error("Tool execution failed: {tool} — {reason}")]
    ExecutionFailed { tool: String, reason: String },

    #[error("Invalid arguments for tool {tool}: {reason}")]
    InvalidArgs { tool: String, reason: String },

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}
