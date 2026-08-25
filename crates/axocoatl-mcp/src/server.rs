//! MCP server — exposes Axocoatl agents as callable MCP tools.
//! Any MCP-compatible client can discover and invoke agents.

use std::sync::Arc;

use rmcp::model::{ErrorData as McpError, *};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler};

const MAX_AGENT_TOOLS: usize = 1024;
const MAX_AGENT_ID_BYTES: usize = 256;
const MAX_AGENT_INPUT_BYTES: usize = 1024 * 1024;
const MAX_AGENT_RESULT_BYTES: usize = 1024 * 1024;
const MAX_AGENT_ERROR_BYTES: usize = 16 * 1024;

fn truncate_owned_utf8(mut value: String, limit: usize, label: &str) -> String {
    if value.len() <= limit {
        return value;
    }

    let original_bytes = value.len();
    let marker =
        format!("\n[truncated: {label} was {original_bytes} bytes; limit is {limit} bytes]");
    let mut end = limit.saturating_sub(marker.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    let remaining = limit.saturating_sub(value.len());
    let mut marker_end = remaining.min(marker.len());
    while marker_end > 0 && !marker.is_char_boundary(marker_end) {
        marker_end -= 1;
    }
    value.push_str(&marker[..marker_end]);
    value
}

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
        if agent_ids.len() > MAX_AGENT_TOOLS {
            return Err(McpError::internal_error(
                format!("agent tool discovery exceeds the {MAX_AGENT_TOOLS}-tool safety limit"),
                None,
            ));
        }
        if agent_ids
            .iter()
            .any(|agent_id| agent_id.is_empty() || agent_id.len() > MAX_AGENT_ID_BYTES)
        {
            return Err(McpError::internal_error(
                "agent tool discovery contains an empty or oversized agent ID",
                None,
            ));
        }

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
        if input.len() > MAX_AGENT_INPUT_BYTES {
            return Err(McpError::invalid_params(
                format!("agent input exceeds the {MAX_AGENT_INPUT_BYTES}-byte safety limit"),
                None,
            ));
        }

        match self.executor.execute_agent(agent_id, input).await {
            Ok(output) => Ok(CallToolResult::success(vec![Content::text(
                truncate_owned_utf8(output, MAX_AGENT_RESULT_BYTES, "agent output"),
            )])),
            Err(error) => Ok(CallToolResult::error(vec![Content::text(
                truncate_owned_utf8(error, MAX_AGENT_ERROR_BYTES, "agent error"),
            )])),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_agent_text_is_utf8_safe_bounded_and_marked() {
        let output = truncate_owned_utf8(
            "🦎".repeat(MAX_AGENT_RESULT_BYTES),
            MAX_AGENT_RESULT_BYTES,
            "agent output",
        );
        assert!(output.len() <= MAX_AGENT_RESULT_BYTES);
        assert!(output.is_char_boundary(output.len()));
        assert!(output.contains("[truncated: agent output was"));
    }

    #[test]
    fn short_outbound_text_is_unchanged() {
        assert_eq!(
            truncate_owned_utf8("hello".to_string(), MAX_AGENT_RESULT_BYTES, "agent output"),
            "hello"
        );
    }
}
