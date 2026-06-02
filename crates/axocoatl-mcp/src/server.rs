//! MCP server — exposes Axocoatl agents as callable MCP tools.
//! Any MCP-compatible client can discover and invoke agents.

use std::sync::Arc;

use rmcp::model::{ErrorData as McpError, *};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler};

/// Callback for routing MCP tool calls to agents.
#[async_trait::async_trait]
pub trait AgentExecutor: Send + Sync + 'static {
    /// List available agent IDs.
    async fn list_agent_ids(&self) -> Vec<String>;

    /// Execute an agent by ID with the given input text.
    async fn execute_agent(&self, agent_id: &str, input: &str) -> Result<String, String>;
}

/// Axocoatl MCP server — exposes registered agents as tools.
pub struct AxocoatlMcpServer {
    executor: Arc<dyn AgentExecutor>,
}

impl AxocoatlMcpServer {
    pub fn new(executor: Arc<dyn AgentExecutor>) -> Self {
        Self { executor }
    }
}

impl ServerHandler for AxocoatlMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let agent_ids = self.executor.list_agent_ids().await;

        let tools: Vec<Tool> = agent_ids
            .iter()
            .map(|id| {
                // Build input schema as a JsonObject (Arc<Map<String, Value>>)
                let schema: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "input": {
                                "type": "string",
                                "description": "Input text for the agent"
                            }
                        },
                        "required": ["input"]
                    }))
                    .unwrap_or_default();

                Tool::new(
                    format!("agent_{id}"),
                    format!("Execute agent {id}"),
                    Arc::new(schema),
                )
            })
            .collect();

        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let tool_name = request.name.to_string();
        let agent_id = tool_name
            .strip_prefix("agent_")
            .ok_or_else(|| McpError::invalid_request("Unknown tool", None))?;

        let input = request
            .arguments
            .as_ref()
            .and_then(|args| args.get("input"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match self.executor.execute_agent(agent_id, input).await {
            Ok(output) => Ok(CallToolResult::success(vec![Content::text(output)])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }
}
