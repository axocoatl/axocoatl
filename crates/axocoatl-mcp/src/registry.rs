use std::collections::{HashMap, HashSet};
use std::fmt::{self, Write as _};
use std::time::Duration;

use axocoatl_llm::ToolDefinition;
use rmcp::model::{PaginatedRequestParams, Tool};
use rmcp::service::RunningService;
use rmcp::RoleClient;

use crate::error::McpError;

// MCP servers are configured integrations, but every response remains untrusted input.
// These limits bound what discovery and execution can retain or expose to a model after
// rmcp has decoded one protocol message. Transport-level allocation is discussed at the
// connection sites below because rmcp 1.8 does not expose a receive-size limit for either
// its stdio line reader or reqwest-backed Streamable HTTP response decoder.
const MAX_MCP_SERVERS: usize = 32;
const MAX_TOOLS_PER_SERVER: usize = 256;
const MAX_TOTAL_TOOLS: usize = 1024;
const MAX_DISCOVERY_PAGES: usize = 64;
const MAX_SERVER_NAME_BYTES: usize = 256;
const MAX_TOOL_NAME_BYTES: usize = 256;
const MAX_CURSOR_BYTES: usize = 4096;
const MAX_DESCRIPTION_BYTES: usize = 4096;
const MAX_INPUT_SCHEMA_BYTES: usize = 32 * 1024;
const MAX_SERVER_DEFINITION_BYTES: usize = 2 * 1024 * 1024;
const MAX_REGISTRY_DEFINITION_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESULT_TEXT_BYTES: usize = 1024 * 1024;
const MAX_ERROR_TEXT_BYTES: usize = 16 * 1024;
const MAX_HTTP_HEADERS: usize = 64;
const MAX_HTTP_HEADER_NAME_BYTES: usize = 256;
const MAX_HTTP_HEADER_VALUE_BYTES: usize = 8 * 1024;
const MAX_HTTP_HEADERS_BYTES: usize = 64 * 1024;
const MCP_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const MCP_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const MCP_CALL_TIMEOUT: Duration = Duration::from_secs(120);

const ERROR_TRUNCATION_MARKER: &str = "\n[truncated: MCP error exceeded the safety limit]";

struct BoundedFmtWriter {
    output: String,
    content_limit: usize,
    truncated: bool,
}

impl BoundedFmtWriter {
    fn new(limit: usize, marker: &str) -> Self {
        Self {
            output: String::with_capacity(limit),
            content_limit: limit.saturating_sub(marker.len()),
            truncated: false,
        }
    }
}

impl fmt::Write for BoundedFmtWriter {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let remaining = self.content_limit.saturating_sub(self.output.len());
        if value.len() <= remaining {
            self.output.push_str(value);
            return Ok(());
        }

        self.output.push_str(utf8_prefix(value, remaining));
        self.truncated = true;
        Err(fmt::Error)
    }
}

fn bounded_display(value: &impl fmt::Display, limit: usize) -> String {
    let mut writer = BoundedFmtWriter::new(limit, ERROR_TRUNCATION_MARKER);
    let _ = write!(&mut writer, "{value}");
    if writer.truncated {
        writer.output.push_str(ERROR_TRUNCATION_MARKER);
    }
    writer.output
}

fn bounded_owned_text(value: &str, limit: usize, label: &str) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }

    let marker = format!(
        "\n[truncated: {label} was {} bytes; limit is {limit} bytes]",
        value.len()
    );
    let content_limit = limit.saturating_sub(marker.len());
    let mut output = String::with_capacity(limit);
    output.push_str(utf8_prefix(value, content_limit));
    output.push_str(utf8_prefix(&marker, limit.saturating_sub(output.len())));
    output
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[derive(Default)]
struct JsonSizeWriter {
    bytes: usize,
    limit: usize,
    exceeded: bool,
}

impl JsonSizeWriter {
    fn with_limit(limit: usize) -> Self {
        Self {
            bytes: 0,
            limit,
            exceeded: false,
        }
    }
}

impl std::io::Write for JsonSizeWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(bytes.len()).min(self.limit + 1);
        self.exceeded |= self.bytes > self.limit;
        // Count without retaining a second serialized copy of the schema.
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_size_with_limit(value: &impl serde::Serialize, limit: usize) -> (usize, bool) {
    let mut writer = JsonSizeWriter::with_limit(limit);
    // Serializing an already-decoded JSON object to this infallible writer cannot fail.
    let _ = serde_json::to_writer(&mut writer, value);
    (writer.bytes, writer.exceeded)
}

fn bounded_input_schema(
    schema: &serde_json::Map<String, serde_json::Value>,
) -> (serde_json::Value, usize) {
    let (size, exceeded) = json_size_with_limit(schema, MAX_INPUT_SCHEMA_BYTES);
    if !exceeded {
        return (serde_json::Value::Object(schema.clone()), size);
    }

    let replacement = serde_json::json!({
        "type": "object",
        "description": format!(
            "[truncated: MCP input schema exceeded the {MAX_INPUT_SCHEMA_BYTES}-byte safety limit]"
        ),
        "additionalProperties": true,
        "x-axocoatl-truncated": true
    });
    let (replacement_size, _) = json_size_with_limit(&replacement, MAX_INPUT_SCHEMA_BYTES);
    (replacement, replacement_size)
}

fn bounded_joined_text(content: &[rmcp::model::Content]) -> String {
    let mut original_bytes = 0usize;
    let mut text_count = 0usize;
    for block in content {
        if let Some(text) = block.raw.as_text() {
            if text_count > 0 {
                original_bytes = original_bytes.saturating_add(1);
            }
            original_bytes = original_bytes.saturating_add(text.text.len());
            text_count += 1;
        }
    }

    let marker = (original_bytes > MAX_RESULT_TEXT_BYTES).then(|| {
        format!(
            "\n[truncated: MCP text was {original_bytes} bytes; limit is {MAX_RESULT_TEXT_BYTES} bytes]"
        )
    });
    let content_limit = marker.as_ref().map_or(MAX_RESULT_TEXT_BYTES, |marker| {
        MAX_RESULT_TEXT_BYTES.saturating_sub(marker.len())
    });
    let mut output = String::with_capacity(original_bytes.min(MAX_RESULT_TEXT_BYTES));
    let mut appended = 0usize;

    for block in content {
        let Some(text) = block.raw.as_text() else {
            continue;
        };
        if appended > 0 && output.len() < content_limit {
            output.push('\n');
        }
        let remaining = content_limit.saturating_sub(output.len());
        output.push_str(utf8_prefix(&text.text, remaining));
        appended += 1;
        if output.len() == content_limit {
            break;
        }
    }

    if let Some(marker) = marker {
        output.push_str(&marker);
    }
    output
}

fn parse_http_headers(
    headers: &HashMap<String, String>,
) -> Result<HashMap<http::HeaderName, http::HeaderValue>, McpError> {
    if headers.len() > MAX_HTTP_HEADERS {
        return Err(McpError::ConnectionFailed(format!(
            "Streamable HTTP headers exceed the {MAX_HTTP_HEADERS}-header safety limit"
        )));
    }

    let mut total_bytes = 0usize;
    let mut parsed = HashMap::with_capacity(headers.len());
    for (name, value) in headers {
        if name.len() > MAX_HTTP_HEADER_NAME_BYTES {
            return Err(McpError::ConnectionFailed(
                "Streamable HTTP header name exceeds the safety limit".to_string(),
            ));
        }
        if value.len() > MAX_HTTP_HEADER_VALUE_BYTES {
            return Err(McpError::ConnectionFailed(format!(
                "Streamable HTTP header '{name}' value exceeds the safety limit"
            )));
        }
        total_bytes = total_bytes
            .checked_add(name.len().saturating_add(value.len()))
            .ok_or_else(|| {
                McpError::ConnectionFailed(
                    "Streamable HTTP headers exceed the aggregate safety limit".to_string(),
                )
            })?;
        if total_bytes > MAX_HTTP_HEADERS_BYTES {
            return Err(McpError::ConnectionFailed(
                "Streamable HTTP headers exceed the aggregate safety limit".to_string(),
            ));
        }

        let header_name = http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            McpError::ConnectionFailed(format!("Streamable HTTP header name '{name}' is invalid"))
        })?;
        let mut header_value = http::HeaderValue::from_str(value).map_err(|_| {
            // Never echo a header value: it commonly contains credentials.
            McpError::ConnectionFailed(format!(
                "Streamable HTTP header '{name}' has an invalid value"
            ))
        })?;
        header_value.set_sensitive(true);
        parsed.insert(header_name, header_value);
    }
    Ok(parsed)
}

fn streamable_http_config(
    url: &str,
    headers: &HashMap<String, String>,
) -> Result<rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig, McpError>
{
    Ok(
        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url)
            .custom_headers(parse_http_headers(headers)?),
    )
}

struct StagedTool {
    qualified: String,
    bare: String,
    definition: ToolDefinition,
    definition_bytes: usize,
}

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
    format!("mcp__{server}__{tool}")
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
    /// Approximate serialized bytes retained in model-facing tool definitions.
    definition_bytes: usize,
}

impl McpToolRegistry {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            tool_index: HashMap::new(),
            original_names: HashMap::new(),
            transports: HashMap::new(),
            clients: HashMap::new(),
            definition_bytes: 0,
        }
    }

    async fn discover_tools_bounded(
        client: &RunningService<RoleClient, ()>,
        server_name: &str,
    ) -> Result<Vec<StagedTool>, McpError> {
        let peer = client.peer();
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        let mut staged = Vec::new();
        let mut server_definition_bytes = 0usize;

        for _ in 0..MAX_DISCOVERY_PAGES {
            let result = peer
                .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                .await
                .map_err(|error| {
                    McpError::ConnectionFailed(bounded_display(&error, MAX_ERROR_TEXT_BYTES))
                })?;

            let next_total = staged
                .len()
                .checked_add(result.tools.len())
                .ok_or_else(|| {
                    McpError::ConnectionFailed(
                        "MCP tool discovery count overflowed the safety limit".to_string(),
                    )
                })?;
            if next_total > MAX_TOOLS_PER_SERVER {
                return Err(McpError::ConnectionFailed(format!(
                    "MCP server '{server_name}' exposes more than the {MAX_TOOLS_PER_SERVER}-tool safety limit"
                )));
            }

            for tool in result.tools {
                let staged_tool = Self::stage_tool(server_name, tool)?;
                if staged
                    .iter()
                    .any(|existing: &StagedTool| existing.qualified == staged_tool.qualified)
                {
                    return Err(McpError::ConnectionFailed(format!(
                        "MCP server '{server_name}' exposes duplicate tool name '{}'",
                        staged_tool.bare
                    )));
                }
                server_definition_bytes = server_definition_bytes
                    .checked_add(staged_tool.definition_bytes)
                    .ok_or_else(|| {
                        McpError::ConnectionFailed(
                            "MCP tool definitions overflowed the safety limit".to_string(),
                        )
                    })?;
                if server_definition_bytes > MAX_SERVER_DEFINITION_BYTES {
                    return Err(McpError::ConnectionFailed(format!(
                        "MCP server '{server_name}' tool definitions exceed the {MAX_SERVER_DEFINITION_BYTES}-byte safety limit"
                    )));
                }
                staged.push(staged_tool);
            }

            let Some(next_cursor) = result.next_cursor else {
                return Ok(staged);
            };
            if next_cursor.len() > MAX_CURSOR_BYTES {
                return Err(McpError::ConnectionFailed(format!(
                    "MCP server '{server_name}' returned an oversized discovery cursor"
                )));
            }
            if !seen_cursors.insert(next_cursor.clone()) {
                return Err(McpError::ConnectionFailed(format!(
                    "MCP server '{server_name}' repeated a discovery cursor"
                )));
            }
            cursor = Some(next_cursor);
        }

        Err(McpError::ConnectionFailed(format!(
            "MCP server '{server_name}' exceeded the {MAX_DISCOVERY_PAGES}-page discovery limit"
        )))
    }

    fn stage_tool(server_name: &str, tool: Tool) -> Result<StagedTool, McpError> {
        let bare = tool.name.into_owned();
        if bare.is_empty() || bare.len() > MAX_TOOL_NAME_BYTES {
            return Err(McpError::ConnectionFailed(format!(
                "MCP server '{server_name}' returned an empty or oversized tool name"
            )));
        }
        let qualified = qualified_tool_name(server_name, &bare);
        let description = bounded_owned_text(
            tool.description.as_deref().unwrap_or_default(),
            MAX_DESCRIPTION_BYTES,
            "MCP tool description",
        );
        let (parameters, schema_bytes) = bounded_input_schema(tool.input_schema.as_ref());
        let definition_bytes = qualified
            .len()
            .saturating_add(bare.len())
            .saturating_add(description.len())
            .saturating_add(schema_bytes);
        let definition = ToolDefinition {
            name: qualified.clone(),
            description,
            parameters,
            // MCP does not expose a trustworthy read-only concurrency contract
            // in its portable tool schema. Treat discovered calls as mutating
            // by default; an explicitly constructed/stored definition may opt
            // into a narrower policy and the executor will honor it.
            concurrency: axocoatl_llm::ConcurrencyPolicy::Exclusive,
        };
        Ok(StagedTool {
            qualified,
            bare,
            definition,
            definition_bytes,
        })
    }

    async fn install_connected_client(
        &mut self,
        name: String,
        transport_type: &str,
        cached_transport: McpTransportType,
        client: RunningService<RoleClient, ()>,
    ) -> Result<(), McpError> {
        let staged = tokio::time::timeout(
            MCP_DISCOVERY_TIMEOUT,
            Self::discover_tools_bounded(&client, &name),
        )
        .await
        .map_err(|_| {
            McpError::ConnectionFailed(format!(
                "MCP server '{name}' tool discovery timed out after {} seconds",
                MCP_DISCOVERY_TIMEOUT.as_secs()
            ))
        })??;

        self.install_staged_tools(&name, transport_type, staged)?;
        self.transports.insert(name.clone(), cached_transport);
        self.clients.insert(name, client);
        Ok(())
    }

    fn install_staged_tools(
        &mut self,
        name: &str,
        transport_type: &str,
        staged: Vec<StagedTool>,
    ) -> Result<(), McpError> {
        if staged.len() > MAX_TOOLS_PER_SERVER {
            return Err(McpError::ConnectionFailed(format!(
                "MCP server '{name}' exposes more than the {MAX_TOOLS_PER_SERVER}-tool safety limit"
            )));
        }
        let next_tool_count = self
            .tool_index
            .len()
            .checked_add(staged.len())
            .ok_or_else(|| {
                McpError::ConnectionFailed(
                    "MCP registry tool count overflowed the safety limit".to_string(),
                )
            })?;
        if next_tool_count > MAX_TOTAL_TOOLS {
            return Err(McpError::ConnectionFailed(format!(
                "MCP registry would exceed the {MAX_TOTAL_TOOLS}-tool safety limit"
            )));
        }
        if let Some(collision) = staged
            .iter()
            .find(|tool| self.tool_index.contains_key(&tool.qualified))
        {
            return Err(McpError::ConnectionFailed(format!(
                "MCP tool name '{}' collides with an already-connected server",
                collision.qualified
            )));
        }
        let added_definition_bytes = staged
            .iter()
            .try_fold(0usize, |total, tool| {
                total.checked_add(tool.definition_bytes)
            })
            .ok_or_else(|| {
                McpError::ConnectionFailed(
                    "MCP registry definition bytes overflowed the safety limit".to_string(),
                )
            })?;
        let next_definition_bytes = self
            .definition_bytes
            .checked_add(added_definition_bytes)
            .ok_or_else(|| {
                McpError::ConnectionFailed(
                    "MCP registry definition bytes overflowed the safety limit".to_string(),
                )
            })?;
        if next_definition_bytes > MAX_REGISTRY_DEFINITION_BYTES {
            return Err(McpError::ConnectionFailed(format!(
                "MCP registry would exceed the {MAX_REGISTRY_DEFINITION_BYTES}-byte definition safety limit"
            )));
        }

        let tool_count = staged.len();
        for tool in staged {
            self.tool_index
                .insert(tool.qualified.clone(), (name.to_string(), tool.definition));
            self.original_names.insert(tool.qualified, tool.bare);
        }
        self.definition_bytes = next_definition_bytes;
        self.servers.insert(
            name.to_string(),
            McpServerInfo {
                name: name.to_string(),
                transport_type: transport_type.to_string(),
                tool_count,
            },
        );
        Ok(())
    }

    fn drop_tools_for_server(&mut self, name: &str) {
        let drop: Vec<String> = self
            .tool_index
            .iter()
            .filter(|(_, (server, _))| server == name)
            .map(|(qualified, _)| qualified.clone())
            .collect();
        let mut removed_bytes = 0usize;
        for qualified in drop {
            let bare = self.original_names.remove(&qualified).unwrap_or_default();
            if let Some((_, definition)) = self.tool_index.remove(&qualified) {
                let (schema_bytes, _) =
                    json_size_with_limit(&definition.parameters, MAX_INPUT_SCHEMA_BYTES);
                removed_bytes = removed_bytes.saturating_add(
                    qualified
                        .len()
                        .saturating_add(bare.len())
                        .saturating_add(definition.description.len())
                        .saturating_add(schema_bytes),
                );
            }
        }
        self.definition_bytes = self.definition_bytes.saturating_sub(removed_bytes);
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
        if name.is_empty() || name.len() > MAX_SERVER_NAME_BYTES {
            return Err(McpError::ConnectionFailed(
                "MCP server name is empty or exceeds the safety limit".to_string(),
            ));
        }
        if self.servers.contains_key(&name) {
            return Err(McpError::ConnectionFailed(format!(
                "MCP server '{name}' is already connected; reconnect it instead"
            )));
        }
        if self.servers.len() >= MAX_MCP_SERVERS {
            return Err(McpError::ConnectionFailed(format!(
                "MCP registry already contains the {MAX_MCP_SERVERS}-server safety limit"
            )));
        }
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
                let client = tokio::time::timeout(
                    MCP_HANDSHAKE_TIMEOUT,
                    ().serve(
                        TokioChildProcess::new(Command::new(command).configure(|cmd| {
                            cmd.args(&args);
                            cmd.envs(&env);
                        }))
                        .map_err(|error| {
                            McpError::ConnectionFailed(bounded_display(
                                &error,
                                MAX_ERROR_TEXT_BYTES,
                            ))
                        })?,
                    ),
                )
                .await
                .map_err(|_| {
                    McpError::ConnectionFailed(format!(
                        "MCP server '{name}' handshake timed out after {} seconds",
                        MCP_HANDSHAKE_TIMEOUT.as_secs()
                    ))
                })?
                .map_err(|error| {
                    McpError::ConnectionFailed(bounded_display(&error, MAX_ERROR_TEXT_BYTES))
                })?;

                self.install_connected_client(name, "stdio", cached_transport, client)
                    .await?;
            }
            McpTransportType::StreamableHttp { url, headers } => {
                use rmcp::ServiceExt;

                let config = streamable_http_config(url, headers)?;
                let transport = rmcp::transport::StreamableHttpClientTransport::from_config(config);

                let client = tokio::time::timeout(MCP_HANDSHAKE_TIMEOUT, ().serve(transport))
                    .await
                    .map_err(|_| {
                        McpError::ConnectionFailed(format!(
                            "MCP server '{name}' HTTP handshake timed out after {} seconds",
                            MCP_HANDSHAKE_TIMEOUT.as_secs()
                        ))
                    })?
                    .map_err(|error| {
                        McpError::ConnectionFailed(format!(
                            "HTTP transport: {}",
                            bounded_display(&error, MAX_ERROR_TEXT_BYTES)
                        ))
                    })?;

                self.install_connected_client(name, "streamable_http", cached_transport, client)
                    .await?;
            }
        }

        Ok(())
    }

    /// Remove a server and all its tools from the index. Returns true if
    /// something was removed. The cached transport is also dropped — call
    /// `connect_server` again with fresh transport details to re-install.
    pub fn remove_server(&mut self, name: &str) -> bool {
        let had = self.servers.remove(name).is_some();
        self.drop_tools_for_server(name);
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
        self.drop_tools_for_server(name);
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
            .ok_or_else(|| {
                McpError::ServerNotFound(bounded_owned_text(
                    server,
                    MAX_SERVER_NAME_BYTES,
                    "MCP server name",
                ))
            })?;

        let params = rmcp::model::CallToolRequestParams::new(tool.clone())
            .with_arguments(arguments.as_object().cloned().unwrap_or_default());
        let result = tokio::time::timeout(MCP_CALL_TIMEOUT, peer.call_tool(params))
            .await
            .map_err(|_| {
                McpError::CallFailed(format!(
                    "tool call timed out after {} seconds",
                    MCP_CALL_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|error| McpError::CallFailed(bounded_display(&error, MAX_ERROR_TEXT_BYTES)))?;

        // Join without cloning every text block, and never retain more than the
        // model-facing result budget. rmcp has already decoded this one message.
        let text = bounded_joined_text(&result.content);

        if result.is_error.unwrap_or(false) {
            let error = bounded_display(
                &format_args!("tool '{tool}' on server '{server}' reported an error: {text}"),
                MAX_ERROR_TEXT_BYTES,
            );
            return Err(McpError::CallFailed(error));
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

    fn test_tool(name: impl Into<String>, description: impl Into<String>) -> Tool {
        Tool::new(
            name.into(),
            description.into(),
            std::sync::Arc::new(serde_json::Map::from_iter([(
                "type".to_string(),
                serde_json::json!("object"),
            )])),
        )
    }

    #[test]
    fn tool_metadata_is_bounded_and_marked_without_splitting_utf8() {
        let description = "🦎".repeat(MAX_DESCRIPTION_BYTES);
        let mut schema = serde_json::Map::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert(
            "oversized".to_string(),
            serde_json::json!("水".repeat(MAX_INPUT_SCHEMA_BYTES)),
        );
        let tool = Tool::new("large", description, std::sync::Arc::new(schema));

        let staged = McpToolRegistry::stage_tool("bounded", tool).expect("tool is staged");
        assert!(staged.definition.description.len() <= MAX_DESCRIPTION_BYTES);
        assert!(staged
            .definition
            .description
            .is_char_boundary(staged.definition.description.len()));
        assert!(staged.definition.description.contains("[truncated:"));
        assert_eq!(staged.definition.parameters["x-axocoatl-truncated"], true);
        let (schema_bytes, exceeded) =
            json_size_with_limit(&staged.definition.parameters, MAX_INPUT_SCHEMA_BYTES);
        assert!(!exceeded);
        assert!(schema_bytes <= MAX_INPUT_SCHEMA_BYTES);
    }

    #[test]
    fn joined_tool_result_is_utf8_safe_bounded_and_marked() {
        let content = vec![
            rmcp::model::Content::text("first"),
            rmcp::model::Content::text("🦎".repeat(MAX_RESULT_TEXT_BYTES)),
        ];
        let output = bounded_joined_text(&content);
        assert!(output.len() <= MAX_RESULT_TEXT_BYTES);
        assert!(output.is_char_boundary(output.len()));
        assert!(output.contains("[truncated: MCP text was"));
    }

    #[test]
    fn streamable_http_headers_are_validated_forwarded_and_secret_safe() {
        let headers = HashMap::from([
            (
                "Authorization".to_string(),
                "Bearer secret-token".to_string(),
            ),
            ("X-Workspace".to_string(), "test".to_string()),
        ]);
        let config = streamable_http_config("https://mcp.example.test", &headers)
            .expect("valid HTTP transport config");
        let parsed = config.custom_headers;
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[&http::header::AUTHORIZATION], "Bearer secret-token");
        assert!(parsed[&http::header::AUTHORIZATION].is_sensitive());

        let secret = "must-not-appear\nin-diagnostics";
        let invalid = HashMap::from([("Authorization".to_string(), secret.to_string())]);
        let error = parse_http_headers(&invalid).expect_err("newline is invalid");
        assert!(!error.to_string().contains(secret));
    }

    #[test]
    fn qualified_name_collision_cannot_overwrite_connected_tool() {
        let mut registry = McpToolRegistry::new();
        let first =
            McpToolRegistry::stage_tool("a__b", test_tool("c", "first")).expect("first tool");
        registry
            .install_staged_tools("a__b", "memory", vec![first])
            .expect("first server installs");
        let original = registry
            .tool_index
            .get("mcp__a__b__c")
            .expect("qualified tool exists")
            .1
            .description
            .clone();

        let colliding = McpToolRegistry::stage_tool("a", test_tool("b__c", "second"))
            .expect("colliding tool stages");
        let error = registry
            .install_staged_tools("a", "memory", vec![colliding])
            .expect_err("collision is rejected");
        assert!(error.to_string().contains("collides"));
        assert_eq!(registry.tool_count(), 1);
        assert_eq!(registry.tool_index["mcp__a__b__c"].1.description, original);
    }

    #[test]
    fn reconnect_style_replacement_releases_definition_budget() {
        let mut registry = McpToolRegistry::new();

        for _ in 0..3 {
            let tool = McpToolRegistry::stage_tool("repeat", test_tool("echo", "description"))
                .expect("tool stages");
            registry
                .install_staged_tools("repeat", "memory", vec![tool])
                .expect("server installs");
            let installed_bytes = registry.definition_bytes;
            assert!(installed_bytes > 0);

            registry.drop_tools_for_server("repeat");
            registry.servers.remove("repeat");
            assert_eq!(registry.definition_bytes, 0);
            assert_eq!(registry.tool_count(), 0);
        }
    }

    #[test]
    fn per_server_tool_count_is_bounded() {
        let mut registry = McpToolRegistry::new();
        let staged = (0..=MAX_TOOLS_PER_SERVER)
            .map(|index| {
                McpToolRegistry::stage_tool(
                    "too-many",
                    test_tool(format!("tool_{index}"), "description"),
                )
                .expect("tool stages")
            })
            .collect();
        let error = registry
            .install_staged_tools("too-many", "memory", staged)
            .expect_err("oversized discovery is rejected");
        assert!(error.to_string().contains("tool safety limit"));
        assert_eq!(registry.tool_count(), 0);
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
            if msg == "large" {
                return "🦎".repeat(MAX_RESULT_TEXT_BYTES / 4 + 32);
            }
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

        let large = reg
            .call_tool("mem", "echo", serde_json::json!({ "msg": "large" }))
            .await
            .expect("large call_tool result is bounded");
        let large_text = large["text"].as_str().expect("text result");
        assert!(large_text.len() <= MAX_RESULT_TEXT_BYTES);
        assert!(large_text.contains("[truncated: MCP text was"));

        // An unknown server is a clear, typed error — not a panic.
        let err = reg.call_tool("ghost", "echo", serde_json::json!({})).await;
        assert!(matches!(err, Err(McpError::ServerNotFound(_))));

        // Removing the server tears the connection down.
        assert!(reg.remove_server("mem"));
        server.abort();
    }
}
