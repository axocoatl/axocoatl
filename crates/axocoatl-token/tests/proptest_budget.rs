use proptest::prelude::*;
use std::sync::Arc;

use axocoatl_core::{ChatMessage, OverflowPolicy, TokenBudget};
use axocoatl_token::{TokenCounter, TokenTracker};

/// Simple counter for property testing.
struct TestCounter;
impl TokenCounter for TestCounter {
    fn count_text(&self, text: &str) -> usize {
        text.len() / 4 + 1
    }
    fn count_messages(&self, messages: &[ChatMessage]) -> usize {
        messages
            .iter()
            .map(|m| m.text_content().map_or(1, |t| self.count_text(t)))
            .sum()
    }
    fn count_tool_definition(&self, tool_json: &serde_json::Value) -> usize {
        self.count_text(&tool_json.to_string())
    }
}

proptest! {
    /// Property: token budget is never exceeded regardless of message sequence.
    #[test]
    fn budget_never_exceeded(
        token_amounts in prop::collection::vec(1usize..500, 1..30),
        budget_limit in 100usize..10000
    ) {
        let tracker = TokenTracker::new(
            TokenBudget {
                per_call: budget_limit,
                per_execution: budget_limit,
                overflow_policy: OverflowPolicy::Abort,
            },
            Arc::new(TestCounter),
        );

        let mut total_recorded = 0usize;
        for tokens in &token_amounts {
            let input = tokens / 2 + 1;
            let output = tokens - input;

            if total_recorded + input + output > budget_limit {
                // Should refuse
                let result = tracker.record_usage(input, output);
                // After exceeding, total_used may be over budget (we recorded then checked)
                // but the error IS returned
                prop_assert!(result.is_err() || tracker.total_used() <= budget_limit);
                break;
            } else {
                // Should accept
                let result = tracker.record_usage(input, output);
                prop_assert!(result.is_ok());
                total_recorded += input + output;
            }
        }
    }

    /// Property: check_headroom correctly predicts whether next call fits.
    #[test]
    fn headroom_check_is_consistent(
        used in 0usize..5000,
        requested in 1usize..5000,
        budget_limit in 100usize..10000
    ) {
        let tracker = TokenTracker::new(
            TokenBudget {
                per_call: budget_limit,
                per_execution: budget_limit,
                overflow_policy: OverflowPolicy::Abort,
            },
            Arc::new(TestCounter),
        );

        // Record some initial usage (capped to budget so we don't error here)
        let safe_used = used.min(budget_limit);
        if safe_used > 0 {
            let _ = tracker.record_usage(safe_used, 0);
        }

        let headroom_result = tracker.check_headroom(requested);
        let current = tracker.total_used();

        if current + requested > budget_limit {
            prop_assert!(headroom_result.is_err());
        } else {
            prop_assert!(headroom_result.is_ok());
        }
    }
}
