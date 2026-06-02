#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("Unknown model for token counting: {0}")]
    UnknownModel(String),

    #[error("Tokenizer initialization failed: {0}")]
    InitFailed(String),
}

#[derive(Debug, thiserror::Error)]
pub enum BudgetError {
    #[error("Execution token budget exceeded: used {used}, budget {budget}")]
    ExecutionBudgetExceeded { used: usize, budget: usize },

    #[error(
        "Call would exceed budget: current {current} + requested {requested} > budget {budget}"
    )]
    WouldExceedBudget {
        current: usize,
        requested: usize,
        budget: usize,
    },
}
