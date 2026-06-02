/// Top-level framework error type.
#[derive(Debug, thiserror::Error)]
pub enum AxocoatlError {
    #[error("Agent error: {0}")]
    Agent(String),

    #[error("Provider error: {0}")]
    Provider(String),

    #[error("Token budget exceeded: used {used}, budget {budget}")]
    TokenBudgetExceeded { used: usize, budget: usize },

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Memory error: {0}")]
    Memory(String),

    #[error("Coordination error: {0}")]
    Coordination(String),

    #[error("Isolation error: {0}")]
    Isolation(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display() {
        let err = AxocoatlError::TokenBudgetExceeded {
            used: 5000,
            budget: 4000,
        };
        assert_eq!(
            err.to_string(),
            "Token budget exceeded: used 5000, budget 4000"
        );
    }

    #[test]
    fn error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let axocoatl_err: AxocoatlError = io_err.into();
        assert!(axocoatl_err.to_string().contains("file missing"));
    }
}
