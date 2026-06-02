use std::collections::HashMap;

use axocoatl_llm::ToolDefinition;

use crate::error::McpError;

/// Transport types for connecting to MCP servers.
#[derive(Debug, Clone)]
pub enum McpTransportType {
    /// Local process via stdin/stdout (rmcp feature: transport-child-process).
    Stdio { command: String, args: Vec<String> },
    /// Remote server via Streamable HTTP (rmcp feature: transport-streamable-http-client-reqwest).
    /// NOTE: SSE was removed in rmcp 0.11.0.
    StreamableHttp {
        url: String,
        headers: HashMap<String, String>,
    },
}

/// Info about a connected MCP server.
#[derive(Debug, Clone)]
pub struct McpServerInfo {
    pub name: String,
    pub transport_type: String,
    pub tool_count: usize,
}

/// Construct the qualified tool name we expose to the LLM. Uses the
/// standard `mcp__{server}__{tool}` convention so collisions are
/// impossible and the routing is unambiguous when an agent calls a tool.
pub fn qualified_tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{}__{}", server, tool)
}

/// Registry of MCP tool servers and their tools.
/// Provides a unified interface for discovering and calling MCP tools.
pub struct McpToolRegistry {
    /// Connected servers: name → server info.
    servers: HashMap<String, McpServerInfo>,
    /// Tool index keyed by the QUALIFIED name (`mcp__server__tool`).
    /// Value is `(server, ToolDefinition)`. The bare tool name lives on the
    /// definition itself as `original_name` so callers (e.g. permission UI
    /// and reverse lookup) can recover it without re-parsing.
    tool_index: HashMap<String, (String, ToolDefinition)>,
    /// Original (un-qualified) tool name per qualified key. Lets us show
    /// users "filesystem_read" instead of "mcp__filesystem__filesystem_read"
    /// in the permissions UI while the LLM still sees the qualified form.
    original_names: HashMap<String, String>,
    /// Original transport per server. Cached so `reconnect_server` can
    /// re-dial without the user re-entering credentials.
    transports: HashMap<String, McpTransportType>,
}

impl McpToolRegistry {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            tool_index: HashMap::new(),
            original_names: HashMap::new(),
            transports: HashMap::new(),
        }
    }

    /// Connect to an MCP server and discover its tools.
    ///
    /// For stdio transport, this spawns a child process and performs
    /// the MCP handshake to discover available tools.
    pub async fn connect_server(
        &mut self,
        name: impl Into<String>,
        transport: McpTransportType,
    ) -> Result<(), McpError> {
        let name = name.into();
        // Cache the user-facing transport BEFORE the match's HTTP arm
        // shadows the local `transport` with rmcp's transport type.
        let cached_transport = transport.clone();

        match &transport {
            McpTransportType::Stdio { command, args } => {
                use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
                use rmcp::ServiceExt;
                use tokio::process::Command;

                let args = args.clone();
                let client = ()
                    .serve(
                        TokioChildProcess::new(Command::new(command).configure(|cmd| {
                            cmd.args(&args);
                        }))
                        .map_err(|e| McpError::ConnectionFailed(e.to_string()))?,
                    )
                    .await
                    .map_err(|e| McpError::ConnectionFailed(e.to_string()))?;

                // Discover tools from the server
                let tools = client
                    .list_all_tools()
                    .await
                    .map_err(|e| McpError::ConnectionFailed(e.to_string()))?;

                // Index all tools from this server under their qualified
                // name so two servers can both expose e.g. "read" without
                // overwriting each other.
                for tool in &tools {
                    let bare = tool.name.to_string();
                    let qualified = qualified_tool_name(&name, &bare);
                    let tool_def = ToolDefinition {
                        name: qualified.clone(),
                        description: tool
                            .description
                            .as_ref()
                            .map(|d| d.to_string())
                            .unwrap_or_default(),
                        parameters: serde_json::to_value(&tool.input_schema).unwrap_or_default(),
                        concurrency: axocoatl_llm::ConcurrencyPolicy::Safe,
                    };
                    self.tool_index
                        .insert(qualified.clone(), (name.clone(), tool_def));
                    self.original_names.insert(qualified, bare);
                }

                self.servers.insert(
                    name.clone(),
                    McpServerInfo {
                        name: name.clone(),
                        transport_type: "stdio".to_string(),
                        tool_count: tools.len(),
                    },
                );
                self.transports.insert(name, cached_transport);

                // Gracefully shut down the discovery client
                // (in production, we'd keep persistent connections)
                client
                    .cancel()
                    .await
                    .map_err(|e| McpError::ConnectionFailed(e.to_string()))?;
            }
            McpTransportType::StreamableHttp { url, headers: _ } => {
                use rmcp::ServiceExt;

                // Streamable HTTP transport via rmcp
                let transport =
                    rmcp::transport::StreamableHttpClientTransport::from_uri(url.as_str());

                let client = ()
                    .serve(transport)
                    .await
                    .map_err(|e| McpError::ConnectionFailed(format!("HTTP transport: {e}")))?;

                // Discover tools
                let tools = client
                    .list_all_tools()
                    .await
                    .map_err(|e| McpError::ConnectionFailed(e.to_string()))?;

                for tool in &tools {
                    let bare = tool.name.to_string();
                    let qualified = qualified_tool_name(&name, &bare);
                    let tool_def = ToolDefinition {
                        name: qualified.clone(),
                        description: tool
                            .description
                            .as_ref()
                            .map(|d| d.to_string())
                            .unwrap_or_default(),
                        parameters: serde_json::to_value(&tool.input_schema).unwrap_or_default(),
                        concurrency: axocoatl_llm::ConcurrencyPolicy::Safe,
                    };
                    self.tool_index
                        .insert(qualified.clone(), (name.clone(), tool_def));
                    self.original_names.insert(qualified, bare);
                }

                self.servers.insert(
                    name.clone(),
                    McpServerInfo {
                        name: name.clone(),
                        transport_type: "streamable_http".to_string(),
                        tool_count: tools.len(),
                    },
                );
                self.transports.insert(name, cached_transport);

                client
                    .cancel()
                    .await
                    .map_err(|e| McpError::ConnectionFailed(e.to_string()))?;
            }
        }

        Ok(())
    }

    /// Remove a server and all its tools from the index. Returns true if
    /// something was removed. The cached transport is also dropped — call
    /// `connect_server` again with fresh transport details to re-install.
    pub fn remove_server(&mut self, name: &str) -> bool {
        let had = self.servers.remove(name).is_some();
        // Drop every tool whose owning server is `name`.
        let drop: Vec<String> = self
            .tool_index
            .iter()
            .filter(|(_, (srv, _))| srv == name)
            .map(|(k, _)| k.clone())
            .collect();
        for k in drop {
            self.tool_index.remove(&k);
            self.original_names.remove(&k);
        }
        self.transports.remove(name);
        had
    }

    /// Drop the live tools for a server, then re-dial using its cached
    /// transport (which `connect_server` stashed at first install). Lets
    /// the user fix transient failures or pick up a server that was just
    /// updated, without losing the credentials they entered.
    pub async fn reconnect_server(&mut self, name: &str) -> Result<(), McpError> {
        let transport =
            self.transports.get(name).cloned().ok_or_else(|| {
                McpError::ConnectionFailed(format!("no cached transport for {name}"))
            })?;
        // Drop current tools first so the connect_server below builds the
        // index from a clean state.
        let drop: Vec<String> = self
            .tool_index
            .iter()
            .filter(|(_, (srv, _))| srv == name)
            .map(|(k, _)| k.clone())
            .collect();
        for k in drop {
            self.tool_index.remove(&k);
            self.original_names.remove(&k);
        }
        self.servers.remove(name);
        // Reconnect — connect_server takes a name + transport.
        self.connect_server(name.to_string(), transport).await
    }

    /// The unqualified tool name (e.g. `read`) for a qualified key
    /// (e.g. `mcp__filesystem__read`), if known.
    pub fn original_name(&self, qualified: &str) -> Option<&str> {
        self.original_names.get(qualified).map(|s| s.as_str())
    }

    /// Cached transport for a server (used by reconnect callers that want
    /// to surface transport details in the UI).
    pub fn transport_for(&self, name: &str) -> Option<&McpTransportType> {
        self.transports.get(name)
    }

    /// Get all available tools as axocoatl-llm ToolDefinitions (for passing to LLM).
    pub fn as_llm_tools(&self) -> Vec<ToolDefinition> {
        self.tool_index.values().map(|(_, td)| td.clone()).collect()
    }

    /// Get tool names for a specific server.
    pub fn tools_for_server(&self, server_name: &str) -> Vec<String> {
        self.tool_index
            .iter()
            .filter(|(_, (sn, _))| sn == server_name)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Get all tool names.
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_index.keys().cloned().collect()
    }

    /// Look up which server owns a tool.
    pub fn server_for_tool(&self, tool_name: &str) -> Option<&str> {
        self.tool_index
            .get(tool_name)
            .map(|(server, _)| server.as_str())
    }

    /// List connected servers.
    pub fn servers(&self) -> Vec<&McpServerInfo> {
        self.servers.values().collect()
    }

    /// All tools as (tool_name, server_name, description) tuples, for display.
    pub fn tool_entries(&self) -> Vec<(String, String, String)> {
        self.tool_index
            .iter()
            .map(|(name, (server, def))| (name.clone(), server.clone(), def.description.clone()))
            .collect()
    }

    /// Number of registered tools.
    pub fn tool_count(&self) -> usize {
        self.tool_index.len()
    }
}

impl Default for McpToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry() {
        let reg = McpToolRegistry::new();
        assert_eq!(reg.tool_count(), 0);
        assert!(reg.servers().is_empty());
        assert!(reg.tool_names().is_empty());
    }

    #[test]
    fn as_llm_tools_empty() {
        let reg = McpToolRegistry::new();
        assert!(reg.as_llm_tools().is_empty());
    }

    // Integration tests with real MCP servers are gated — they require npx or a server binary.
    // The connect_server path is tested through the vertical integration test.
}
