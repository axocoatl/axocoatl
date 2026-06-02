//! Unified tool executor — routes calls to built-in tools, MCP servers, or WASM sandboxes.

use std::collections::HashMap;
use std::sync::Arc;

use crate::builtin::BuiltinTool;
use crate::error::ToolError;

/// A registered tool with its execution backend.
#[derive(Clone)]
pub enum ToolBackend {
    /// Built-in Rust tool (runs in-process).
    Builtin(Arc<dyn BuiltinTool>),
    /// MCP tool on a named server.
    Mcp { server_name: String },
    /// WASM tool in sandbox.
    Wasm { module_name: String },
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

    /// Register an MCP tool (from a connected server).
    pub fn register_mcp(&mut self, name: impl Into<String>, server_name: impl Into<String>) {
        self.tools.insert(
            name.into(),
            ToolBackend::Mcp {
                server_name: server_name.into(),
            },
        );
    }

    /// Register a WASM tool.
    pub fn register_wasm(&mut self, name: impl Into<String>, module_name: impl Into<String>) {
        self.tools.insert(
            name.into(),
            ToolBackend::Wasm {
                module_name: module_name.into(),
            },
        );
    }

    /// Execute a tool by name.
    pub async fn execute(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let backend = self
            .tools
            .get(tool_name)
            .ok_or_else(|| ToolError::NotFound(tool_name.to_string()))?;

        match backend {
            ToolBackend::Builtin(tool) => tool.execute(arguments).await,
            ToolBackend::Mcp { server_name } => {
                // MCP tool execution requires a persistent connection (not yet implemented).
                // The registry currently disconnects after discovery.
                // For now, return a descriptive error.
                Err(ToolError::ExecutionFailed {
                    tool: tool_name.to_string(),
                    reason: format!(
                        "MCP tool '{}' on server '{}': persistent connections not yet implemented. \
                         Tools are discovered but execution requires keeping the MCP client alive.",
                        tool_name, server_name
                    ),
                })
            }
            ToolBackend::Wasm { module_name } => {
                // TODO: Route to WasmtimeSandbox for execution
                Err(ToolError::ExecutionFailed {
                    tool: tool_name.to_string(),
                    reason: format!("WASM execution of '{module_name}' not yet wired"),
                })
            }
        }
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
            Some(ToolBackend::Builtin(_)) => Some(axocoatl_llm::ConcurrencyPolicy::Safe),
            Some(ToolBackend::Mcp { .. }) => Some(axocoatl_llm::ConcurrencyPolicy::Safe),
            Some(ToolBackend::Wasm { .. }) => Some(axocoatl_llm::ConcurrencyPolicy::Safe),
            None => None,
        }
    }

    /// Convert registered tools to LLM-compatible tool definitions.
    pub fn as_llm_tools(&self) -> Vec<axocoatl_llm::ToolDefinition> {
        self.tools
            .iter()
            .filter_map(|(name, backend)| match backend {
                ToolBackend::Builtin(tool) => Some(axocoatl_llm::ToolDefinition {
                    name: name.clone(),
                    description: tool.description().to_string(),
                    parameters: tool.parameters_schema(),
                    concurrency: axocoatl_llm::ConcurrencyPolicy::Safe,
                }),
                _ => None, // MCP/WASM tools get their schemas from their registries
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

    #[test]
    fn as_llm_tools_includes_builtins() {
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));
        executor.register_builtin("json_keys", Arc::new(JsonKeysTool));

        let tools = executor.as_llm_tools();
        assert_eq!(tools.len(), 2);
    }
}
