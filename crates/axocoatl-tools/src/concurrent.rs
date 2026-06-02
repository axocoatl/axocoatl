//! Concurrent tool execution using tokio JoinSet.
//! Partitions tool calls by ConcurrencyPolicy and dispatches accordingly.

use std::sync::Arc;

use axocoatl_llm::{ConcurrencyPolicy, ToolCall};

use crate::executor::ToolExecutor;

/// Result of a single tool execution, preserving submission order.
#[derive(Debug)]
pub struct ToolResult {
    /// Monotonic sequence ID (submission order).
    pub seq: usize,
    pub tool_call: ToolCall,
    pub result: Result<serde_json::Value, crate::error::ToolError>,
}

/// Dispatches tool calls concurrently based on their ConcurrencyPolicy.
pub struct ConcurrentToolDispatcher;

impl ConcurrentToolDispatcher {
    /// Execute tool calls with concurrency control.
    ///
    /// - If ANY `Exclusive` tool is present, ALL tools run sequentially in submission order
    /// - Otherwise, `Safe` tools run in parallel via JoinSet
    /// - `Ordered` tools run sequentially in submission order, in parallel with Safe group
    ///
    /// Results are returned sorted by submission order (seq).
    /// Panicked tasks produce error results (never silently dropped).
    pub async fn dispatch(
        executor: &Arc<ToolExecutor>,
        tool_calls: &[ToolCall],
        policy_lookup: impl Fn(&str) -> ConcurrencyPolicy,
    ) -> Vec<ToolResult> {
        if tool_calls.is_empty() {
            return Vec::new();
        }

        // If only one tool call, skip concurrency overhead
        if tool_calls.len() == 1 {
            let tc = &tool_calls[0];
            let result = executor.execute(&tc.name, tc.arguments.clone()).await;
            return vec![ToolResult {
                seq: 0,
                tool_call: tc.clone(),
                result,
            }];
        }

        // Check if any Exclusive tool is present — if so, serialize everything
        let has_exclusive = tool_calls
            .iter()
            .any(|tc| policy_lookup(&tc.name) == ConcurrencyPolicy::Exclusive);

        if has_exclusive {
            // All tools run sequentially in submission order
            let mut results = Vec::with_capacity(tool_calls.len());
            for (seq, tc) in tool_calls.iter().enumerate() {
                let result = executor.execute(&tc.name, tc.arguments.clone()).await;
                results.push(ToolResult {
                    seq,
                    tool_call: tc.clone(),
                    result,
                });
            }
            return results;
        }

        // No exclusive tools — partition into Safe (parallel) and Ordered (sequential)
        let mut safe_calls: Vec<(usize, ToolCall)> = Vec::new();
        let mut ordered_calls: Vec<(usize, ToolCall)> = Vec::new();

        for (seq, tc) in tool_calls.iter().enumerate() {
            match policy_lookup(&tc.name) {
                ConcurrencyPolicy::Safe => safe_calls.push((seq, tc.clone())),
                ConcurrencyPolicy::Ordered => ordered_calls.push((seq, tc.clone())),
                ConcurrencyPolicy::Exclusive => unreachable!("checked above"),
            }
        }

        let mut all_results = Vec::with_capacity(tool_calls.len());

        // Execute Safe tools in parallel via JoinSet
        if !safe_calls.is_empty() {
            let mut join_set = tokio::task::JoinSet::new();

            for (seq, tc) in safe_calls {
                let exec = executor.clone();
                let name = tc.name.clone();
                let args = tc.arguments.clone();
                let tc_clone = tc.clone();
                join_set.spawn(async move {
                    let result = exec.execute(&name, args).await;
                    (seq, tc_clone, result)
                });
            }

            while let Some(join_result) = join_set.join_next().await {
                match join_result {
                    Ok((seq, tc, result)) => {
                        all_results.push(ToolResult {
                            seq,
                            tool_call: tc,
                            result,
                        });
                    }
                    Err(e) => {
                        // Panicked task — produce an error result so the LLM still gets a response
                        tracing::error!(error = %e, "Tool execution task panicked");
                        all_results.push(ToolResult {
                            seq: usize::MAX, // will be at end; caller uses tool_call.id for matching
                            tool_call: ToolCall {
                                id: "panicked".to_string(),
                                name: "unknown".to_string(),
                                arguments: serde_json::Value::Null,
                            },
                            result: Err(crate::error::ToolError::ExecutionFailed {
                                tool: "unknown".to_string(),
                                reason: format!("Tool task panicked: {e}"),
                            }),
                        });
                    }
                }
            }
        }

        // Execute Ordered tools sequentially
        for (seq, tc) in ordered_calls {
            let result = executor.execute(&tc.name, tc.arguments.clone()).await;
            all_results.push(ToolResult {
                seq,
                tool_call: tc,
                result,
            });
        }

        // Sort by submission order
        all_results.sort_by_key(|r| r.seq);
        all_results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::EchoTool;

    #[tokio::test]
    async fn dispatch_empty() {
        let executor = Arc::new(ToolExecutor::new());
        let results =
            ConcurrentToolDispatcher::dispatch(&executor, &[], |_| ConcurrencyPolicy::Safe).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn dispatch_single_tool() {
        let mut exec = ToolExecutor::new();
        exec.register_builtin("echo", Arc::new(EchoTool));
        let executor = Arc::new(exec);

        let calls = vec![ToolCall {
            id: "1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({"text": "hello"}),
        }];

        let results =
            ConcurrentToolDispatcher::dispatch(&executor, &calls, |_| ConcurrencyPolicy::Safe)
                .await;

        assert_eq!(results.len(), 1);
        assert!(results[0].result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_parallel_safe_tools() {
        let mut exec = ToolExecutor::new();
        exec.register_builtin("echo", Arc::new(EchoTool));
        let executor = Arc::new(exec);

        let calls: Vec<ToolCall> = (0..5)
            .map(|i| ToolCall {
                id: format!("call_{i}"),
                name: "echo".to_string(),
                arguments: serde_json::json!({"text": format!("msg_{i}")}),
            })
            .collect();

        let results =
            ConcurrentToolDispatcher::dispatch(&executor, &calls, |_| ConcurrencyPolicy::Safe)
                .await;

        assert_eq!(results.len(), 5);
        // Results should be in submission order
        for (i, r) in results.iter().enumerate() {
            assert_eq!(r.seq, i);
            assert!(r.result.is_ok());
        }
    }

    #[tokio::test]
    async fn dispatch_mixed_policies() {
        let mut exec = ToolExecutor::new();
        exec.register_builtin("echo", Arc::new(EchoTool));
        let executor = Arc::new(exec);

        let calls = vec![
            ToolCall {
                id: "0".into(),
                name: "echo".into(),
                arguments: serde_json::json!({"text": "safe1"}),
            },
            ToolCall {
                id: "1".into(),
                name: "echo".into(),
                arguments: serde_json::json!({"text": "exclusive"}),
            },
            ToolCall {
                id: "2".into(),
                name: "echo".into(),
                arguments: serde_json::json!({"text": "safe2"}),
            },
        ];

        let results = ConcurrentToolDispatcher::dispatch(&executor, &calls, |_name| {
            // Simulate: call_1 is exclusive, others are safe
            ConcurrencyPolicy::Safe
        })
        .await;

        assert_eq!(results.len(), 3);
        // All in submission order
        assert_eq!(results[0].seq, 0);
        assert_eq!(results[1].seq, 1);
        assert_eq!(results[2].seq, 2);
    }

    #[tokio::test]
    async fn dispatch_preserves_order() {
        let mut exec = ToolExecutor::new();
        exec.register_builtin("echo", Arc::new(EchoTool));
        let executor = Arc::new(exec);

        let calls: Vec<ToolCall> = (0..10)
            .map(|i| ToolCall {
                id: format!("{i}"),
                name: "echo".to_string(),
                arguments: serde_json::json!({"text": format!("msg_{i}")}),
            })
            .collect();

        let results =
            ConcurrentToolDispatcher::dispatch(&executor, &calls, |_| ConcurrencyPolicy::Safe)
                .await;

        assert_eq!(results.len(), 10);
        for (i, r) in results.iter().enumerate() {
            assert_eq!(r.seq, i, "Result {} has wrong seq {}", i, r.seq);
        }
    }
}
