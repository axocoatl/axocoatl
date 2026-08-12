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

    #[error("Session error: {0}")]
    Session(String),

    /// A request conflicts with the lifecycle of the session's current attempt
    /// set (for example, starting a second set before keeping or discarding the
    /// first). Kept distinct so the HTTP boundary can return 409 instead of
    /// flattening a safe concurrency guard into a generic bad request.
    #[error("Attempt conflict: {0}")]
    AttemptConflict(String),
}
