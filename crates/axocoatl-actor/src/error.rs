#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("LLM provider error: {0}")]
    Provider(String),

    #[error("Token budget exceeded: used {used}, budget {budget}")]
    TokenBudgetExceeded { used: usize, budget: usize },

    #[error("Agent initialization failed: {0}")]
    InitFailed(String),

    #[error("Execution timeout after {seconds}s")]
    Timeout { seconds: u64 },

    #[error("Tool call failed: {tool} - {reason}")]
    ToolFailed { tool: String, reason: String },

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Internal error: {0}")]
    Internal(String),
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("Agent not found: {0}")]
    NotFound(String),

    #[error("Send failed: {0}")]
    SendFailed(String),
}
