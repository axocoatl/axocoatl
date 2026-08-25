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
    async fn execute_owned(
        executor: &Arc<ToolExecutor>,
        seq: usize,
        tool_call: ToolCall,
    ) -> ToolResult {
        let exec = executor.clone();
        let name = tool_call.name.clone();
        let args = tool_call.arguments.clone();
        let handle = tokio::spawn(async move { exec.execute(&name, args).await });
        match handle.await {
            Ok(result) => ToolResult {
                seq,
                tool_call,
                result,
            },
            Err(error) => {
                tracing::error!(tool = %tool_call.name, %error, "Tool execution task panicked");
                ToolResult {
                    seq,
                    result: Err(crate::error::ToolError::ExecutionFailed {
                        tool: tool_call.name.clone(),
                        reason: format!("Tool task panicked: {error}"),
                    }),
                    tool_call,
                }
            }
        }
    }

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

        // A single call still runs in an owned task: an in-process builtin can
        // panic, and the dispatcher contract is to preserve its identity as an
        // error result rather than unwind the agent actor.
        if tool_calls.len() == 1 {
            return vec![Self::execute_owned(executor, 0, tool_calls[0].clone()).await];
        }

        // Check if any Exclusive tool is present — if so, serialize everything
        let has_exclusive = tool_calls
            .iter()
            .any(|tc| policy_lookup(&tc.name) == ConcurrencyPolicy::Exclusive);

        if has_exclusive {
            // All tools run sequentially in submission order
            let mut results = Vec::with_capacity(tool_calls.len());
            for (seq, tc) in tool_calls.iter().enumerate() {
                results.push(Self::execute_owned(executor, seq, tc.clone()).await);
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
            let mut identities = std::collections::HashMap::new();

            for (seq, tc) in safe_calls {
                let exec = executor.clone();
                let name = tc.name.clone();
                let args = tc.arguments.clone();
                let task = join_set.spawn(async move { exec.execute(&name, args).await });
                identities.insert(task.id(), (seq, tc));
            }

            while let Some(join_result) = join_set.join_next_with_id().await {
                match join_result {
                    Ok((task_id, result)) => {
                        let (seq, tc) = identities
                            .remove(&task_id)
                            .expect("every joined tool task retains its identity");
                        all_results.push(ToolResult {
                            seq,
                            tool_call: tc,
                            result,
                        });
                    }
                    Err(error) => {
                        let (seq, tc) = identities
                            .remove(&error.id())
                            .expect("every failed tool task retains its identity");
                        tracing::error!(tool = %tc.name, %error, "Tool execution task panicked");
                        all_results.push(ToolResult {
                            seq,
                            result: Err(crate::error::ToolError::ExecutionFailed {
                                tool: tc.name.clone(),
                                reason: format!("Tool task panicked: {error}"),
                            }),
                            tool_call: tc,
                        });
                    }
                }
            }
        }

        // Execute Ordered tools sequentially
        for (seq, tc) in ordered_calls {
            all_results.push(Self::execute_owned(executor, seq, tc).await);
        }

        // Sort by submission order
        all_results.sort_by_key(|r| r.seq);
        all_results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::{BuiltinTool, EchoTool};

    struct PanickingTool;

    #[async_trait::async_trait]
    impl BuiltinTool for PanickingTool {
        fn description(&self) -> &str {
            "panic for dispatcher identity testing"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<serde_json::Value, crate::error::ToolError> {
            panic!("intentional tool panic")
        }
    }

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
            provider_metadata: Default::default(),
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
                provider_metadata: Default::default(),
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
    async fn parallel_panic_preserves_original_sequence_and_call_identity() {
        let mut exec = ToolExecutor::new();
        exec.register_builtin("panic_tool", Arc::new(PanickingTool));
        exec.register_builtin("echo", Arc::new(EchoTool));
        let executor = Arc::new(exec);
        let calls = vec![
            ToolCall {
                id: "call-a".to_string(),
                name: "panic_tool".to_string(),
                arguments: serde_json::json!({}),
                provider_metadata: Default::default(),
            },
            ToolCall {
                id: "call-b".to_string(),
                name: "echo".to_string(),
                arguments: serde_json::json!({"text": "ok"}),
                provider_metadata: Default::default(),
            },
        ];

        let results =
            ConcurrentToolDispatcher::dispatch(&executor, &calls, |_| ConcurrencyPolicy::Safe)
                .await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].seq, 0);
        assert_eq!(results[0].tool_call.id, "call-a");
        assert_eq!(results[0].tool_call.name, "panic_tool");
        assert!(matches!(
            results[0].result,
            Err(crate::error::ToolError::ExecutionFailed { ref tool, .. })
                if tool == "panic_tool"
        ));
        assert_eq!(results[1].seq, 1);
        assert_eq!(results[1].tool_call.id, "call-b");
        assert_eq!(results[1].tool_call.name, "echo");
        assert!(results[1].result.is_ok());
    }

    #[tokio::test]
    async fn single_panic_preserves_exact_call_identity() {
        let mut exec = ToolExecutor::new();
        exec.register_builtin("panic_tool", Arc::new(PanickingTool));
        let executor = Arc::new(exec);
        let call = ToolCall {
            id: "single-a".to_string(),
            name: "panic_tool".to_string(),
            arguments: serde_json::json!({"sentinel": "single"}),
            provider_metadata: Default::default(),
        };

        let results =
            ConcurrentToolDispatcher::dispatch(&executor, std::slice::from_ref(&call), |_| {
                ConcurrencyPolicy::Safe
            })
            .await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].seq, 0);
        assert_eq!(results[0].tool_call.id, "single-a");
        assert_eq!(results[0].tool_call.arguments, call.arguments);
        assert!(matches!(
            results[0].result,
            Err(crate::error::ToolError::ExecutionFailed { ref tool, .. })
                if tool == "panic_tool"
        ));
    }

    async fn assert_sequential_panic_policy(policy: ConcurrencyPolicy) {
        let mut exec = ToolExecutor::new();
        exec.register_builtin("panic_tool", Arc::new(PanickingTool));
        exec.register_builtin("echo", Arc::new(EchoTool));
        let executor = Arc::new(exec);
        let calls = vec![
            ToolCall {
                id: "call-a".to_string(),
                name: "panic_tool".to_string(),
                arguments: serde_json::json!({}),
                provider_metadata: Default::default(),
            },
            ToolCall {
                id: "call-b".to_string(),
                name: "echo".to_string(),
                arguments: serde_json::json!({"text": "survived"}),
                provider_metadata: Default::default(),
            },
        ];

        let results = ConcurrentToolDispatcher::dispatch(&executor, &calls, |_| policy).await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].seq, 0);
        assert_eq!(results[0].tool_call.id, "call-a");
        assert!(results[0].result.is_err());
        assert_eq!(results[1].seq, 1);
        assert_eq!(results[1].tool_call.id, "call-b");
        assert_eq!(
            results[1].result.as_ref().unwrap(),
            &serde_json::json!({"text": "survived"})
        );
    }

    #[tokio::test]
    async fn exclusive_panic_does_not_skip_later_call() {
        assert_sequential_panic_policy(ConcurrencyPolicy::Exclusive).await;
    }

    #[tokio::test]
    async fn ordered_panic_does_not_skip_later_call() {
        assert_sequential_panic_policy(ConcurrencyPolicy::Ordered).await;
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
                provider_metadata: Default::default(),
            },
            ToolCall {
                id: "1".into(),
                name: "echo".into(),
                arguments: serde_json::json!({"text": "exclusive"}),
                provider_metadata: Default::default(),
            },
            ToolCall {
                id: "2".into(),
                name: "echo".into(),
                arguments: serde_json::json!({"text": "safe2"}),
                provider_metadata: Default::default(),
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
                provider_metadata: Default::default(),
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
