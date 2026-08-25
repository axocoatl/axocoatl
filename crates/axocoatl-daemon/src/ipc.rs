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
    resolve_socket_path(
        std::env::var_os("AXOCOATL_SOCKET_PATH"),
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::var_os("HOME"),
    )
}

fn resolve_socket_path(
    explicit: Option<std::ffi::OsString>,
    runtime_dir: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    if let Some(path) = explicit.filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(dir) = runtime_dir.filter(|value| !value.is_empty()) {
        return PathBuf::from(dir).join("axocoatl").join("axocoatl.sock");
    }
    if let Some(dir) = home.filter(|value| !value.is_empty()) {
        return PathBuf::from(dir)
            .join(".axocoatl")
            .join("run")
            .join("axocoatl.sock");
    }
    // Last-resort compatibility for an environment without a user home or an
    // XDG runtime directory. Normal desktop/service installs never take this
    // branch; retaining it keeps constrained containers usable.
    PathBuf::from("./data/axocoatl.sock")
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
    /// List canonical manual Automations through the workflow compatibility API.
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
        #[serde(default)]
        reasoning_tokens: usize,
        /// False means the numeric usage is only the best known subtotal.
        /// Missing older fields decode conservatively as incomplete.
        #[serde(default)]
        token_usage_known: bool,
    },
    /// List of agent IDs.
    Agents { ids: Vec<String> },
    /// Pong — health check reply.
    Pong,
    /// Error.
    Error {
        message: String,
        /// Execution errors carry the paid subtotal observed before failure.
        /// Generic and pre-dispatch errors omit all four accounting fields so
        /// older error payloads keep their original wire shape.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_tokens: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_tokens: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_tokens: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_usage_known: Option<bool>,
    },
    /// Workflow execution result.
    WorkflowResponse {
        workflow_id: String,
        content: String,
        agent_outputs: Vec<IpcAgentOutput>,
        total_input_tokens: usize,
        total_output_tokens: usize,
        #[serde(default)]
        total_reasoning_tokens: usize,
        /// False means the totals are a known subtotal. Missing older fields
        /// decode conservatively as incomplete.
        #[serde(default)]
        token_usage_known: bool,
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
        /// Reasoning tokens are billed output on providers that report them.
        /// `default` keeps a new CLI able to read a response from an older
        /// daemon during a rolling local upgrade.
        #[serde(default)]
        total_reasoning: usize,
        #[serde(default)]
        token_usage_known: bool,
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
        #[serde(default)]
        reasoning_tokens: usize,
        /// False means the numeric usage is only the best known subtotal.
        /// Missing older fields decode conservatively as incomplete.
        #[serde(default)]
        token_usage_known: bool,
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
    #[serde(default)]
    pub workspace_id: String,
    pub working_dir: String,
    pub mode: String,
    pub status: String,
    /// Durable environment readiness. Defaults empty when an older daemon or
    /// cached IPC payload predates readiness reporting.
    #[serde(default)]
    pub environment_state: String,
    /// Exact proposed/approved project setup, never an instruction to execute
    /// unless `environment_state` is Ready.
    #[serde(default)]
    pub setup_command: Option<String>,
    #[serde(default)]
    pub environment_error: Option<String>,
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
    let environment_state = match s.environment.state {
        axocoatl_session::SessionEnvironmentState::Unprepared => "unprepared",
        axocoatl_session::SessionEnvironmentState::AwaitingApproval => "awaiting_approval",
        axocoatl_session::SessionEnvironmentState::Preparing => "preparing",
        axocoatl_session::SessionEnvironmentState::Ready => "ready",
        axocoatl_session::SessionEnvironmentState::Failed => "failed",
    };
    IpcSessionInfo {
        id: s.id.clone(),
        name: s.name.clone(),
        workspace_id: s.workspace_id.clone(),
        working_dir: s.working_dir.display().to_string(),
        mode,
        status: format!("{:?}", s.status).to_lowercase(),
        environment_state: environment_state.to_string(),
        setup_command: s.environment.setup_command.clone(),
        environment_error: s.environment.error.clone(),
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IpcTokenUsage {
    pub agent_id: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub reasoning_tokens: Option<usize>,
    #[serde(default)]
    pub token_usage_known: bool,
}

impl IpcTokenUsage {
    pub fn total(&self) -> usize {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.reasoning_tokens.unwrap_or(0))
    }
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
    #[serde(default)]
    pub reasoning_tokens: usize,
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

fn error_response(message: impl ToString) -> IpcResponse {
    IpcResponse::Error {
        message: message.to_string(),
        input_tokens: None,
        output_tokens: None,
        reasoning_tokens: None,
        token_usage_known: None,
    }
}

fn measured_error_response(
    message: impl ToString,
    usage: &axocoatl_core::TokenUsageStats,
    token_usage_known: bool,
) -> IpcResponse {
    IpcResponse::Error {
        message: message.to_string(),
        input_tokens: Some(usage.input_tokens),
        output_tokens: Some(usage.output_tokens),
        reasoning_tokens: Some(usage.reasoning_tokens.unwrap_or(0)),
        token_usage_known: Some(token_usage_known),
    }
}

fn measured_daemon_failure_response(
    failure: crate::bootstrap::MeasuredDaemonFailure,
) -> IpcResponse {
    measured_error_response(
        failure.error,
        &failure.token_usage,
        failure.token_usage_known,
    )
}

fn workflow_error_response(error: crate::DaemonError) -> IpcResponse {
    match error.workflow_token_usage() {
        Some((usage, known)) => measured_error_response(error.to_string(), usage, known),
        None => measured_error_response(error, &axocoatl_core::TokenUsageStats::default(), true),
    }
}

fn workflow_response(output: crate::workflow::WorkflowOutput) -> IpcResponse {
    if let Some(error) = output.terminal_error() {
        return workflow_error_response(error);
    }

    IpcResponse::WorkflowResponse {
        workflow_id: output.workflow_id,
        content: output.final_content,
        agent_outputs: output
            .agent_outputs
            .into_iter()
            .map(|(id, output)| IpcAgentOutput {
                agent_id: id,
                content: output.content,
                input_tokens: output.token_usage.input_tokens,
                output_tokens: output.token_usage.output_tokens,
                reasoning_tokens: output.token_usage.reasoning_tokens.unwrap_or(0),
            })
            .collect(),
        total_input_tokens: output.total_token_usage.input_tokens,
        total_output_tokens: output.total_token_usage.output_tokens,
        total_reasoning_tokens: output.total_token_usage.reasoning_tokens.unwrap_or(0),
        token_usage_known: output.token_usage_known,
        completed_agents: output.completed_agents,
        failed_agents: output.failed_agents,
    }
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

/// Reserve the daemon's singleton Unix socket without needing runtime state.
///
/// CLI entrypoints call this immediately after config load, before bootstrap
/// can spawn actors or mutate persisted state. Holding the returned listener
/// holds the singleton reservation until it is attached with
/// [`serve_ipc_listener`].
pub async fn bind_ipc_listener(socket_path: &Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    // A second daemon must not unlink the live daemon's address. Only a socket
    // that no process accepts is stale; a regular file or symlink is never an
    // IPC cleanup target.
    match tokio::fs::symlink_metadata(socket_path).await {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "refusing to replace non-socket IPC path {}",
                        socket_path.display()
                    ),
                ));
            }
            if UnixStream::connect(socket_path).await.is_ok() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!(
                        "an Axocoatl daemon is already listening at {}",
                        socket_path.display()
                    ),
                ));
            }
            tokio::fs::remove_file(socket_path).await?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    // Ensure the socket's immediate parent exists. Only chmod a directory we
    // created ourselves: an explicit path such as `/tmp/axocoatl.sock` must
    // never make daemon startup change permissions on `/tmp` (or any other
    // pre-existing user directory).
    if let Some(parent) = socket_path.parent() {
        match tokio::fs::symlink_metadata(parent).await {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "refusing to use non-directory IPC parent {}",
                        parent.display()
                    ),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir_all(parent).await?;
                tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
            }
            Err(error) => return Err(error),
        }
    }

    let listener = UnixListener::bind(socket_path)?;
    if let Err(error) =
        tokio::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600)).await
    {
        drop(listener);
        let _ = tokio::fs::remove_file(socket_path).await;
        return Err(error);
    }
    tracing::info!(path = %socket_path.display(), "IPC server listening");
    Ok(listener)
}

/// Attach initialized daemon state to an already-reserved IPC listener.
pub fn serve_ipc_listener(
    listener: UnixListener,
    daemon: Arc<RwLock<AxocoatlDaemon>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut shutdown = daemon.read().await.shutdown_subscriber();
        let mut clients = tokio::task::JoinSet::new();
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                accepted = listener.accept() => match accepted {
                    Ok((stream, _addr)) => {
                        let daemon = daemon.clone();
                        clients.spawn(async move {
                            if let Err(e) = handle_client(stream, daemon).await {
                                tracing::debug!(error = %e, "IPC client disconnected");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "IPC accept error");
                    }
                },
                joined = clients.join_next(), if !clients.is_empty() => {
                    if let Some(Err(error)) = joined {
                        if !error.is_cancelled() {
                            tracing::debug!(error = %error, "IPC client task failed");
                        }
                    }
                }
            }
        }
        clients.abort_all();
        while let Some(joined) = clients.join_next().await {
            if let Err(error) = joined {
                if !error.is_cancelled() {
                    tracing::debug!(error = %error, "IPC client task failed during shutdown");
                }
            }
        }
    })
}

/// Compatibility helper that reserves and starts the IPC server in one call.
/// New daemon entrypoints should reserve with [`bind_ipc_listener`] before
/// bootstrap, then attach state through [`serve_ipc_listener`].
pub async fn start_ipc_server(
    daemon: Arc<RwLock<AxocoatlDaemon>>,
    socket_path: &Path,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let listener = bind_ipc_listener(socket_path).await?;
    Ok(serve_ipc_listener(listener, daemon))
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
                match daemon.execute_agent_measured(&agent_id, &input).await {
                    Ok(measured) => IpcResponse::Response {
                        token_usage_known: measured.token_usage_known,
                        content: measured.output.content,
                        tool_calls: measured
                            .output
                            .tool_calls
                            .into_iter()
                            .map(|tc| IpcToolCall {
                                tool_name: tc.tool_name,
                                arguments: tc.arguments,
                                result: tc.result,
                            })
                            .collect(),
                        input_tokens: measured.output.token_usage.input_tokens,
                        output_tokens: measured.output.token_usage.output_tokens,
                        reasoning_tokens: measured.output.token_usage.reasoning_tokens.unwrap_or(0),
                    },
                    Err(failure) => measured_daemon_failure_response(failure),
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
                let context = {
                    let daemon = daemon.read().await;
                    crate::automation_executor::AutomationExecutionContext::from_daemon(&daemon)
                };
                let result = match context.get_automation(&workflow_id).await {
                    Some(automation)
                        if matches!(
                            &automation.trigger,
                            axocoatl_config::AutomationTrigger::Manual
                        ) =>
                    {
                        let result = crate::automation_executor::execute_automation_in_context(
                            &context,
                            &automation,
                            &input,
                        )
                        .await;
                        crate::automation_runtime::record_automation_outcome(
                            &context,
                            &automation,
                            &result,
                        );
                        result
                    }
                    _ => Err(crate::DaemonError::WorkflowNotFound(workflow_id.clone())),
                };
                match result {
                    Ok(output) => workflow_response(output),
                    Err(error) => workflow_error_response(error),
                }
            }
            IpcRequest::ListWorkflows => {
                use axocoatl_config::{AutomationNodeKind, AutomationTrigger};

                let store = { daemon.read().await.automation_store.clone() };
                let automations = store.read().await.list();
                let workflows = automations
                    .into_iter()
                    .filter(|automation| matches!(&automation.trigger, AutomationTrigger::Manual))
                    .map(|automation| {
                        let agents = automation
                            .nodes
                            .iter()
                            .filter_map(|node| match &node.kind {
                                AutomationNodeKind::Agent { agent_id, .. } => {
                                    Some(agent_id.clone())
                                }
                                _ => None,
                            })
                            .collect();
                        let entry_point = automation.nodes.iter().find_map(|node| {
                            let has_incoming =
                                automation.edges.iter().any(|edge| edge.to == node.id);
                            match (&node.kind, has_incoming) {
                                (AutomationNodeKind::Agent { agent_id, .. }, false) => {
                                    Some(agent_id.clone())
                                }
                                _ => None,
                            }
                        });
                        IpcWorkflowInfo {
                            id: automation.id,
                            name: automation.name,
                            agents,
                            entry_point,
                        }
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
                let mut total_input: usize = 0;
                let mut total_output: usize = 0;
                let mut total_reasoning: usize = 0;
                let mut token_usage_known = true;
                for id in ids {
                    if let Some(actor) = daemon.agent_registry.get(&id).await {
                        if let Ok(measured) =
                            axocoatl_actor::get_agent_measured_token_usage(&actor).await
                        {
                            let usage = measured.usage;
                            token_usage_known &= measured.complete;
                            total_input = total_input.saturating_add(usage.input_tokens);
                            total_output = total_output.saturating_add(usage.output_tokens);
                            total_reasoning =
                                total_reasoning.saturating_add(usage.reasoning_tokens.unwrap_or(0));
                            per_agent.push(IpcTokenUsage {
                                agent_id: id.to_string(),
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                                reasoning_tokens: usage.reasoning_tokens,
                                token_usage_known: measured.complete,
                            });
                        }
                    }
                }
                IpcResponse::TokenUsage {
                    per_agent,
                    total_input,
                    total_output,
                    total_reasoning,
                    token_usage_known,
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
                    Err(error) => error_response(error),
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
                    Err(error) => error_response(error),
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
                match daemon.execute_session_measured(&session_id, &input).await {
                    Ok(measured) => IpcResponse::SessionResponse {
                        session_id,
                        content: measured.output.content,
                        input_tokens: measured.output.token_usage.input_tokens,
                        output_tokens: measured.output.token_usage.output_tokens,
                        reasoning_tokens: measured.output.token_usage.reasoning_tokens.unwrap_or(0),
                        token_usage_known: measured.token_usage_known,
                    },
                    Err(failure) => measured_daemon_failure_response(failure),
                }
            }
            IpcRequest::CloseSession { session_id } => {
                let daemon = daemon.read().await;
                match daemon.close_session(&session_id).await {
                    Ok(()) => IpcResponse::SessionClosed { session_id },
                    Err(error) => error_response(error),
                }
            }
            IpcRequest::Ping => IpcResponse::Pong,
            IpcRequest::Shutdown => {
                write_message(&mut stream, &IpcResponse::ShutdownAck).await?;
                daemon.read().await.request_shutdown();
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

    #[test]
    fn token_usage_wire_defaults_reasoning_and_totals_reported_reasoning() {
        let legacy: IpcResponse = serde_json::from_str(
            r#"{"type":"token_usage","per_agent":[],"total_input":10,"total_output":5}"#,
        )
        .unwrap();
        assert!(matches!(
            legacy,
            IpcResponse::TokenUsage {
                total_reasoning: 0,
                token_usage_known: false,
                ..
            }
        ));

        let usage = IpcTokenUsage {
            agent_id: "reasoner".to_string(),
            input_tokens: 10,
            output_tokens: 5,
            reasoning_tokens: Some(7),
            token_usage_known: true,
        };
        assert_eq!(usage.total(), 22);

        let legacy_response: IpcResponse = serde_json::from_str(
            r#"{"type":"response","content":"ok","tool_calls":[],"input_tokens":1,"output_tokens":2}"#,
        )
        .unwrap();
        assert!(matches!(
            legacy_response,
            IpcResponse::Response {
                reasoning_tokens: 0,
                token_usage_known: false,
                ..
            }
        ));

        let legacy_session: IpcResponse = serde_json::from_str(
            r#"{"type":"session_response","session_id":"s","content":"ok","input_tokens":3,"output_tokens":4}"#,
        )
        .unwrap();
        assert!(matches!(
            legacy_session,
            IpcResponse::SessionResponse {
                reasoning_tokens: 0,
                token_usage_known: false,
                ..
            }
        ));
    }

    #[test]
    fn error_wire_omits_unmeasured_usage_and_retains_measured_subtotals() {
        let legacy: IpcResponse =
            serde_json::from_str(r#"{"type":"error","message":"before dispatch"}"#).unwrap();
        assert!(matches!(
            legacy,
            IpcResponse::Error {
                input_tokens: None,
                output_tokens: None,
                reasoning_tokens: None,
                token_usage_known: None,
                ..
            }
        ));

        let generic = error_response("before dispatch");
        assert_eq!(
            serde_json::to_value(generic).unwrap(),
            serde_json::json!({"type": "error", "message": "before dispatch"})
        );

        let measured = measured_error_response(
            "provider stream failed",
            &axocoatl_core::TokenUsageStats::new(13, 8).with_reasoning(3),
            false,
        );
        assert_eq!(
            serde_json::to_value(measured).unwrap(),
            serde_json::json!({
                "type": "error",
                "message": "provider stream failed",
                "input_tokens": 13,
                "output_tokens": 8,
                "reasoning_tokens": 3,
                "token_usage_known": false,
            })
        );
    }

    #[test]
    fn measured_fatal_and_handled_workflow_failures_project_usage() {
        let failure = crate::bootstrap::MeasuredDaemonFailure {
            error: crate::DaemonError::AgentSpawn("provider stream failed".into()),
            token_usage: axocoatl_core::TokenUsageStats::new(21, 5),
            token_usage_known: false,
        };
        assert!(matches!(
            measured_daemon_failure_response(failure),
            IpcResponse::Error {
                input_tokens: Some(21),
                output_tokens: Some(5),
                reasoning_tokens: Some(0),
                token_usage_known: Some(false),
                ..
            }
        ));

        let output = crate::workflow::WorkflowOutput {
            workflow_id: "review".into(),
            agent_outputs: Vec::new(),
            agent_activations: Vec::new(),
            final_content: "partial".into(),
            total_token_usage: axocoatl_core::TokenUsageStats::new(34, 13).with_reasoning(2),
            token_usage_known: false,
            completed_agents: vec!["reader".into()],
            failed_agents: vec![("writer".into(), "provider timeout".into())],
        };
        match workflow_response(output) {
            IpcResponse::Error {
                message,
                input_tokens: Some(34),
                output_tokens: Some(13),
                reasoning_tokens: Some(2),
                token_usage_known: Some(false),
            } => {
                assert!(message.contains("writer: provider timeout"));
            }
            other => panic!("expected measured workflow error, got {other:?}"),
        }

        let fatal = crate::DaemonError::workflow_execution_measured(
            "automation graph failed",
            axocoatl_core::TokenUsageStats::new(55, 8),
            true,
        );
        assert!(matches!(
            workflow_error_response(fatal),
            IpcResponse::Error {
                input_tokens: Some(55),
                output_tokens: Some(8),
                reasoning_tokens: Some(0),
                token_usage_known: Some(true),
                ..
            }
        ));

        assert!(matches!(
            workflow_error_response(crate::DaemonError::WorkflowNotFound("missing".into())),
            IpcResponse::Error {
                input_tokens: Some(0),
                output_tokens: Some(0),
                reasoning_tokens: Some(0),
                token_usage_known: Some(true),
                ..
            }
        ));
    }

    #[test]
    fn socket_path_is_stable_across_working_directories() {
        assert_eq!(
            resolve_socket_path(
                None,
                Some("/run/user/501".into()),
                Some("/Users/axo".into())
            ),
            PathBuf::from("/run/user/501/axocoatl/axocoatl.sock")
        );
        assert_eq!(
            resolve_socket_path(None, None, Some("/Users/axo".into())),
            PathBuf::from("/Users/axo/.axocoatl/run/axocoatl.sock")
        );
        assert_eq!(
            resolve_socket_path(
                Some("/tmp/custom-axo.sock".into()),
                Some("/run/user/501".into()),
                Some("/Users/axo".into()),
            ),
            PathBuf::from("/tmp/custom-axo.sock")
        );
    }

    #[test]
    fn session_info_reports_setup_gate_and_reads_legacy_payloads() {
        let data = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("package-lock.json"), "{}").unwrap();
        let mut store = axocoatl_session::SessionStore::new(data.path()).unwrap();
        let session = store
            .create(
                "Node project",
                "wsp-node",
                work.path(),
                axocoatl_session::SessionMode::SingleAgent {
                    agent_id: "coder".into(),
                },
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap();
        let info = ipc_session_info(&session);
        assert_eq!(info.environment_state, "awaiting_approval");
        assert_eq!(info.setup_command.as_deref(), Some("npm ci"));

        let legacy: IpcSessionInfo = serde_json::from_str(
            r#"{"id":"s","name":"Old","workspace_id":"w","working_dir":"/tmp","mode":"single-agent","status":"active"}"#,
        )
        .unwrap();
        assert!(legacy.environment_state.is_empty());
        assert!(legacy.setup_command.is_none());
        assert!(legacy.environment_error.is_none());
    }

    #[tokio::test]
    async fn socket_reservation_rejects_a_second_daemon_before_state_exists() {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let nonce = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "axo-ipc-reservation-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("axocoatl.sock");

        let listener = bind_ipc_listener(&socket_path).await.unwrap();
        let error = bind_ipc_listener(&socket_path).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);

        drop(listener);
        std::fs::remove_file(&socket_path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

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
