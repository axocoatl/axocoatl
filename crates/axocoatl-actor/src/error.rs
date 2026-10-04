#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("LLM provider error: {0}")]
    Provider(String),

    /// The provider stream closed before its completion event. No tool call
    /// from that response was released; its usage is unknown.
    #[error("LLM provider stream ended early: {0}")]
    IncompleteProviderStream(String),

    /// The Agent's own token guard (`token_budget`) stopped a model call.
    #[error(
        "This Agent reached its token limit for this activation ({} tokens; {} needed). \
         Raise the Agent's token budget or narrow the task.",
        group_digits(*.budget),
        group_digits(*.used)
    )]
    TokenBudgetExceeded { used: usize, budget: usize },

    /// The host's budget for this Agent (a Session grant) cannot admit
    /// another model call. The text names the limit in plain words.
    #[error("{0}")]
    BudgetExhausted(String),

    #[error("Current request needs {required} context tokens, exceeding limit {limit}")]
    ContextLimitExceeded { required: usize, limit: usize },

    #[error("Agent initialization failed: {0}")]
    InitFailed(String),

    #[error("Execution timeout after {seconds}s")]
    Timeout { seconds: u64 },

    #[error("Tool call failed: {tool} - {reason}")]
    ToolFailed { tool: String, reason: String },

    /// The activation ran every tool round its limit allows and the model
    /// still asked for tools. `pending` names them; none of them ran.
    #[error(
        "This Agent reached its tool-round limit for this activation ({} rounds) and still \
         asked for {pending}; those calls did not run. Run it again to go on, or narrow the \
         task.",
        group_digits(*.limit)
    )]
    ToolRoundLimit { limit: usize, pending: String },

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl From<axocoatl_llm::ProviderError> for AgentError {
    /// A host budget refusal keeps its own words; anything else is a
    /// provider error.
    fn from(error: axocoatl_llm::ProviderError) -> Self {
        match error {
            axocoatl_llm::ProviderError::BudgetExhausted { message, .. } => {
                Self::BudgetExhausted(message)
            }
            error => Self::Provider(error.to_string()),
        }
    }
}

/// `1234567` as `1,234,567`.
pub(crate) fn group_digits(value: usize) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("Agent not found: {0}")]
    NotFound(String),

    #[error("Send failed: {0}")]
    SendFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_failures_name_the_limit_in_plain_words() {
        assert_eq!(
            AgentError::TokenBudgetExceeded {
                used: 619_018,
                budget: 600_000
            }
            .to_string(),
            "This Agent reached its token limit for this activation (600,000 tokens; 619,018 \
             needed). Raise the Agent's token budget or narrow the task."
        );
        let refused: AgentError = axocoatl_llm::ProviderError::BudgetExhausted {
            provider: "ollama".into(),
            message: "The Session budget for this Agent is used up: 1,000 of its 1,457,714 \
                      tokens remain and the next model call needs 36,864."
                .into(),
        }
        .into();
        let text = refused.to_string();
        assert!(text.starts_with("The Session budget for this Agent is used up"));
        assert!(!text.contains("LLM provider error") && !text.contains("Invalid request"));
        let other: AgentError = axocoatl_llm::ProviderError::Network("reset".into()).into();
        assert_eq!(
            other.to_string(),
            "LLM provider error: Network error: reset"
        );
        assert_eq!(
            AgentError::ToolRoundLimit {
                limit: 1_024,
                pending: "bash".into()
            }
            .to_string(),
            "This Agent reached its tool-round limit for this activation (1,024 rounds) and \
             still asked for bash; those calls did not run. Run it again to go on, or narrow \
             the task."
        );
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1,000");
        assert_eq!(group_digits(36_864), "36,864");
    }
}
