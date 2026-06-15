use std::collections::HashMap;

use axocoatl_llm::ToolDefinition;
use rmcp::service::RunningService;
use rmcp::RoleClient;

use crate::error::McpError;

/// Transport types for connecting to MCP servers.
#[derive(Debug, Clone)]
pub enum McpTransportType {
    /// Local process via stdin/stdout (rmcp feature: transport-child-process).
    /// `env` is layered onto the child process's environment — most stdio
    /// servers take their API key / token this way (e.g. `BRAVE_API_KEY`,
    /// `GITHUB_PERSONAL_ACCESS_TOKEN`). Without it those servers exit before
    /// the initialize handshake.
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
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
    /// Live client per server, kept alive after discovery so an agent's tool
    /// call can be dispatched over the same connection instead of re-dialing.
    /// The `RunningService` owns the background I/O task plus a drop-guard, so
    /// removing it from this map (via `remove_server` / `reconnect_server`)
    /// closes the connection and, for stdio, lets the child process exit.
    clients: HashMap<String, RunningService<RoleClient, ()>>,
}

impl McpToolRegistry {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            tool_index: HashMap::new(),
            original_names: HashMap::new(),
            transports: HashMap::new(),
            clients: HashMap::new(),
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
            McpTransportType::Stdio { command, args, env } => {
                use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
                use rmcp::ServiceExt;
                use tokio::process::Command;

                let args = args.clone();
                let env = env.clone();
                let client = ()
                    .serve(
                        TokioChildProcess::new(Command::new(command).configure(|cmd| {
                            cmd.args(&args);
                            cmd.envs(&env);
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
                self.transports.insert(name.clone(), cached_transport);

                // Keep the connection alive: discovery and execution share one
                // client, so an agent's tool call dispatches over this same
                // handshake (see `call_tool`). The connection is closed when the
                // server is removed or reconnected.
                self.clients.insert(name, client);
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
                self.transports.insert(name.clone(), cached_transport);

                // Keep the connection alive (same rationale as the stdio arm).
                self.clients.insert(name, client);
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
        // Dropping the client closes the connection via its drop-guard (the
        // background task is cancelled and, for stdio, the child exits).
        self.clients.remove(name);
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
        // Close the existing connection cleanly before re-dialing so we don't
        // leak the old background task / child process.
        if let Some(old) = self.clients.remove(name) {
            let _ = old.cancel().await;
        }
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

    /// Call a tool on a connected server over its persistent client and return
    /// the result as JSON (`{"text": ...}` from the joined text content blocks).
    ///
    /// `tool` is the BARE name the server registered — map a qualified
    /// `mcp__server__tool` key back with [`original_name`](Self::original_name)
    /// first. A server-reported tool error (`isError`) is surfaced as
    /// [`McpError::CallFailed`]; an unknown server as [`McpError::ServerNotFound`].
    pub async fn call_tool(
        &self,
        server: &str,
        tool: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let tool = tool.into();
        // Clone the cheap channel-handle peer rather than borrowing the map
        // across the round trip; the `RunningService` stays parked in `clients`.
        let peer = self
            .clients
            .get(server)
            .map(|svc| svc.peer().clone())
            .ok_or_else(|| McpError::ServerNotFound(server.to_string()))?;

        let params = rmcp::model::CallToolRequestParams::new(tool.clone())
            .with_arguments(arguments.as_object().cloned().unwrap_or_default());
        let result = peer
            .call_tool(params)
            .await
            .map_err(|e| McpError::CallFailed(e.to_string()))?;

        // MCP tool results are a list of content blocks; join the text ones.
        let text = result
            .content
            .iter()
            .filter_map(|c| c.raw.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");

        if result.is_error.unwrap_or(false) {
            return Err(McpError::CallFailed(format!(
                "tool '{tool}' on server '{server}' reported an error: {text}"
            )));
        }

        Ok(serde_json::json!({ "text": text }))
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

    // ── Persistent-connection call path ────────────────────────────────────
    // A trivial in-process MCP server over an in-memory duplex stream stands in
    // for a real stdio child: fully hermetic (no process, no npx, no network)
    // while exercising the exact `().serve(...)` client + `call_tool` round trip
    // the registry now keeps alive after discovery.

    use rmcp::handler::server::router::tool::ToolRouter;
    use rmcp::handler::server::wrapper::Parameters;
    use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    struct EchoArgs {
        msg: String,
    }

    #[derive(Clone)]
    struct EchoServer {
        tool_router: ToolRouter<Self>,
    }

    impl EchoServer {
        fn new() -> Self {
            Self {
                tool_router: Self::tool_router(),
            }
        }
    }

    #[tool_router(router = tool_router)]
    impl EchoServer {
        #[tool(description = "Echo the message back, prefixed.")]
        async fn echo(&self, args: Parameters<EchoArgs>) -> String {
            let Parameters(EchoArgs { msg }) = args;
            format!("echo: {msg}")
        }
    }

    #[tool_handler(router = self.tool_router)]
    impl ServerHandler for EchoServer {}

    #[tokio::test]
    async fn persistent_client_call_tool_returns_real_result() {
        let (server_io, client_io) = tokio::io::duplex(8192);
        let (sr, sw) = tokio::io::split(server_io);
        let (cr, cw) = tokio::io::split(client_io);

        let server = tokio::spawn(async move {
            if let Ok(svc) = EchoServer::new().serve((sr, sw)).await {
                let _ = svc.waiting().await;
            }
        });

        // The same client the registry holds, kept alive past discovery.
        let client = ().serve((cr, cw)).await.expect("client connects");

        // Hand-assemble a registry holding the live client + one indexed tool,
        // mirroring what `connect_server` now does internally.
        let mut reg = McpToolRegistry::new();
        let qualified = qualified_tool_name("mem", "echo");
        reg.tool_index.insert(
            qualified.clone(),
            (
                "mem".to_string(),
                ToolDefinition {
                    name: qualified.clone(),
                    description: "Echo the message back, prefixed.".to_string(),
                    parameters: serde_json::json!({}),
                    concurrency: axocoatl_llm::ConcurrencyPolicy::Safe,
                },
            ),
        );
        reg.original_names
            .insert(qualified.clone(), "echo".to_string());
        reg.servers.insert(
            "mem".to_string(),
            McpServerInfo {
                name: "mem".to_string(),
                transport_type: "memory".to_string(),
                tool_count: 1,
            },
        );
        reg.clients.insert("mem".to_string(), client);

        // The real call, dispatched over the live connection.
        let bare = reg.original_name(&qualified).unwrap().to_string();
        let out = reg
            .call_tool("mem", bare, serde_json::json!({ "msg": "hi" }))
            .await
            .expect("call_tool succeeds");
        assert_eq!(out["text"], "echo: hi");

        // An unknown server is a clear, typed error — not a panic.
        let err = reg.call_tool("ghost", "echo", serde_json::json!({})).await;
        assert!(matches!(err, Err(McpError::ServerNotFound(_))));

        // Removing the server tears the connection down.
        assert!(reg.remove_server("mem"));
        server.abort();
    }
}
