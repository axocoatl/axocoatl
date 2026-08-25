use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axocoatl_core::{TokenBudget, TokenUsageStats};

use crate::counter::TokenCounter;
use crate::error::BudgetError;

fn saturating_fetch_add(value: &AtomicUsize, amount: usize) -> bool {
    let mut current = value.load(Ordering::Relaxed);
    loop {
        let (next, overflowed) = current.overflowing_add(amount);
        let next = if overflowed { usize::MAX } else { next };
        match value.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return overflowed,
            Err(actual) => current = actual,
        }
    }
}

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
        let input_overflowed = saturating_fetch_add(&self.used_input, input_tokens);
        let output_overflowed = saturating_fetch_add(&self.used_output, output_tokens);
        let used_input = self.used_input.load(Ordering::Relaxed);
        let used_output = self.used_output.load(Ordering::Relaxed);
        let combined_overflowed = used_input.checked_add(used_output).is_none();
        let total = used_input.saturating_add(used_output);

        if input_overflowed
            || output_overflowed
            || combined_overflowed
            || total > self.budget.per_execution
        {
            return Err(BudgetError::ExecutionBudgetExceeded {
                used: total,
                budget: self.budget.per_execution,
            });
        }
        Ok(())
    }

    /// Check if a proposed call would exceed budget BEFORE making it. Enforces
    /// both caps: the single-call cap (`per_call`) and the cumulative
    /// per-execution cap. The caller applies the overflow policy (abort/warn).
    pub fn check_headroom(&self, estimated_request_tokens: usize) -> Result<(), BudgetError> {
        // Per-call cap: the caller's reservation for one request must fit
        // `per_call`. Provider paths reserve estimated input plus bounded output.
        if estimated_request_tokens > self.budget.per_call {
            return Err(BudgetError::WouldExceedBudget {
                current: 0,
                requested: estimated_request_tokens,
                budget: self.budget.per_call,
            });
        }
        // Per-execution cap: cumulative usage plus this call must fit.
        let current = self.total_used();
        if current > self.budget.per_execution
            || estimated_request_tokens > self.budget.per_execution.saturating_sub(current)
        {
            return Err(BudgetError::WouldExceedBudget {
                current,
                requested: estimated_request_tokens,
                budget: self.budget.per_execution,
            });
        }
        Ok(())
    }

    /// Total tokens used so far (input + output).
    pub fn total_used(&self) -> usize {
        self.used_input
            .load(Ordering::Relaxed)
            .saturating_add(self.used_output.load(Ordering::Relaxed))
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
    fn hostile_usage_saturates_without_wrapping_budget_state() {
        let tracker = TokenTracker::new(test_budget(100), Arc::new(SimpleCounter));
        assert!(matches!(
            tracker.record_usage(usize::MAX, 1),
            Err(BudgetError::ExecutionBudgetExceeded {
                used: usize::MAX,
                budget: 100
            })
        ));
        assert_eq!(tracker.input_used(), usize::MAX);
        assert_eq!(tracker.output_used(), 1);
        assert_eq!(tracker.total_used(), usize::MAX);
        assert!(tracker.record_usage(1, usize::MAX).is_err());
        assert_eq!(tracker.input_used(), usize::MAX);
        assert_eq!(tracker.output_used(), usize::MAX);
        assert_eq!(tracker.total_used(), usize::MAX);
        assert!(tracker.check_headroom(1).is_err());

        let max_budget_tracker =
            TokenTracker::new(test_budget(usize::MAX), Arc::new(SimpleCounter));
        assert!(max_budget_tracker.record_usage(usize::MAX, 0).is_ok());
        assert!(max_budget_tracker.record_usage(0, 1).is_err());
        assert!(max_budget_tracker.check_headroom(1).is_err());
    }

    #[test]
    fn check_headroom_predicts_overflow() {
        let tracker = TokenTracker::new(test_budget(100), Arc::new(SimpleCounter));
        tracker.record_usage(80, 0).unwrap();
        assert!(tracker.check_headroom(30).is_err());
        assert!(tracker.check_headroom(10).is_ok());
    }

    #[test]
    fn check_headroom_enforces_per_call() {
        // A single call larger than per_call is refused even with ample
        // per-execution headroom.
        let budget = TokenBudget {
            per_call: 50,
            per_execution: 10_000,
            overflow_policy: OverflowPolicy::Abort,
        };
        let tracker = TokenTracker::new(budget, Arc::new(SimpleCounter));
        assert!(tracker.check_headroom(60).is_err());
        assert!(tracker.check_headroom(40).is_ok());
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
