//! Unified tool executor — routes calls to built-in tools or MCP servers.

use std::collections::HashMap;
use std::sync::Arc;

use crate::builtin::BuiltinTool;
use crate::error::ToolError;
use crate::limits::{
    bound_tool_error, bound_tool_output, ensure_json_input, TOOL_ARGUMENT_MAX_BYTES,
    TOOL_NAME_MAX_BYTES,
};

/// A registered tool with its execution backend.
#[derive(Clone)]
pub enum ToolBackend {
    /// Built-in Rust tool (runs in-process).
    Builtin(Arc<dyn BuiltinTool>),
    /// MCP tool on a named server. Carries the discovered definition so the
    /// executor can advertise it to the LLM without re-querying the registry.
    Mcp {
        server_name: String,
        definition: axocoatl_llm::ToolDefinition,
    },
}

/// Routes tool calls to the appropriate backend.
pub struct ToolExecutor {
    tools: HashMap<String, ToolBackend>,
    mcp_registry: Option<Arc<tokio::sync::RwLock<axocoatl_mcp::McpToolRegistry>>>,
}

impl ToolExecutor {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            mcp_registry: None,
        }
    }

    /// Set the MCP tool registry for routing MCP tool calls.
    pub fn with_mcp_registry(
        mut self,
        registry: Arc<tokio::sync::RwLock<axocoatl_mcp::McpToolRegistry>>,
    ) -> Self {
        self.mcp_registry = Some(registry);
        self
    }

    /// Register a built-in tool.
    pub fn register_builtin(&mut self, name: impl Into<String>, tool: Arc<dyn BuiltinTool>) {
        self.tools.insert(name.into(), ToolBackend::Builtin(tool));
    }

    /// Register an MCP tool (from a connected server). `name` is the qualified
    /// `mcp__server__tool` name the LLM sees; `definition` is what gets
    /// advertised to it.
    pub fn register_mcp(
        &mut self,
        name: impl Into<String>,
        server_name: impl Into<String>,
        definition: axocoatl_llm::ToolDefinition,
    ) {
        self.tools.insert(
            name.into(),
            ToolBackend::Mcp {
                server_name: server_name.into(),
                definition,
            },
        );
    }

    /// Execute a tool by name.
    pub async fn execute(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        if tool_name.len() > TOOL_NAME_MAX_BYTES {
            return Err(ToolError::InvalidArgs {
                tool: "tool_executor".to_string(),
                reason: format!(
                    "tool name is {} bytes; the limit is {TOOL_NAME_MAX_BYTES} bytes",
                    tool_name.len()
                ),
            });
        }
        let backend = self
            .tools
            .get(tool_name)
            .ok_or_else(|| bound_tool_error(ToolError::NotFound(tool_name.to_string())))?;

        ensure_json_input(&arguments, tool_name, TOOL_ARGUMENT_MAX_BYTES)
            .map_err(bound_tool_error)?;

        let result = match backend {
            ToolBackend::Builtin(tool) => tool.execute(arguments).await,
            ToolBackend::Mcp { server_name, .. } => {
                // Route to the live client the registry keeps alive after
                // discovery. The LLM calls the qualified `mcp__server__tool`
                // name; the server expects the bare name it registered.
                let Some(registry) = &self.mcp_registry else {
                    return Err(bound_tool_error(ToolError::ExecutionFailed {
                        tool: tool_name.to_string(),
                        reason: "MCP registry not configured on this executor".to_string(),
                    }));
                };
                let reg = registry.read().await;
                let bare = reg
                    .original_name(tool_name)
                    .unwrap_or(tool_name)
                    .to_string();
                reg.call_tool(server_name, bare, arguments)
                    .await
                    .map_err(|e| ToolError::ExecutionFailed {
                        tool: tool_name.to_string(),
                        reason: e.to_string(),
                    })
            }
        };

        let value = result.map_err(bound_tool_error)?;
        bound_tool_output(value).map_err(bound_tool_error)
    }

    /// List all registered tool names.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Get the concurrency policy for a tool by name.
    pub fn get_concurrency_policy(
        &self,
        tool_name: &str,
    ) -> Option<axocoatl_llm::ConcurrencyPolicy> {
        match self.tools.get(tool_name) {
            Some(ToolBackend::Builtin(tool)) => Some(tool.concurrency_policy()),
            Some(ToolBackend::Mcp { definition, .. }) => Some(definition.concurrency),
            None => None,
        }
    }

    /// Convert registered tools to LLM-compatible tool definitions.
    pub fn as_llm_tools(&self) -> Vec<axocoatl_llm::ToolDefinition> {
        self.tools
            .iter()
            .map(|(name, backend)| match backend {
                ToolBackend::Builtin(tool) => axocoatl_llm::ToolDefinition {
                    name: name.clone(),
                    description: tool.description().to_string(),
                    parameters: tool.parameters_schema(),
                    concurrency: tool.concurrency_policy(),
                },
                ToolBackend::Mcp { definition, .. } => definition.clone(),
            })
            .collect()
    }
}

impl Default for ToolExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience: execute a batch of tool calls concurrently.
/// This is a thin wrapper around ConcurrentToolDispatcher::dispatch.
impl ToolExecutor {
    pub async fn execute_concurrent(
        self: &Arc<Self>,
        tool_calls: &[axocoatl_llm::ToolCall],
        policy_lookup: impl Fn(&str) -> axocoatl_llm::ConcurrencyPolicy,
    ) -> Vec<crate::concurrent::ToolResult> {
        crate::concurrent::ConcurrentToolDispatcher::dispatch(self, tool_calls, policy_lookup).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct HugeResultTool;

    #[async_trait::async_trait]
    impl BuiltinTool for HugeResultTool {
        fn description(&self) -> &str {
            "test-only oversized result"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<serde_json::Value, ToolError> {
            Ok(serde_json::json!({
                "payload": "🦀".repeat(crate::limits::TOOL_OUTPUT_MAX_BYTES)
            }))
        }
    }

    struct CountingTool(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl BuiltinTool for CountingTool {
        fn description(&self) -> &str {
            "test-only counter"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<serde_json::Value, ToolError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({"ok": true}))
        }
    }

    #[tokio::test]
    async fn register_and_execute_builtin() {
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));

        let result = executor
            .execute("echo", serde_json::json!({"text": "hello"}))
            .await
            .unwrap();

        assert_eq!(result["text"], "hello");
    }

    #[tokio::test]
    async fn unknown_tool_returns_error() {
        let executor = ToolExecutor::new();
        let result = executor.execute("nonexistent", serde_json::json!({})).await;
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }

    #[tokio::test]
    async fn oversized_tool_name_is_rejected_without_reflection() {
        let executor = ToolExecutor::new();
        let result = executor
            .execute(
                &"x".repeat(crate::limits::TOOL_NAME_MAX_BYTES + 1),
                serde_json::json!({}),
            )
            .await
            .unwrap_err();
        match result {
            ToolError::InvalidArgs { tool, reason } => {
                assert_eq!(tool, "tool_executor");
                assert!(!reason.contains(&"x".repeat(128)));
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[test]
    fn as_llm_tools_includes_builtins() {
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));
        executor.register_builtin("json_keys", Arc::new(JsonKeysTool));

        let tools = executor.as_llm_tools();
        assert_eq!(tools.len(), 2);
    }

    fn mcp_def(name: &str) -> axocoatl_llm::ToolDefinition {
        axocoatl_llm::ToolDefinition {
            name: name.to_string(),
            description: "does a thing".to_string(),
            parameters: serde_json::json!({}),
            concurrency: axocoatl_llm::ConcurrencyPolicy::Safe,
        }
    }

    #[test]
    fn as_llm_tools_advertises_mcp_tools() {
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));
        executor.register_mcp("mcp__srv__do", "srv", mcp_def("mcp__srv__do"));

        let tools = executor.as_llm_tools();
        assert_eq!(tools.len(), 2);
        assert!(tools.iter().any(|t| t.name == "mcp__srv__do"));
    }

    #[tokio::test]
    async fn mcp_without_registry_reports_unconfigured() {
        // With no registry wired, the Mcp arm must surface a clear configuration
        // error — not the old "not yet implemented", and not NotFound (the tool
        // IS registered, so dispatch reaches the Mcp arm).
        let mut executor = ToolExecutor::new();
        executor.register_mcp("mcp__srv__do", "srv", mcp_def("mcp__srv__do"));

        let err = executor
            .execute("mcp__srv__do", serde_json::json!({}))
            .await
            .unwrap_err();
        match err {
            ToolError::ExecutionFailed { reason, .. } => {
                assert!(reason.contains("registry not configured"), "got: {reason}");
            }
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn executor_replaces_oversized_backend_results_with_bounded_preview() {
        let mut executor = ToolExecutor::new();
        executor.register_builtin("huge", Arc::new(HugeResultTool));
        let result = executor
            .execute("huge", serde_json::json!({}))
            .await
            .unwrap();

        assert_eq!(result["tool_result_truncated"], true);
        assert!(result["message"]
            .as_str()
            .unwrap()
            .contains("model-transport limit"));
        assert!(serde_json::to_vec(&result).unwrap().len() < crate::limits::TOOL_OUTPUT_MAX_BYTES);
    }

    #[tokio::test]
    async fn executor_rejects_oversized_arguments_before_backend_work() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut executor = ToolExecutor::new();
        executor.register_builtin("count", Arc::new(CountingTool(calls.clone())));
        let error = executor
            .execute(
                "count",
                serde_json::json!({
                    "payload": "x".repeat(crate::limits::TOOL_ARGUMENT_MAX_BYTES + 1)
                }),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, ToolError::InvalidArgs { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
