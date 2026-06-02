//! Unix socket IPC server for persistent daemon mode.
//!
//! The daemon stays alive as a background process. CLI clients connect via a Unix
//! domain socket, send requests (chat, list sessions, etc.), and receive responses.
//! This avoids re-bootstrapping agents on every `axocoatl chat` invocation.
//!
//! Protocol: length-prefixed JSON. Each message is a 4-byte big-endian u32 length
//! followed by that many bytes of UTF-8 JSON.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::RwLock;

use crate::bootstrap::AxocoatlDaemon;

/// Default socket path for the daemon IPC.
pub fn default_socket_path() -> PathBuf {
    let data_dir = std::env::var("AXOCOATL_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    PathBuf::from(data_dir).join("axocoatl.sock")
}

// ── IPC Message Types ────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcRequest {
    /// Execute a chat turn on an agent.
    Execute {
        agent_id: String,
        input: String,
        session_id: String,
    },
    /// List agents registered in the daemon.
    ListAgents,
    /// Ping — health check.
    Ping,
    /// Execute a multi-agent workflow.
    ExecuteWorkflow { workflow_id: String, input: String },
    /// List configured workflows.
    ListWorkflows,
    /// Per-agent token usage report (all agents if agent_id is None).
    GetTokenUsage { agent_id: Option<String> },
    /// Agent status (all agents if agent_id is None).
    GetAgentStatus { agent_id: Option<String> },
    /// Stop and re-spawn an agent.
    RestartAgent { agent_id: String },
    /// List connected MCP servers.
    ListMcpServers,
    /// List discovered MCP tools (optionally filtered by server).
    ListMcpTools { server: Option<String> },
    /// Create a directory session (single-agent mode).
    CreateSession {
        name: String,
        working_dir: String,
        agent: String,
    },
    /// List directory sessions.
    ListSessions,
    /// Execute an instruction inside a directory session.
    ExecuteSession { session_id: String, input: String },
    /// Close a directory session.
    CloseSession { session_id: String },
    /// Request graceful shutdown.
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcResponse {
    /// Chat response from an agent.
    Response {
        content: String,
        tool_calls: Vec<IpcToolCall>,
        input_tokens: usize,
        output_tokens: usize,
    },
    /// List of agent IDs.
    Agents { ids: Vec<String> },
    /// Pong — health check reply.
    Pong,
    /// Error.
    Error { message: String },
    /// Workflow execution result.
    WorkflowResponse {
        workflow_id: String,
        content: String,
        agent_outputs: Vec<IpcAgentOutput>,
        total_input_tokens: usize,
        total_output_tokens: usize,
        completed_agents: Vec<String>,
        failed_agents: Vec<(String, String)>,
    },
    /// List of workflow configs.
    Workflows { workflows: Vec<IpcWorkflowInfo> },
    /// Per-agent token usage.
    TokenUsage {
        per_agent: Vec<IpcTokenUsage>,
        total_input: usize,
        total_output: usize,
    },
    /// Per-agent status.
    AgentStatuses { statuses: Vec<IpcAgentStatus> },
    /// Agent restart acknowledged.
    RestartAck { agent_id: String },
    /// Connected MCP servers.
    McpServers { servers: Vec<IpcMcpServer> },
    /// Discovered MCP tools.
    McpTools { tools: Vec<IpcMcpTool> },
    /// A single directory session.
    Session { session: IpcSessionInfo },
    /// A list of directory sessions.
    Sessions { sessions: Vec<IpcSessionInfo> },
    /// Session execution result.
    SessionResponse {
        session_id: String,
        content: String,
        input_tokens: usize,
        output_tokens: usize,
    },
    /// Session closed.
    SessionClosed { session_id: String },
    /// Shutdown acknowledged.
    ShutdownAck,
}

/// Summary of a directory session, for IPC clients.
#[derive(Debug, Serialize, Deserialize)]
pub struct IpcSessionInfo {
    pub id: String,
    pub name: String,
    pub working_dir: String,
    pub mode: String,
    pub status: String,
}

/// Build an [`IpcSessionInfo`] from a session.
fn ipc_session_info(s: &axocoatl_session::Session) -> IpcSessionInfo {
    let mode = match &s.mode {
        axocoatl_session::SessionMode::SingleAgent { agent_id } => {
            format!("single-agent ({agent_id})")
        }
        axocoatl_session::SessionMode::Lattice { .. } => "lattice".to_string(),
        axocoatl_session::SessionMode::Custom { agents } => {
            format!("custom ({} agents)", agents.len())
        }
    };
    IpcSessionInfo {
        id: s.id.clone(),
        name: s.name.clone(),
        working_dir: s.working_dir.display().to_string(),
        mode,
        status: format!("{:?}", s.status).to_lowercase(),
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IpcTokenUsage {
    pub agent_id: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub reasoning_tokens: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IpcAgentStatus {
    pub agent_id: String,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IpcMcpServer {
    pub name: String,
    pub transport: String,
    pub tool_count: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IpcMcpTool {
    pub name: String,
    pub server: String,
    pub description: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IpcAgentOutput {
    pub agent_id: String,
    pub content: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IpcWorkflowInfo {
    pub id: String,
    pub name: String,
    pub agents: Vec<String>,
    pub entry_point: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IpcToolCall {
    pub tool_name: String,
    pub arguments: serde_json::Value,
    pub result: Option<serde_json::Value>,
}

// ── Wire Protocol ────────────────────────────────────────────────

/// Write a length-prefixed JSON message to a stream.
pub async fn write_message(stream: &mut UnixStream, msg: &impl Serialize) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(msg).map_err(|e| std::io::Error::other(e.to_string()))?;
    let len = bytes.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// Read a length-prefixed JSON message from a stream.
pub async fn read_message<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
) -> std::io::Result<T> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len > 10 * 1024 * 1024 {
        return Err(std::io::Error::other("IPC message too large (>10MB)"));
    }

    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    serde_json::from_slice(&buf).map_err(|e| std::io::Error::other(e.to_string()))
}

// ── IPC Server ───────────────────────────────────────────────────

/// Start the IPC server on a Unix domain socket.
/// Returns a JoinHandle that runs until the daemon shuts down.
pub async fn start_ipc_server(
    daemon: Arc<RwLock<AxocoatlDaemon>>,
    socket_path: &Path,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    // Remove stale socket file
    if socket_path.exists() {
        tokio::fs::remove_file(socket_path).await?;
    }

    // Ensure parent directory exists
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let listener = UnixListener::bind(socket_path)?;
    tracing::info!(path = %socket_path.display(), "IPC server listening");

    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let daemon = daemon.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_client(stream, daemon).await {
                            tracing::debug!(error = %e, "IPC client disconnected");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "IPC accept error");
                }
            }
        }
    });

    Ok(handle)
}

/// Handle a single IPC client connection.
async fn handle_client(
    mut stream: UnixStream,
    daemon: Arc<RwLock<AxocoatlDaemon>>,
) -> std::io::Result<()> {
    loop {
        let request: IpcRequest = match read_message(&mut stream).await {
            Ok(req) => req,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };

        let response = match request {
            IpcRequest::Execute {
                agent_id,
                input,
                session_id: _,
            } => {
                let daemon = daemon.read().await;
                match daemon.execute_agent(&agent_id, &input).await {
                    Ok(output) => IpcResponse::Response {
                        content: output.content,
                        tool_calls: output
                            .tool_calls
                            .into_iter()
                            .map(|tc| IpcToolCall {
                                tool_name: tc.tool_name,
                                arguments: tc.arguments,
                                result: tc.result,
                            })
                            .collect(),
                        input_tokens: output.token_usage.input_tokens,
                        output_tokens: output.token_usage.output_tokens,
                    },
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::ListAgents => {
                let daemon = daemon.read().await;
                let ids = daemon
                    .agent_registry
                    .list_ids()
                    .await
                    .into_iter()
                    .map(|id| id.to_string())
                    .collect();
                IpcResponse::Agents { ids }
            }
            IpcRequest::ExecuteWorkflow { workflow_id, input } => {
                let daemon = daemon.read().await;
                match daemon.execute_workflow(&workflow_id, &input).await {
                    Ok(output) => IpcResponse::WorkflowResponse {
                        workflow_id: output.workflow_id,
                        content: output.final_content,
                        agent_outputs: output
                            .agent_outputs
                            .into_iter()
                            .map(|(id, o)| IpcAgentOutput {
                                agent_id: id,
                                content: o.content,
                                input_tokens: o.token_usage.input_tokens,
                                output_tokens: o.token_usage.output_tokens,
                            })
                            .collect(),
                        total_input_tokens: output.total_token_usage.input_tokens,
                        total_output_tokens: output.total_token_usage.output_tokens,
                        completed_agents: output.completed_agents,
                        failed_agents: output.failed_agents,
                    },
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::ListWorkflows => {
                let daemon = daemon.read().await;
                let workflows = daemon
                    .config
                    .workflows
                    .iter()
                    .map(|w| IpcWorkflowInfo {
                        id: w.id.clone(),
                        name: w.name.clone(),
                        agents: w.agents.clone(),
                        entry_point: w.entry_point.clone(),
                    })
                    .collect();
                IpcResponse::Workflows { workflows }
            }
            IpcRequest::GetTokenUsage { agent_id } => {
                let daemon = daemon.read().await;
                let ids = match &agent_id {
                    Some(id) => vec![axocoatl_core::AgentId::new(id)],
                    None => daemon.agent_registry.list_ids().await,
                };
                let mut per_agent = Vec::new();
                let mut total_input = 0;
                let mut total_output = 0;
                for id in ids {
                    if let Some(actor) = daemon.agent_registry.get(&id).await {
                        if let Ok(usage) = axocoatl_actor::get_agent_token_usage(&actor).await {
                            total_input += usage.input_tokens;
                            total_output += usage.output_tokens;
                            per_agent.push(IpcTokenUsage {
                                agent_id: id.to_string(),
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                                reasoning_tokens: usage.reasoning_tokens,
                            });
                        }
                    }
                }
                IpcResponse::TokenUsage {
                    per_agent,
                    total_input,
                    total_output,
                }
            }
            IpcRequest::GetAgentStatus { agent_id } => {
                let daemon = daemon.read().await;
                let ids = match &agent_id {
                    Some(id) => vec![axocoatl_core::AgentId::new(id)],
                    None => daemon.agent_registry.list_ids().await,
                };
                let mut statuses = Vec::new();
                for id in ids {
                    if let Some(actor) = daemon.agent_registry.get(&id).await {
                        let status = axocoatl_actor::get_agent_status(&actor)
                            .await
                            .map(|s| format!("{s:?}"))
                            .unwrap_or_else(|e| format!("Unreachable ({e})"));
                        statuses.push(IpcAgentStatus {
                            agent_id: id.to_string(),
                            status,
                        });
                    }
                }
                IpcResponse::AgentStatuses { statuses }
            }
            IpcRequest::RestartAgent { agent_id } => {
                let daemon = daemon.read().await;
                match daemon.restart_agent(&agent_id).await {
                    Ok(()) => IpcResponse::RestartAck { agent_id },
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::ListMcpServers => {
                let daemon = daemon.read().await;
                let reg = daemon.mcp_registry.read().await;
                let servers = reg
                    .servers()
                    .into_iter()
                    .map(|s| IpcMcpServer {
                        name: s.name.clone(),
                        transport: s.transport_type.clone(),
                        tool_count: s.tool_count,
                    })
                    .collect();
                IpcResponse::McpServers { servers }
            }
            IpcRequest::ListMcpTools { server } => {
                let daemon = daemon.read().await;
                let reg = daemon.mcp_registry.read().await;
                let tools = reg
                    .tool_entries()
                    .into_iter()
                    .filter(|(_, srv, _)| server.as_ref().is_none_or(|s| s == srv))
                    .map(|(name, srv, desc)| IpcMcpTool {
                        name,
                        server: srv,
                        description: desc,
                    })
                    .collect();
                IpcResponse::McpTools { tools }
            }
            IpcRequest::CreateSession {
                name,
                working_dir,
                agent,
            } => {
                let daemon = daemon.read().await;
                match daemon
                    .create_session(
                        &name,
                        &working_dir,
                        axocoatl_session::SessionMode::SingleAgent { agent_id: agent },
                        Vec::new(),
                        Vec::new(),
                        None,
                    )
                    .await
                {
                    Ok(s) => IpcResponse::Session {
                        session: ipc_session_info(&s),
                    },
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::ListSessions => {
                let daemon = daemon.read().await;
                let sessions = daemon
                    .list_sessions()
                    .await
                    .iter()
                    .map(ipc_session_info)
                    .collect();
                IpcResponse::Sessions { sessions }
            }
            IpcRequest::ExecuteSession { session_id, input } => {
                let daemon = daemon.read().await;
                match daemon.execute_session(&session_id, &input).await {
                    Ok(output) => IpcResponse::SessionResponse {
                        session_id,
                        content: output.content,
                        input_tokens: output.token_usage.input_tokens,
                        output_tokens: output.token_usage.output_tokens,
                    },
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::CloseSession { session_id } => {
                let daemon = daemon.read().await;
                match daemon.close_session(&session_id).await {
                    Ok(()) => IpcResponse::SessionClosed { session_id },
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::Ping => IpcResponse::Pong,
            IpcRequest::Shutdown => {
                write_message(&mut stream, &IpcResponse::ShutdownAck).await?;
                // Signal shutdown will be handled by the caller
                return Ok(());
            }
        };

        write_message(&mut stream, &response).await?;
    }
}

// ── IPC Client ───────────────────────────────────────────────────

/// Connect to a running daemon via IPC.
pub struct IpcClient {
    stream: UnixStream,
}

impl IpcClient {
    /// Connect to the daemon socket.
    pub async fn connect(socket_path: &Path) -> std::io::Result<Self> {
        let stream = UnixStream::connect(socket_path).await?;
        Ok(Self { stream })
    }

    /// Send a request and receive a response.
    pub async fn request(&mut self, req: &IpcRequest) -> std::io::Result<IpcResponse> {
        write_message(&mut self.stream, req).await?;
        read_message(&mut self.stream).await
    }

    /// Check if the daemon is alive.
    pub async fn ping(&mut self) -> bool {
        matches!(self.request(&IpcRequest::Ping).await, Ok(IpcResponse::Pong))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ipc_message_round_trip() {
        // Test wire protocol with a pipe (no real daemon needed)
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let mut client = client_stream;
        let mut server = server_stream;

        // Client sends request
        let req = IpcRequest::Ping;
        write_message(&mut client, &req).await.unwrap();

        // Server reads request
        let received: IpcRequest = read_message(&mut server).await.unwrap();
        assert!(matches!(received, IpcRequest::Ping));

        // Server sends response
        write_message(&mut server, &IpcResponse::Pong)
            .await
            .unwrap();

        // Client reads response
        let resp: IpcResponse = read_message(&mut client).await.unwrap();
        assert!(matches!(resp, IpcResponse::Pong));
    }

    #[tokio::test]
    async fn ipc_execute_message_serialization() {
        let req = IpcRequest::Execute {
            agent_id: "assistant".to_string(),
            input: "hello".to_string(),
            session_id: "abc-123".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("execute"));
        assert!(json.contains("assistant"));

        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            IpcRequest::Execute {
                agent_id, input, ..
            } => {
                assert_eq!(agent_id, "assistant");
                assert_eq!(input, "hello");
            }
            _ => panic!("wrong variant"),
        }
    }
}
