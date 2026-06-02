#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("Configuration error: {0}")]
    Config(#[from] axocoatl_config::ConfigError),

    #[error("Provider setup failed: {0}")]
    Provider(String),

    #[error("Agent spawn failed: {0}")]
    AgentSpawn(String),

    #[error("MCP connection failed: {0}")]
    Mcp(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Workflow not found: {0}")]
    WorkflowNotFound(String),

    #[error("Workflow execution failed: {0}")]
    WorkflowExecution(String),

    #[error("Workflow timed out after {0} seconds")]
    WorkflowTimeout(u64),

    #[error("Session error: {0}")]
    Session(String),
}
