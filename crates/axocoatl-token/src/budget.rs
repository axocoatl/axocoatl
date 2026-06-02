use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axocoatl_core::{TokenBudget, TokenUsageStats};

use crate::counter::TokenCounter;
use crate::error::BudgetError;

/// Thread-safe token budget tracker for a single agent execution.
#[derive(Clone)]
pub struct TokenTracker {
    used_input: Arc<AtomicUsize>,
    used_output: Arc<AtomicUsize>,
    budget: TokenBudget,
    counter: Arc<dyn TokenCounter>,
}

impl TokenTracker {
    pub fn new(budget: TokenBudget, counter: Arc<dyn TokenCounter>) -> Self {
        Self {
            used_input: Arc::new(AtomicUsize::new(0)),
            used_output: Arc::new(AtomicUsize::new(0)),
            budget,
            counter,
        }
    }

    /// Record tokens used in a call. Returns Err if budget exceeded.
    pub fn record_usage(
        &self,
        input_tokens: usize,
        output_tokens: usize,
    ) -> Result<(), BudgetError> {
        let new_input = self.used_input.fetch_add(input_tokens, Ordering::Relaxed) + input_tokens;
        let new_output =
            self.used_output.fetch_add(output_tokens, Ordering::Relaxed) + output_tokens;
        let total = new_input + new_output;

        if total > self.budget.per_execution {
            return Err(BudgetError::ExecutionBudgetExceeded {
                used: total,
                budget: self.budget.per_execution,
            });
        }
        Ok(())
    }

    /// Check if a proposed call would exceed budget BEFORE making it.
    pub fn check_headroom(&self, estimated_input: usize) -> Result<(), BudgetError> {
        let current =
            self.used_input.load(Ordering::Relaxed) + self.used_output.load(Ordering::Relaxed);
        if current + estimated_input > self.budget.per_execution {
            return Err(BudgetError::WouldExceedBudget {
                current,
                requested: estimated_input,
                budget: self.budget.per_execution,
            });
        }
        Ok(())
    }

    /// Total tokens used so far (input + output).
    pub fn total_used(&self) -> usize {
        self.used_input.load(Ordering::Relaxed) + self.used_output.load(Ordering::Relaxed)
    }

    /// Get input tokens used.
    pub fn input_used(&self) -> usize {
        self.used_input.load(Ordering::Relaxed)
    }

    /// Get output tokens used.
    pub fn output_used(&self) -> usize {
        self.used_output.load(Ordering::Relaxed)
    }

    /// Get a reference to the underlying counter.
    pub fn counter(&self) -> &dyn TokenCounter {
        self.counter.as_ref()
    }

    /// Get the budget configuration.
    pub fn budget(&self) -> &TokenBudget {
        &self.budget
    }

    /// Consume tracker and return final usage stats.
    pub fn finalize(self) -> TokenUsageStats {
        TokenUsageStats::new(
            self.used_input.load(Ordering::Relaxed),
            self.used_output.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_core::OverflowPolicy;

    fn test_budget(per_execution: usize) -> TokenBudget {
        TokenBudget {
            per_call: per_execution,
            per_execution,
            overflow_policy: OverflowPolicy::Abort,
        }
    }

    /// Simple counter that returns text length / 4 (rough approximation).
    struct SimpleCounter;
    impl TokenCounter for SimpleCounter {
        fn count_text(&self, text: &str) -> usize {
            text.len() / 4 + 1
        }
        fn count_messages(&self, messages: &[axocoatl_core::ChatMessage]) -> usize {
            messages
                .iter()
                .map(|m| m.text_content().map_or(1, |t| self.count_text(t)))
                .sum()
        }
        fn count_tool_definition(&self, tool_json: &serde_json::Value) -> usize {
            self.count_text(&tool_json.to_string())
        }
    }

    #[test]
    fn record_under_budget_succeeds() {
        let tracker = TokenTracker::new(test_budget(1000), Arc::new(SimpleCounter));
        assert!(tracker.record_usage(100, 50).is_ok());
        assert_eq!(tracker.total_used(), 150);
    }

    #[test]
    fn record_over_budget_fails() {
        let tracker = TokenTracker::new(test_budget(100), Arc::new(SimpleCounter));
        assert!(tracker.record_usage(60, 50).is_err());
    }

    #[test]
    fn check_headroom_predicts_overflow() {
        let tracker = TokenTracker::new(test_budget(100), Arc::new(SimpleCounter));
        tracker.record_usage(80, 0).unwrap();
        assert!(tracker.check_headroom(30).is_err());
        assert!(tracker.check_headroom(10).is_ok());
    }

    #[test]
    fn finalize_returns_stats() {
        let tracker = TokenTracker::new(test_budget(1000), Arc::new(SimpleCounter));
        tracker.record_usage(100, 50).unwrap();
        tracker.record_usage(200, 75).unwrap();
        let stats = tracker.finalize();
        assert_eq!(stats.input_tokens, 300);
        assert_eq!(stats.output_tokens, 125);
        assert_eq!(stats.total(), 425);
    }

    #[test]
    fn multiple_records_accumulate() {
        let tracker = TokenTracker::new(test_budget(1000), Arc::new(SimpleCounter));
        for _ in 0..10 {
            tracker.record_usage(10, 5).unwrap();
        }
        assert_eq!(tracker.total_used(), 150);
    }

    #[test]
    fn clone_shares_state() {
        let tracker = TokenTracker::new(test_budget(1000), Arc::new(SimpleCounter));
        let clone = tracker.clone();
        tracker.record_usage(100, 0).unwrap();
        assert_eq!(clone.total_used(), 100);
    }
}
