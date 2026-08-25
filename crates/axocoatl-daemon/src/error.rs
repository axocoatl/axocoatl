#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("Configuration error: {0}")]
    Config(#[source] Box<axocoatl_config::ConfigError>),

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

    /// A structural Automation failure after provider work had already begun.
    /// The known subtotal travels with the error so WebSocket/run-history
    /// boundaries never turn a failed paid run into an apparent zero-token run.
    #[error("Workflow execution failed: {message}")]
    WorkflowExecutionMeasured {
        message: String,
        token_usage: axocoatl_core::TokenUsageStats,
        token_usage_known: bool,
    },

    #[error("Session error: {0}")]
    Session(String),

    /// A Session failure after provider work had already begun. The subtotal
    /// remains attached even if persisting the terminal ledger transition also
    /// fails, when reading the ledger alone cannot recover the accounting.
    #[error("{message}")]
    SessionExecutionMeasured {
        message: String,
        token_usage: axocoatl_core::TokenUsageStats,
        token_usage_known: bool,
    },

    /// A safe Session concurrency/integrity guard (for example attempting to
    /// detach context that a canonical turn already pins).
    #[error("Session conflict: {0}")]
    SessionConflict(String),

    /// An idempotent resend attached to the exact turn that is already live.
    /// This is a non-terminal dispatch disposition; the daemon intentionally
    /// does not publish a SessionError for it.
    #[error("session {session} is already running turn {turn}")]
    SessionTurnReattached { session: String, turn: String },

    /// A request conflicts with the lifecycle of the session's current attempt
    /// set (for example, starting a second set before keeping or discarding the
    /// first). Kept distinct so the HTTP boundary can return 409 instead of
    /// flattening a safe concurrency guard into a generic bad request.
    #[error("Attempt conflict: {0}")]
    AttemptConflict(String),
}

impl From<axocoatl_config::ConfigError> for DaemonError {
    fn from(error: axocoatl_config::ConfigError) -> Self {
        Self::Config(Box::new(error))
    }
}

impl DaemonError {
    pub fn workflow_execution_measured(
        error: impl std::fmt::Display,
        token_usage: axocoatl_core::TokenUsageStats,
        token_usage_known: bool,
    ) -> Self {
        Self::WorkflowExecutionMeasured {
            message: error.to_string(),
            token_usage,
            token_usage_known,
        }
    }

    pub fn workflow_token_usage(&self) -> Option<(&axocoatl_core::TokenUsageStats, bool)> {
        match self {
            Self::WorkflowExecutionMeasured {
                token_usage,
                token_usage_known,
                ..
            } => Some((token_usage, *token_usage_known)),
            _ => None,
        }
    }

    pub fn session_execution_measured(
        error: impl std::fmt::Display,
        token_usage: axocoatl_core::TokenUsageStats,
        token_usage_known: bool,
    ) -> Self {
        Self::SessionExecutionMeasured {
            message: error.to_string(),
            token_usage,
            token_usage_known,
        }
    }

    pub fn session_token_usage(&self) -> Option<(&axocoatl_core::TokenUsageStats, bool)> {
        match self {
            Self::SessionExecutionMeasured {
                token_usage,
                token_usage_known,
                ..
            } => Some((token_usage, *token_usage_known)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measured_session_error_keeps_subtotal_and_original_message() {
        let error = DaemonError::session_execution_measured(
            DaemonError::Session("provider stream ended".into()),
            axocoatl_core::TokenUsageStats::new(13, 8).with_reasoning(3),
            false,
        );
        let (usage, known) = error.session_token_usage().unwrap();
        assert_eq!(usage.total(), 24);
        assert!(!known);
        assert_eq!(error.to_string(), "Session error: provider stream ended");
    }
}
