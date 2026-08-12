//! Daemon bootstrap: config → providers → agents → coordination.
//! This is the integration point that wires all subsystems together.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axocoatl_actor::{
    AgentActor, AgentBehavior, AgentRegistry, CoordinatorBehavior, DefaultAgentBehavior,
    WorkerConfig, DEFAULT_WORKER_BUDGET,
};
use axocoatl_config::{AgentRoleYaml, AxocoatlConfig};
use axocoatl_coordination::{EventLattice, LatticeEvent};
use axocoatl_core::{AgentId, AgentRole};
use axocoatl_isolation::session_sandbox::{ExecResult, Sandbox, SessionSandbox};
use axocoatl_llm::ProviderRegistry;
use axocoatl_mcp::approval::{McpApprovalGate, SharedApprovalGate};
use axocoatl_mcp::permissions::McpPermissionStore;
use axocoatl_mcp::{McpToolRegistry, McpTransportType};
use axocoatl_memory::chat::ChatStore;
use axocoatl_memory::files::FileStore;
use axocoatl_memory::{CheckpointPolicy, CheckpointStore};
use axocoatl_session::{Session, SessionMode, SessionStore};
use axocoatl_token::{ApproximateCounter, TokenCounter};
use axocoatl_tools::ToolExecutor;
use ractor::Actor;

use crate::error::DaemonError;
use crate::scheduler::ScheduleTable;

/// Max lifetime of a remote E2B sandbox (seconds). The remote VM self-terminates
/// after this; keep-alive/refresh for very long remote sessions is a later pass.
const E2B_SESSION_TIMEOUT_SECS: u64 = 3600;

/// Runtime teardown is deliberately shorter than a lane command's normal
/// timeout. Discard must be able to break a review call that is waiting on the
/// very container being discarded, then acquire the workspace lease and remove
/// the clone only after every process owner is gone.
const ATTEMPT_ACTOR_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const ATTEMPT_CONTAINER_STOP_TIMEOUT: Duration = Duration::from_secs(10);
const ATTEMPT_OPERATION_RELEASE_TIMEOUT: Duration = Duration::from_secs(15);

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn judge_ranking_contract(candidate_indices: &[usize]) -> String {
    let indices = candidate_indices
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Candidate indexes are exactly [{indices}]. Return every index exactly once. \
         Assign unique integer ranks that are a permutation of 1 through {count}; \
         ties are forbidden. The winner must be the candidate whose rank is 1. \
         If candidates are otherwise indistinguishable, break the tie by lower \
         candidate index and say that the tie was broken deterministically.",
        count = candidate_indices.len(),
    )
}

/// Runtime ownership for one unresolved attempt set.
///
/// The durable manifest says what exists; this entry owns the process-local
/// things that must be joined before those files may be removed. A completed
/// lane deliberately stays here until Keep or Discard so cleanup has one path
/// for successful, failed, and still-running sets.
struct ActiveAttemptRun {
    set_id: String,
    actors: Vec<(AgentId, ractor::ActorRef<axocoatl_actor::AgentMessage>)>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// One real filesystem boundary per lane. Stopping these also kills any
    /// background command or PTY an attempt created outside its actor task.
    sandboxes: Vec<Arc<dyn Sandbox>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredCheckedTree {
    index: usize,
    commit_oid: String,
    tree_oid: String,
    patch_sha256: String,
    #[serde(default)]
    changes_gitlink: bool,
}

#[derive(Debug)]
struct CapturedCandidate {
    checked: StoredCheckedTree,
    paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct StoredFileFingerprint {
    kind: String,
    sha256: String,
    #[serde(default)]
    executable: bool,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredKeepPath {
    path: String,
    preimage: Option<StoredFileFingerprint>,
    postimage: Option<StoredFileFingerprint>,
}

/// Durable write-ahead journal for installing one checked candidate.
///
/// `preimage_tree` is the exact Git view of the primary working tree at the
/// Keep commit point. `postimage_tree` is computed by merging the protected
/// checked candidate commit over a protected preimage commit with their shared
/// attempt base. Only object ids cross process boundaries; the real index and
/// lossy patch text are never transaction inputs. Every affected leaf is then
/// copied once into the immutable raw `keep-apply/postimage` store before this
/// journal is published, then installed with a same-filesystem atomic rename
/// from a rebuildable `keep-apply/stage`. A retry classifies raw
/// kind/mode/bytes as preimage or postimage and continues after cancellation or
/// process death without depending on Git filters or container setup.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredKeepApply {
    index: usize,
    patch_sha256: String,
    candidate_tree: String,
    preimage_tree: String,
    postimage_tree: String,
    paths: Vec<StoredKeepPath>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredKeepReceipt {
    set_id: String,
    index: usize,
    status: crate::git::GitStatus,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredTranscriptCommit {
    base_checkpoint_version: u64,
    base_message_count: usize,
    task_sha256: String,
    assistant_sha256: String,
}

struct StreamAgentRunOptions {
    model_override: Option<String>,
    trace: Option<Arc<StdMutex<Vec<crate::trajectory::Action>>>>,
    supplied_history: Option<Vec<axocoatl_core::ChatMessage>>,
}

impl ActiveAttemptRun {
    fn new(set_id: impl Into<String>) -> Self {
        Self {
            set_id: set_id.into(),
            actors: Vec::new(),
            tasks: Vec::new(),
            sandboxes: Vec::new(),
        }
    }
}

/// Register every discovered MCP tool into `executor` under its qualified
/// `mcp__server__tool` name, so a model call reaches the `Mcp` backend and the
/// tool is advertised to the LLM. The executor must also be given the registry
/// (`with_mcp_registry`) for that backend to dispatch over the live connection.
fn register_discovered_mcp_tools(executor: &mut ToolExecutor, reg: &McpToolRegistry) {
    for def in reg.as_llm_tools() {
        if let Some(server) = reg.server_for_tool(&def.name).map(str::to_string) {
            executor.register_mcp(def.name.clone(), server, def);
        }
    }
}

/// Running state of the Axocoatl daemon.
pub struct AxocoatlDaemon {
    pub config: AxocoatlConfig,
    /// Resolved data directory (env `AXOCOATL_DATA_DIR` or `./data`). Used by
    /// any code that needs to place files under the daemon's storage root —
    /// the chat-attachment upload route is the first non-bootstrap consumer.
    pub data_dir: String,
    pub provider_registry: ProviderRegistry,
    pub agent_registry: AgentRegistry,
    pub counter: Arc<dyn TokenCounter>,
    pub checkpoint_store: Arc<CheckpointStore>,
    pub event_lattice: Arc<EventLattice>,
    /// MCP server registry. Held behind a `RwLock` because the dashboard's
    /// Gallery "Install" flow connects new servers at runtime — that mutates
    /// the index. Reads (tool listing, dispatch) take the read lock.
    pub mcp_registry: Arc<tokio::sync::RwLock<McpToolRegistry>>,
    /// Persisted MCP permission decisions ("Allow this agent on this server"
    /// etc.). Consulted before every MCP tool call; misses route to the gate.
    pub mcp_permissions: Arc<tokio::sync::RwLock<McpPermissionStore>>,
    /// In-memory gate: when an MCP tool call has no recorded decision,
    /// the executor parks here while the dashboard prompts the user.
    pub mcp_approval_gate: SharedApprovalGate,
    /// Shared hook registry — registers the MCP approval hook globally
    /// so every agent's tool calls flow through the permission gate.
    pub hook_registry: Arc<axocoatl_tools::HookRegistry>,
    pub schedule_table: ScheduleTable,
    /// Rebuildable observations for event- and Skill-triggered Automations,
    /// exposed through the compatibility `/api/proactive` route.
    pub proactive_table: crate::proactive::ProactiveTable,
    /// Persistent store of directory sessions.
    pub session_store: Arc<tokio::sync::Mutex<SessionStore>>,
    /// Persistent store for the retained lightweight-chat API (no directory or
    /// sandbox). Loaded from {data_dir}/chats/*.json at boot. Atomic temp+rename
    /// JSON writes per chat — see [`ChatStore::persist`].
    pub chat_store: Arc<tokio::sync::Mutex<ChatStore>>,
    /// Content-addressed file store — the local "Files API". Files are keyed
    /// by SHA-256 of their bytes, dedup'd across all chats that reference them.
    /// Extracted text (PDF/CSV/XLSX/OCR) is cached on disk so re-attaching
    /// the same file doesn't re-parse.
    pub file_store: Arc<tokio::sync::Mutex<FileStore>>,
    /// Persistent store of unified Automations — single source of truth
    /// for what runs. Seeded once from the legacy YAML sections at first
    /// boot, after which the dashboard editor writes here directly.
    pub automation_store: Arc<tokio::sync::RwLock<crate::automation_store::AutomationStore>>,
    /// HITL interrupts keyed by `{automation_id}:{run_id}:{node_id}`. Live
    /// executors block on `notify.notified()`; bootstrap also reconstructs
    /// entries from persisted `interrupt_parked` checkpoints so the dashboard
    /// can resume them after process restart.
    pub pending_interrupts: Arc<
        tokio::sync::RwLock<std::collections::HashMap<String, crate::interrupt::PendingInterrupt>>,
    >,
    /// Per-automation run history + checkpoints — the time-travel store.
    /// Writes happen from inside the executor after every node completion.
    pub run_store: Arc<crate::automation_runs::AutomationRunStore>,
    /// Live session isolation instances (local Podman container or remote
    /// microVM), keyed by session id. Trait-typed so the backend is pluggable.
    session_sandboxes: Arc<tokio::sync::Mutex<HashMap<String, Arc<dyn Sandbox>>>>,
    /// Per-session singleflight for deterministic sandbox startup. Two starts
    /// with the same Podman name would otherwise remove each other's container.
    sandbox_starts: Arc<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Files the session's agent wrote during its most recent turn.
    ///
    /// Recorded in the daemon rather than assembled in a viewer from live
    /// frames: "what did the agent just change" has to survive a reload, and a
    /// fact that only exists while a tab is open is not one a reviewer can rely
    /// on. Replaced wholesale each turn — it answers *last*, not *ever*.
    session_last_turn: Arc<StdMutex<HashMap<String, Vec<String>>>>,
    /// Ring buffer of the most recent lattice events (capped at 200).
    pub event_log: Arc<StdMutex<VecDeque<LatticeEvent>>>,
    /// The observability stream bus — flattened events + live agent tokens.
    /// Every app or compatibility WebSocket subscribes to this.
    pub stream_bus: tokio::sync::broadcast::Sender<crate::stream::StreamFrame>,
    /// Live state of every in-flight workflow run, rebuilt from the bus.
    /// A freshly-connected WebSocket reads this to re-attach to a run.
    pub active_runs: Arc<StdMutex<std::collections::HashMap<String, crate::stream::RunState>>>,
    /// In-flight chat turns. Keyed by chat_id; the sender fires to ask the
    /// WS handler to stop forwarding tokens and finalize. v1 limitation:
    /// the underlying provider call keeps running in the background — we
    /// stop the visible stream but token cost is still paid. A true abort
    /// would require provider-level cancellation hooks.
    pub active_chat_turns:
        Arc<tokio::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>,
    /// Process-local ownership of attempt actors and lane tasks, keyed by
    /// session id. The durable current-set manifest is the cross-restart source
    /// of truth; this registry exists to make teardown ordered and awaitable.
    active_attempts: Arc<tokio::sync::Mutex<HashMap<String, ActiveAttemptRun>>>,
    /// Out-of-band cancellation marks let Discard/close interrupt live lanes or
    /// a long repository check even while a review/check call owns the workspace
    /// operation.
    attempt_cancellations: Arc<tokio::sync::Mutex<HashSet<String>>>,
    /// Serializes every workspace-owning operation for a session across the
    /// full async operation, closing start/turn/review/decision TOCTOU races.
    attempt_operations: Arc<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    pub tool_executor: Arc<ToolExecutor>,
    shared_registry: Arc<axocoatl_memory::SharedBlockRegistry>,
    agent_handles: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for AxocoatlDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AxocoatlDaemon")
            .field("agents", &self.config.agents.len())
            .finish_non_exhaustive()
    }
}

/// An agent's event-lattice activation params: `(threshold, decay_rate)`.
///
/// Uses the per-agent `activation_threshold` / `activation_decay` overrides when
/// set; otherwise the default — entry agents (no `depends_on`) get `(1.0, 0.0)`
/// so a single `UserInput` (signal 1.0) activates them, and downstream agents
/// get `(0.5 × N, 0.01)` so they fire once their N dependencies' `TaskCompleted`
/// signals (0.5 each) accumulate.
fn lattice_params(agent_yaml: &axocoatl_config::AgentConfigYaml) -> (f32, f32) {
    let (mut threshold, mut decay_rate) = if agent_yaml.depends_on.is_empty() {
        (1.0_f32, 0.0_f32)
    } else {
        (agent_yaml.depends_on.len() as f32 * 0.5, 0.01)
    };
    if let Some(t) = agent_yaml.activation_threshold {
        threshold = t;
    }
    if let Some(d) = agent_yaml.activation_decay {
        decay_rate = d;
    }
    (threshold, decay_rate)
}

impl AxocoatlDaemon {
    /// Bootstrap a daemon from a parsed config.
    pub async fn bootstrap(config: AxocoatlConfig) -> Result<Self, DaemonError> {
        let counter: Arc<dyn TokenCounter> = Arc::new(
            ApproximateCounter::new()
                .map_err(|e| DaemonError::Provider(format!("Token counter init: {e}")))?,
        );

        // 1. Set up providers
        let mut provider_registry = ProviderRegistry::new();
        Self::setup_providers(&config, &mut provider_registry)?;

        // 2. Set up checkpoint store
        let data_dir = std::env::var("AXOCOATL_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
        // Harden the data root up front: 0700 so no other local user can
        // traverse into the persisted checkpoints / transcripts / memory below
        // it. This is the umbrella over the per-file 0600 modes in the stores.
        if let Err(e) = std::fs::create_dir_all(&data_dir) {
            tracing::warn!(path = %data_dir, error = %e, "could not create data dir");
        }
        axocoatl_memory::perms::restrict_dir(std::path::Path::new(&data_dir));
        let checkpoint_store = Arc::new(CheckpointStore::new(
            format!("{data_dir}/checkpoints"),
            CheckpointPolicy::EveryLlmCall,
        ));

        // 3. Core memory (Tier 3): build the shared-block registry once, from the
        //    union of all agents' `shared: true` blocks. Per-agent block stores are
        //    built at spawn time.
        let mut shared_registry = axocoatl_memory::SharedBlockRegistry::new(
            axocoatl_memory::shared_blocks_dir(&data_dir),
        );
        for agent in &config.agents {
            let core = agent.memory.core.to_core();
            for block in core.blocks.iter().filter(|b| b.shared) {
                shared_registry
                    .ensure(axocoatl_memory::MemoryBlock::from(block))
                    .await;
            }
        }
        let shared_registry = Arc::new(shared_registry);

        // 5. Set up tool executor with built-in tools. Kept mutable: discovered
        // MCP tools are registered into it below, once the MCP registry is up,
        // before it's shared.
        let mut tool_executor = ToolExecutor::new();
        tool_executor.register_builtin("echo", Arc::new(axocoatl_tools::EchoTool));
        tool_executor.register_builtin("json_keys", Arc::new(axocoatl_tools::JsonKeysTool));
        tool_executor.register_builtin("text_split", Arc::new(axocoatl_tools::TextSplitTool));

        // 6. Set up agent registry
        let agent_registry = AgentRegistry::new();

        // 6b. Set up the observability stream bus EARLY — every approval
        // prompt + every WS frame routes through this. Spawning agents
        // after this lets the hook registry capture a bus handle.
        let (stream_bus, _) = tokio::sync::broadcast::channel(4096);

        // 6c. Connect to configured MCP servers BEFORE spawning agents so
        // the hook registry has a live registry to ask about tool ownership.
        // A failing server logs a warning but does not abort bootstrap.
        let mut mcp_registry = McpToolRegistry::new();
        for mcp in &config.mcp_servers {
            let transport = match mcp.transport.as_str() {
                "stdio" => {
                    let Some(command) = mcp.command.clone() else {
                        tracing::warn!(server = %mcp.name, "stdio MCP server missing 'command', skipping");
                        continue;
                    };
                    McpTransportType::Stdio {
                        command,
                        args: mcp.args.clone(),
                        env: mcp.env.clone(),
                    }
                }
                "streamable_http" | "http" => {
                    let Some(url) = mcp.url.clone() else {
                        tracing::warn!(server = %mcp.name, "http MCP server missing 'url', skipping");
                        continue;
                    };
                    McpTransportType::StreamableHttp {
                        url,
                        headers: mcp.headers.clone(),
                    }
                }
                other => {
                    tracing::warn!(server = %mcp.name, transport = %other, "Unknown MCP transport, skipping");
                    continue;
                }
            };

            match mcp_registry.connect_server(&mcp.name, transport).await {
                Ok(()) => tracing::info!(server = %mcp.name, "Connected to MCP server"),
                Err(e) => {
                    tracing::warn!(server = %mcp.name, error = %e, "Failed to connect to MCP server (continuing)")
                }
            }
        }
        if !config.mcp_servers.is_empty() {
            tracing::info!(
                servers = mcp_registry.servers().len(),
                tools = mcp_registry.tool_count(),
                "MCP registry initialized"
            );
        }
        let mcp_registry = Arc::new(tokio::sync::RwLock::new(mcp_registry));

        // Advertise + route discovered MCP tools through the shared executor:
        // register each tool so a model call reaches the Mcp backend (and is
        // advertised to the LLM), and hand the executor the registry so that
        // backend dispatches over the persistent client. Done before the
        // executor is shared so every agent sees the MCP tools.
        {
            let reg = mcp_registry.read().await;
            register_discovered_mcp_tools(&mut tool_executor, &reg);
        }
        let tool_executor = Arc::new(tool_executor.with_mcp_registry(mcp_registry.clone()));

        // MCP permission decisions live in a single JSON file alongside
        // chats and files. Missing file = empty store, which means every
        // tool call hits the approval gate (correct first-boot behavior).
        let mcp_permissions = {
            let path = std::path::PathBuf::from(format!("{data_dir}/mcp-permissions.json"));
            let store = McpPermissionStore::open(&path)
                .map_err(|e| DaemonError::Session(format!("mcp permissions: {e}")))?;
            Arc::new(tokio::sync::RwLock::new(store))
        };
        let mcp_approval_gate: SharedApprovalGate = Arc::new(McpApprovalGate::new());

        // 6d. Build the global HookRegistry with the MCP approval gate so
        // every MCP tool call hits the human-in-the-loop check (unless a
        // recorded permission already says Allow/Deny).
        let mut hook_registry = axocoatl_tools::HookRegistry::new();
        hook_registry.register_global(Arc::new(crate::mcp_approval_hook::McpApprovalHook::new(
            mcp_registry.clone(),
            mcp_permissions.clone(),
            mcp_approval_gate.clone(),
            stream_bus.clone(),
        )));
        let hook_registry = Arc::new(hook_registry);

        // User-defined `hooks:` are parsed but not yet executed by the runtime —
        // only the built-in MCP approval hook above runs. Warn once at startup so
        // a configured-but-inert `hooks:` section is never a silent no-op.
        if !config.hooks.is_empty() {
            tracing::warn!(
                count = config.hooks.len(),
                "config `hooks:` is experimental and not yet active; only the built-in MCP approval hook runs"
            );
        }

        // 7. Spawn agents (deferred from earlier so the hook registry exists)
        let mut agent_handles = Vec::new();
        for agent_yaml in &config.agents {
            // Workers are spawned on demand by their coordinator, not as
            // standalone top-level agents — skip them in the main spawn loop.
            if matches!(agent_yaml.role, AgentRoleYaml::Worker) {
                continue;
            }
            let handle = Self::spawn_agent(
                agent_yaml,
                &config,
                &provider_registry,
                &counter,
                &checkpoint_store,
                &tool_executor,
                &shared_registry,
                &agent_registry,
                &hook_registry,
                &stream_bus,
            )
            .await?;
            agent_handles.push(handle);
        }

        // 8. Set up the event lattice used by Skills, Automation triggers,
        //    webhooks, the recent-events API, and compatibility event frames.
        let event_lattice = Arc::new(EventLattice::new(256));

        for agent_yaml in &config.agents {
            // Workers aren't lattice-activated — their coordinator drives them.
            if matches!(agent_yaml.role, AgentRoleYaml::Worker) {
                continue;
            }
            let agent_id = AgentId::new(&agent_yaml.id);
            // Preserve each agent's coordination metadata in the lattice.
            // Runtime execution is owned by sessions and AutomationStore; there
            // is no second config-owned activation runner.
            let (threshold, decay_rate) = lattice_params(agent_yaml);
            event_lattice.register_agent(agent_id, threshold, decay_rate);
        }

        tracing::info!(
            agents_in_lattice = event_lattice.agent_count(),
            "Registered agents in event lattice"
        );

        // 9b. Run tracker — folds bus frames into the live state of every
        //     in-flight run so a reconnecting client can re-attach.
        let active_runs: Arc<StdMutex<std::collections::HashMap<String, crate::stream::RunState>>> =
            Arc::new(StdMutex::new(std::collections::HashMap::new()));
        {
            let runs = active_runs.clone();
            let mut rx = stream_bus.subscribe();
            tokio::spawn(async move {
                use tokio::sync::broadcast::error::RecvError;
                loop {
                    match rx.recv().await {
                        Ok(frame) => {
                            if let Ok(mut map) = runs.lock() {
                                crate::stream::apply_frame(&mut map, &frame);
                            }
                        }
                        // A token flood can make the tracker lag — skip the
                        // gap and keep going; never let the task die.
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => break,
                    }
                }
            });
        }

        // 10. Spawn the event subscriber — keeps the last 200 lattice events in
        // a ring buffer for the integration API and bridges every event onto
        // the stream bus for app and compatibility WebSocket observers.
        let event_log: Arc<StdMutex<VecDeque<LatticeEvent>>> =
            Arc::new(StdMutex::new(VecDeque::with_capacity(200)));
        let log_for_task = event_log.clone();
        let mut event_rx = event_lattice.subscribe();
        let lattice_for_task = event_lattice.clone();
        let bus_for_bridge = stream_bus.clone();
        tokio::spawn(async move {
            while let Ok(notif) = event_rx.recv().await {
                // Bridge to the stream bus for WebSocket observers.
                let _ = bus_for_bridge.send(crate::stream::event_frame(&notif));
                // Keep the ring buffer for the recent-events API.
                if let Some(full) = lattice_for_task.get_event(&notif.event_id) {
                    if let Ok(mut log) = log_for_task.lock() {
                        if log.len() >= 200 {
                            log.pop_front();
                        }
                        log.push_back(full);
                    }
                }
            }
        });

        // 11. Spawn the lattice event-egress (webhook) dispatcher — only when
        //     webhooks are configured, so a default install makes zero outbound
        //     requests and the air-gapped story holds.
        if !config.webhooks.is_empty() {
            tokio::spawn(crate::webhook::run_webhook_dispatcher(
                event_lattice.subscribe(),
                config.webhooks.clone(),
            ));
        }

        // Directory sessions — load any persisted sessions from disk.
        let session_store = {
            let mut store = SessionStore::new(format!("{data_dir}/sessions"))
                .map_err(|e| DaemonError::Session(e.to_string()))?;
            if let Err(e) = store.load_all() {
                tracing::warn!(error = %e, "failed to load some sessions");
            }
            // Seed the "demo-counters" session if it doesn't already exist.
            // This gives a fresh install a one-prompt-away demo of the
            // spawn_terminal tool: open the session, ask the agent to make
            // counters in Python, watch them run live in the Terminals
            // pane.  Idempotent — subsequent boots skip when present.
            let demo_name = "demo-counters";
            let already_present = store.list().iter().any(|s| s.name == demo_name);
            if !already_present {
                let demo_dir = format!("{data_dir}/demos/counters");
                match std::fs::create_dir_all(&demo_dir) {
                    Ok(()) => match store.create(
                        demo_name,
                        &demo_dir,
                        SessionMode::SingleAgent {
                            agent_id: "coder".to_string(),
                        },
                        Vec::new(),
                        Vec::new(),
                        None,
                    ) {
                        Ok(s) => tracing::info!(
                            session_id = %s.id, name = %s.name, dir = %demo_dir,
                            "seeded demo session"
                        ),
                        Err(e) => tracing::warn!(error = %e, "failed to seed demo session"),
                    },
                    Err(e) => {
                        tracing::warn!(error = %e, dir = %demo_dir, "failed to mkdir demo dir")
                    }
                }
            }
            Arc::new(tokio::sync::Mutex::new(store))
        };

        // Reap orphaned session sandbox containers from prior runs before any
        // session opens. A leftover container holds its published host ports
        // and makes new sessions fail to start their port proxy ("proxy
        // already running"). Best-effort — a no-op when podman isn't running.
        {
            let known: Vec<String> = session_store
                .lock()
                .await
                .list()
                .iter()
                .map(|s| s.id.clone())
                .collect();
            SessionSandbox::reap_orphans(&known).await;
        }

        // Lightweight chats — load any persisted chats from disk.
        // Distinct from sessions: no directory, no sandbox, just agent + history.
        let chat_store = {
            let mut store = ChatStore::new(format!("{data_dir}/chats"))
                .map_err(|e| DaemonError::Session(e.to_string()))?;
            if let Err(e) = store.load_all() {
                tracing::warn!(error = %e, "failed to load some chats");
            }
            Arc::new(tokio::sync::Mutex::new(store))
        };

        // Content-addressed file store (the local "Files API"). Mounted at
        // {data_dir}/files. Sidecars carry extracted text so we never re-parse.
        let file_store = {
            let mut store = FileStore::new(format!("{data_dir}/files"))
                .map_err(|e| DaemonError::Session(e.to_string()))?;
            if let Err(e) = store.load_all() {
                tracing::warn!(error = %e, "failed to load some files");
            }
            Arc::new(tokio::sync::Mutex::new(store))
        };

        // Run history store. Each execution writes checkpoints under
        // {data_dir}/runs/{automation_id}/{run_id}.json.
        let run_store = Arc::new(
            crate::automation_runs::AutomationRunStore::open(format!("{data_dir}/runs"))
                .map_err(|e| DaemonError::Session(e.to_string()))?,
        );
        let abandoned_runs = run_store
            .reconcile_orphaned_running(
                "Daemon restarted before this Automation run reached a durable Interrupt or terminal state.",
            )
            .await
            .map_err(|e| {
                DaemonError::Session(format!(
                    "could not reconcile Automation runs during restart: {e}"
                ))
            })?;
        if !abandoned_runs.is_empty() {
            tracing::warn!(
                abandoned_runs = abandoned_runs.len(),
                "marked orphaned Automation runs failed during restart"
            );
        }

        // Unified Automation store. Lives at {data_dir}/automations.json.
        // First-boot seed: project the legacy YAML sections through
        // `Automation::from_legacy` into the store. Subsequent boots use
        // the file as-is — the dashboard editor is the authority.
        let automation_store = {
            let path = std::path::PathBuf::from(format!("{data_dir}/automations.json"));
            let mut store = crate::automation_store::AutomationStore::open(&path)
                .map_err(|e| DaemonError::Session(e.to_string()))?;
            let seeded = store
                .seed_from_legacy_if_empty(&config)
                .map_err(|e| DaemonError::Session(format!("automation store seed failed: {e}")))?;
            if seeded {
                tracing::info!(
                    automations = store.len(),
                    "seeded automation store from legacy YAML sections"
                );
            } else {
                tracing::debug!(
                    automations = store.len(),
                    "automation store already initialized; skipping legacy seed"
                );
            }
            Arc::new(tokio::sync::RwLock::new(store))
        };

        let pending_interrupts = Arc::new(tokio::sync::RwLock::new(
            crate::automation_executor::rehydrate_pending_interrupts(&automation_store, &run_store)
                .await,
        ));
        let recovered_interrupts = pending_interrupts.read().await.len();
        if recovered_interrupts > 0 {
            tracing::info!(
                recovered_interrupts,
                "rehydrated pending Automation Interrupts from checkpoints"
            );
        }

        tracing::info!(agents = config.agents.len(), "Axocoatl daemon bootstrapped");

        Ok(Self {
            config,
            data_dir: data_dir.clone(),
            provider_registry,
            agent_registry,
            counter,
            checkpoint_store,
            event_lattice,
            mcp_registry,
            mcp_permissions,
            mcp_approval_gate,
            hook_registry,
            schedule_table: Arc::new(std::sync::Mutex::new(Vec::new())),
            proactive_table: Arc::new(std::sync::Mutex::new(Vec::new())),
            session_store,
            chat_store,
            file_store,
            active_chat_turns: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            active_attempts: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            attempt_cancellations: Arc::new(tokio::sync::Mutex::new(HashSet::new())),
            attempt_operations: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            automation_store,
            pending_interrupts,
            run_store,
            session_sandboxes: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            sandbox_starts: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            session_last_turn: Arc::new(StdMutex::new(HashMap::new())),
            event_log,
            stream_bus,
            active_runs,
            tool_executor,
            shared_registry,
            agent_handles: std::sync::Mutex::new(agent_handles),
        })
    }

    /// Spawn a single agent: build its provider + behavior, start the actor,
    /// and register it. Shared by bootstrap and `restart_agent`.
    #[allow(clippy::too_many_arguments)]
    /// Resolve an agent's shared blocks (its `shared: true` labels) against the
    /// process-wide registry into a per-agent map.
    fn resolve_shared(
        core: &axocoatl_core::CoreMemoryConfig,
        registry: &axocoatl_memory::SharedBlockRegistry,
    ) -> std::collections::HashMap<String, axocoatl_memory::SharedBlock> {
        let mut m = std::collections::HashMap::new();
        for spec in core.blocks.iter().filter(|b| b.shared) {
            if let Some(sb) = registry.get(&spec.label) {
                m.insert(spec.label.clone(), sb);
            }
        }
        m
    }

    /// Build (load + seed) an agent's per-agent core-memory store.
    async fn build_core(
        agent_id: &str,
        data_dir: &str,
        core: &axocoatl_core::CoreMemoryConfig,
    ) -> Arc<tokio::sync::RwLock<axocoatl_memory::CoreMemoryStore>> {
        let specs: Vec<axocoatl_memory::MemoryBlock> = core
            .blocks
            .iter()
            .map(axocoatl_memory::MemoryBlock::from)
            .collect();
        let store = axocoatl_memory::build_store(
            agent_id,
            axocoatl_memory::core_store_path(data_dir, agent_id),
            &specs,
        )
        .await;
        Arc::new(tokio::sync::RwLock::new(store))
    }

    // The full dependency stack a behavior may need is threaded explicitly
    // rather than bundled into a context struct — each is a distinct shared
    // handle and the call sites are few (bootstrap + restart).

    /// The `fallback: "provider:model"` string declared on a provider's config,
    /// if any. Only the registry providers carry a fallback field.
    fn provider_fallback_spec<'a>(config: &'a AxocoatlConfig, name: &str) -> Option<&'a str> {
        let p = &config.providers;
        let creds = match name {
            "openai" => p.openai.as_ref(),
            "anthropic" => p.anthropic.as_ref(),
            "gemini" => p.gemini.as_ref(),
            "mistral" => p.mistral.as_ref(),
            "openrouter" => p.openrouter.as_ref(),
            _ => None,
        }?;
        creds.fallback.as_deref()
    }

    /// Resolve a provider's configured `fallback` into a concrete backup
    /// provider + model. The spec is `"provider:model"`; a bare `"provider"`
    /// uses that provider's own default model. Returns `None` (with a warning)
    /// when nothing is configured or the backup provider isn't available.
    fn resolve_fallback(
        provider_name: &str,
        config: &AxocoatlConfig,
        registry: &ProviderRegistry,
    ) -> Option<axocoatl_llm::FallbackTarget> {
        let spec = Self::provider_fallback_spec(config, provider_name)?;
        let (backup_name, backup_model) = match spec.split_once(':') {
            Some((p, m)) => (p.trim(), Some(m.trim().to_string())),
            None => (spec.trim(), None),
        };
        let provider: Arc<dyn axocoatl_llm::LlmProvider> = if backup_name == "ollama" {
            let ollama = config.providers.ollama.as_ref()?;
            let model = backup_model
                .clone()
                .or_else(|| ollama.model.clone())
                .unwrap_or_else(|| "llama3.2".to_string());
            Arc::new(axocoatl_llm_ollama::OllamaProvider::with_base_url(
                &ollama.base_url,
                &model,
            ))
        } else {
            match registry.get(backup_name) {
                Some(p) => p.clone(),
                None => {
                    tracing::warn!(
                        provider = %provider_name,
                        fallback = %backup_name,
                        "fallback provider is not configured; ignoring fallback",
                    );
                    return None;
                }
            }
        };
        let model = backup_model.unwrap_or_else(|| provider.model_id().to_string());
        tracing::info!(
            provider = %provider_name,
            fallback = %backup_name,
            model = %model,
            "provider rate-limit fallback enabled",
        );
        Some(axocoatl_llm::FallbackTarget { provider, model })
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_agent(
        agent_yaml: &axocoatl_config::AgentConfigYaml,
        config: &AxocoatlConfig,
        provider_registry: &ProviderRegistry,
        counter: &Arc<dyn TokenCounter>,
        checkpoint_store: &Arc<CheckpointStore>,
        tool_executor: &Arc<ToolExecutor>,
        shared_registry: &Arc<axocoatl_memory::SharedBlockRegistry>,
        agent_registry: &AgentRegistry,
        hook_registry: &Arc<axocoatl_tools::HookRegistry>,
        stream_bus: &tokio::sync::broadcast::Sender<crate::stream::StreamFrame>,
    ) -> Result<tokio::task::JoinHandle<()>, DaemonError> {
        let agent_config = agent_yaml.to_core();
        let agent_id = agent_config.id.clone();
        let provider_name = &agent_config.provider;

        // Per-agent provider: Ollama agents get their own provider with the
        // agent's configured model. Other providers use the global registry.
        let provider: Arc<dyn axocoatl_llm::LlmProvider> = if provider_name == "ollama" {
            let ollama = config.providers.ollama.as_ref().ok_or_else(|| {
                DaemonError::Provider(format!(
                    "Ollama provider not configured for agent '{}'",
                    agent_id
                ))
            })?;
            let model = if agent_yaml.model.is_empty() {
                ollama.model.as_deref().unwrap_or("llama3.2")
            } else {
                &agent_yaml.model
            };
            tracing::info!(agent = %agent_id, model = %model, "Creating per-agent Ollama provider");
            Arc::new(axocoatl_llm_ollama::OllamaProvider::with_base_url(
                &ollama.base_url,
                model,
            ))
        } else {
            provider_registry
                .get(provider_name)
                .cloned()
                .ok_or_else(|| {
                    DaemonError::Provider(format!(
                        "Provider '{}' not configured for agent '{}'",
                        provider_name, agent_id
                    ))
                })?
        };

        // Opt-in rate-limit fallback: if this agent's provider declares a
        // `fallback: "provider:model"`, wrap it so a rate-limited primary
        // retries on the backup. No fallback configured -> unchanged provider.
        let provider = match Self::resolve_fallback(provider_name, config, provider_registry) {
            Some(target) => Arc::new(axocoatl_llm::FallbackProvider::new(provider, Some(target)))
                as Arc<dyn axocoatl_llm::LlmProvider>,
            None => provider,
        };

        // Select the behavior by role. A Coordinator orchestrates a pool of
        // workers (the config's role:Worker agents), spawning and assigning them
        // on demand; every other role runs the standard solo agent behavior.
        let behavior: Box<dyn AgentBehavior> =
            if matches!(agent_config.role, AgentRole::Coordinator) {
                let data_dir =
                    std::env::var("AXOCOATL_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
                // The coordinator gets the full dependency stack so it can give
                // its workers checkpointing + memory + hooks, and checkpoint its
                // own orchestration for resumable runs.
                let mut coord = CoordinatorBehavior::new(provider, counter.clone())
                    .with_model(agent_config.model.clone())
                    .with_tool_executor(tool_executor.clone())
                    .with_checkpoint_store(checkpoint_store.clone())
                    .with_shared_blocks(Self::resolve_shared(
                        &agent_config.memory.core,
                        shared_registry,
                    ))
                    .with_hook_registry(hook_registry.clone())
                    .with_data_dir(data_dir)
                    // Forward Layer-2 run progress (decompose → auction → workers)
                    // onto the stream bus so the dashboard run view can render it.
                    .with_reporter(Arc::new(crate::stream::CoordinatorStreamReporter::new(
                        stream_bus.clone(),
                    )));

                // Workers are scoped to THIS coordinator's workflow(s): the
                // workflows whose entry_point is this coordinator (union across
                // several). Config validation guarantees every worker belongs to
                // a coordinator-led workflow, so none are orphaned.
                let worker_ids: std::collections::HashSet<&str> = config
                    .workflows
                    .iter()
                    .filter(|wf| wf.entry_point.as_deref() == Some(agent_yaml.id.as_str()))
                    .flat_map(|wf| wf.agents.iter().map(String::as_str))
                    .collect();
                for w in &config.agents {
                    if matches!(w.role, AgentRoleYaml::Worker) && worker_ids.contains(w.id.as_str())
                    {
                        coord = coord.add_worker_config(WorkerConfig {
                            id: AgentId::new(&w.id),
                            name: w.name.clone(),
                            system_prompt: w.system_prompt.clone().unwrap_or_default(),
                            tools: w.tools.clone(),
                            // The declared worker's own configured model.
                            model: w.model.clone(),
                            token_budget: w
                                .token_budget
                                .as_ref()
                                .map(|b| b.per_execution)
                                .unwrap_or(DEFAULT_WORKER_BUDGET),
                            recall: w.memory.recall.to_core(),
                        });
                    }
                }
                // Load HTN decomposition methods from this coordinator's
                // workflow, if it declares an htn_methods_file. Non-fatal: on any
                // failure the coordinator falls back to LLM decomposition.
                if let Some(path) = config
                    .workflows
                    .iter()
                    .find(|wf| wf.entry_point.as_deref() == Some(agent_yaml.id.as_str()))
                    .and_then(|wf| wf.htn_methods_file.as_deref())
                {
                    match std::fs::read_to_string(path)
                        .map_err(|e| e.to_string())
                        .and_then(|s| axocoatl_coordination::HtnPlanner::from_methods_yaml(&s))
                    {
                        Ok(planner) => {
                            tracing::info!(agent = %agent_id, file = %path, "Loaded HTN methods");
                            coord = coord.with_htn_methods(planner);
                        }
                        Err(e) => tracing::warn!(
                            agent = %agent_id, file = %path, error = %e,
                            "HTN methods unavailable; coordinator uses LLM decomposition"
                        ),
                    }
                }
                Box::new(coord)
            } else {
                let mut behavior = DefaultAgentBehavior::new(provider, counter.clone())
                    .with_checkpoint_store(checkpoint_store.clone())
                    .with_tool_executor(tool_executor.clone())
                    .with_hook_registry(hook_registry.clone())
                    .with_sampling(agent_config.sampling.clone());

                // Tier 4 semantic memory — one store per agent, for cross-session
                // recall. A failure here is non-fatal: the agent runs without it.
                let data_dir =
                    std::env::var("AXOCOATL_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
                // Daily log — context compaction archives raw conversation
                // segments here before summarizing, so no history is lost.
                behavior = behavior.with_daily_log(Arc::new(axocoatl_memory::DailyLogMemory::new(
                    agent_id.to_string(),
                    format!("{data_dir}/memory/daily_log"),
                )));
                match axocoatl_memory::SemanticMemory::new(
                    &agent_id.to_string(),
                    format!("{data_dir}/memory/semantic"),
                ) {
                    Ok(sem) => behavior = behavior.with_semantic_memory(Arc::new(sem)),
                    Err(e) => {
                        tracing::warn!(agent = %agent_id, error = %e, "semantic memory unavailable")
                    }
                }
                // Core memory (Tier 3) — per-agent editable blocks + shared blocks.
                let core_store =
                    Self::build_core(&agent_id.to_string(), &data_dir, &agent_config.memory.core)
                        .await;
                behavior = behavior.with_core_memory(
                    core_store,
                    Self::resolve_shared(&agent_config.memory.core, shared_registry),
                );
                Box::new(behavior)
            };

        let (actor_ref, handle) = AgentActor::spawn(
            Some(agent_id.to_string()),
            AgentActor,
            (agent_config, behavior),
        )
        .await
        .map_err(|e| DaemonError::AgentSpawn(format!("{}: {e}", agent_id)))?;

        agent_registry.register(agent_id.clone(), actor_ref).await;
        tracing::info!(agent = %agent_id, "Agent spawned");
        Ok(handle)
    }

    /// Stop and re-spawn an agent by ID. The agent's session is restored from
    /// its latest checkpoint by the new actor's `on_start`.
    pub async fn restart_agent(&self, agent_id: &str) -> Result<(), DaemonError> {
        let id = AgentId::new(agent_id);

        let agent_yaml = self
            .config
            .agents
            .iter()
            .find(|a| a.id == agent_id)
            .ok_or_else(|| {
                DaemonError::AgentSpawn(format!("Agent '{agent_id}' is not in the config"))
            })?;

        // Stop the old actor and wait for full termination. ractor's name
        // registry holds the actor name until the actor genuinely stops; a new
        // spawn with the same name before then collides.
        if let Some(actor) = self.agent_registry.get(&id).await {
            let _ = actor
                .stop_and_wait(None, Some(Duration::from_secs(10)))
                .await;
        }
        self.agent_registry.remove(&id).await;

        // Re-spawn through the shared path.
        let handle = Self::spawn_agent(
            agent_yaml,
            &self.config,
            &self.provider_registry,
            &self.counter,
            &self.checkpoint_store,
            &self.tool_executor,
            &self.shared_registry,
            &self.agent_registry,
            &self.hook_registry,
            &self.stream_bus,
        )
        .await?;
        self.agent_handles.lock().unwrap().push(handle);

        // Re-register in the event lattice with the same threshold rules.
        let (threshold, decay_rate) = lattice_params(agent_yaml);
        self.event_lattice.register_agent(id, threshold, decay_rate);

        tracing::info!(agent = %agent_id, "Agent restarted");
        Ok(())
    }

    /// Configured agents whose actor is no longer running (crashed or stopped).
    /// The supervision loop restarts these from their last checkpoint.
    pub async fn dead_agents(&self) -> Vec<String> {
        let mut dead = Vec::new();
        for agent in &self.config.agents {
            // Workers are spawned on demand by their coordinator, never as
            // standalone supervised agents — so they're never "dead". (Treating
            // them as dead makes the supervisor restart them forever, colliding
            // with the coordinator's transient worker actors.)
            if matches!(agent.role, AgentRoleYaml::Worker) {
                continue;
            }
            if !self.agent_registry.is_alive(&AgentId::new(&agent.id)).await {
                dead.push(agent.id.clone());
            }
        }
        dead
    }

    fn setup_providers(
        config: &AxocoatlConfig,
        registry: &mut ProviderRegistry,
    ) -> Result<(), DaemonError> {
        // OpenAI — or any OpenAI-compatible server (LM Studio, MLX/oMLX, vLLM, …)
        // when `base_url` is set, in which case requests go there instead of
        // api.openai.com.
        if let Some(openai) = &config.providers.openai {
            if !openai.api_key.is_empty() {
                let provider = match &openai.base_url {
                    Some(base_url) => axocoatl_llm_openai::OpenAiProvider::with_base_url(
                        openai.api_key.expose_secret(),
                        "gpt-4o", // Default model — agents specify their own
                        base_url,
                    ),
                    None => axocoatl_llm_openai::OpenAiProvider::new(
                        openai.api_key.expose_secret(),
                        "gpt-4o", // Default model — agents specify their own
                    ),
                };
                registry.register(Arc::new(provider));
                match &openai.base_url {
                    Some(base_url) => {
                        tracing::info!(%base_url, "Registered OpenAI provider")
                    }
                    None => tracing::info!("Registered OpenAI provider"),
                }
            }
        }

        // OpenRouter — OpenAI-compatible API at openrouter.ai/api/v1.
        // Reuses the OpenAI provider, just points at a different base URL
        // and identifies as "openrouter" in the registry so agents can
        // pick it with `provider: openrouter`.
        if let Some(openrouter) = &config.providers.openrouter {
            if !openrouter.api_key.is_empty() {
                let provider = axocoatl_llm_openai::OpenAiProvider::with_base_url(
                    openrouter.api_key.expose_secret(),
                    "openai/gpt-4o-mini", // Default — agents pick their own
                    "https://openrouter.ai/api/v1",
                )
                .with_provider_id("openrouter");
                registry.register(Arc::new(provider));
                tracing::info!("Registered OpenRouter provider");
            }
        }

        // Anthropic
        if let Some(anthropic) = &config.providers.anthropic {
            if !anthropic.api_key.is_empty() {
                let provider = axocoatl_llm_anthropic::AnthropicProvider::new(
                    anthropic.api_key.expose_secret(),
                    "claude-sonnet-4-6",
                );
                registry.register(Arc::new(provider));
                tracing::info!("Registered Anthropic provider");
            }
        }

        // Gemini
        if let Some(gemini) = &config.providers.gemini {
            if !gemini.api_key.is_empty() {
                let provider = axocoatl_llm_gemini::GeminiProvider::new(
                    gemini.api_key.expose_secret(),
                    "gemini-2.5-flash",
                );
                registry.register(Arc::new(provider));
                tracing::info!("Registered Gemini provider");
            }
        }

        // Mistral
        if let Some(mistral) = &config.providers.mistral {
            if !mistral.api_key.is_empty() {
                let provider = axocoatl_llm_mistral::MistralProvider::new(
                    mistral.api_key.expose_secret(),
                    "mistral-large-latest",
                );
                registry.register(Arc::new(provider));
                tracing::info!("Registered Mistral provider");
            }
        }

        // Ollama: per-agent providers are created in the spawn loop (each agent
        // specifies its own model). We just validate the config is present here.
        if let Some(ollama) = &config.providers.ollama {
            tracing::info!(base_url = %ollama.base_url, "Ollama provider configured (per-agent models)");
        }

        Ok(())
    }

    /// Execute a task on a specific agent and return the full output.
    pub async fn execute_agent(
        &self,
        agent_id: &str,
        input: &str,
    ) -> Result<axocoatl_core::AgentOutput, DaemonError> {
        self.execute_agent_input(agent_id, axocoatl_core::AgentInput::text(input))
            .await
    }

    /// Execute an agent with a fully-built input, carrying any per-request
    /// `system_override` / `model_override`. Lets a caller run a prompt or model
    /// variant of the agent for a single execution without reconfiguring the
    /// daemon; the override fields are honored by the agent behavior (the same
    /// path the retained lightweight-chat API uses).
    pub async fn execute_agent_input(
        &self,
        agent_id: &str,
        input: axocoatl_core::AgentInput,
    ) -> Result<axocoatl_core::AgentOutput, DaemonError> {
        let id = AgentId::new(agent_id);
        let actor =
            self.agent_registry.get(&id).await.ok_or_else(|| {
                DaemonError::AgentSpawn(format!("Agent '{}' not found", agent_id))
            })?;

        let output = axocoatl_actor::execute_agent(&actor, input)
            .await
            .map_err(DaemonError::AgentSpawn)?;

        Ok(output)
    }

    // ── Directory sessions ──────────────────────────────────────────────

    /// Create a new directory session. `enabled_skills` is the allowlist of
    /// skill ids the session's agents may fire as tools.
    pub async fn create_session(
        &self,
        name: &str,
        working_dir: &str,
        mode: SessionMode,
        enabled_skills: Vec<String>,
        exposed_ports: Vec<u16>,
        image: Option<String>,
    ) -> Result<Session, DaemonError> {
        self.session_store
            .lock()
            .await
            .create(
                name,
                working_dir,
                mode,
                enabled_skills,
                exposed_ports,
                image,
            )
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    /// All known sessions, newest first.
    pub async fn list_sessions(&self) -> Vec<Session> {
        self.session_store.lock().await.list()
    }

    /// Fetch one session by id.
    pub async fn get_session(&self, id: &str) -> Option<Session> {
        self.session_store.lock().await.get(id)
    }

    /// Close a session: stop its sandbox container and mark it closed.
    pub async fn close_session(&self, id: &str) -> Result<(), DaemonError> {
        let current_set = self.peek_current_attempt_set(id).await?.map(|set| set.id);
        let (_operation, _cancellation_requested) = self
            .lock_attempt_operation_for_cleanup(id, current_set.as_deref())
            .await?;
        let result = async {
            // Close is resumable, not a destructive decision. Quiesce actors
            // and containers but preserve the attempt clones, verdicts, and
            // current pointer so reopening can continue with Review/Discard.
            self.quiesce_attempt_locked(id).await?;
            self.stop_session_actors_checked(id).await?;
            self.stop_session_sandbox_checked(id).await?;
            self.session_store
                .lock()
                .await
                .close(id)
                .map_err(|e| DaemonError::Session(e.to_string()))
        }
        .await;
        if let Some(set_id) = current_set {
            self.clear_attempt_cancellation(id, &set_id).await;
        }
        result
    }

    /// Delete a session entirely — stop and remove its sandbox, then drop the
    /// JSON from disk. Memory tiers under `{data_dir}/memory/{session_id}` are
    /// left in place; a user that creates a new session pointing at the same
    /// directory gets a fresh memory slate (different session id).
    pub async fn delete_session(&self, id: &str) -> Result<(), DaemonError> {
        let current_set = self.peek_current_attempt_set(id).await?.map(|set| set.id);
        let (_operation, _cancellation_requested) = self
            .lock_attempt_operation_for_cleanup(id, current_set.as_deref())
            .await?;
        let result = async {
            self.remove_variant_worktrees_locked(id).await?;
            self.stop_session_actors_checked(id).await?;
            self.stop_session_sandbox_checked(id).await?;
            self.session_store
                .lock()
                .await
                .remove(id)
                .map_err(|e| DaemonError::Session(e.to_string()))
        }
        .await;
        if let Some(set_id) = current_set {
            self.clear_attempt_cancellation(id, &set_id).await;
        }
        result
    }

    async fn stop_session_sandbox_checked(&self, id: &str) -> Result<(), DaemonError> {
        let sandbox = self.session_sandboxes.lock().await.get(id).cloned();
        if self.config.sandbox.backend == "e2b" {
            if let Some(sandbox) = sandbox {
                // The remote trait currently exposes best-effort stop only and
                // needs its live remote handle.
                sandbox.stop().await;
            }
        } else {
            // Container names are deterministic. After a daemon restart the
            // process-local map is empty but the preserved Podman container may
            // still be running, so exact teardown must not depend on a handle.
            SessionSandbox::remove_named(id)
                .await
                .map_err(|error| DaemonError::Session(error.to_string()))?;
        }
        self.session_sandboxes.lock().await.remove(id);
        Ok(())
    }

    async fn stop_session_actors_checked(&self, session_id: &str) -> Result<(), DaemonError> {
        let prefix = format!("{session_id}:");
        let actor_ids: Vec<AgentId> = self
            .agent_registry
            .list_ids()
            .await
            .into_iter()
            .filter(|id| id.to_string().starts_with(&prefix))
            .collect();
        let mut shutdowns = tokio::task::JoinSet::new();
        for actor_id in &actor_ids {
            let Some(actor) = self.agent_registry.get(actor_id).await else {
                continue;
            };
            let label = actor_id.to_string();
            shutdowns.spawn(async move {
                if matches!(actor.get_status(), ractor::ActorStatus::Stopped) {
                    return (label, Ok(()));
                }
                let graceful = actor
                    .stop_and_wait(None, Some(Duration::from_secs(10)))
                    .await;
                if graceful.is_ok() || matches!(actor.get_status(), ractor::ActorStatus::Stopped) {
                    return (label, Ok(()));
                }
                let forced = actor.kill_and_wait(Some(Duration::from_secs(5))).await;
                if forced.is_err() && matches!(actor.get_status(), ractor::ActorStatus::Stopped) {
                    (label, Ok(()))
                } else {
                    (label, forced.map_err(|error| error.to_string()))
                }
            });
        }
        let mut failures = Vec::new();
        while let Some(result) = shutdowns.join_next().await {
            match result {
                Ok((_, Ok(()))) => {}
                Ok((actor, Err(error))) => {
                    failures.push(format!("session actor '{actor}' did not stop: {error}"));
                }
                Err(error) => failures.push(format!("session actor shutdown failed: {error}")),
            }
        }
        for actor_id in actor_ids {
            self.agent_registry.remove(&actor_id).await;
        }
        if let Ok(mut runs) = self.active_runs.lock() {
            runs.remove(session_id);
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(DaemonError::Session(failures.join("; ")))
        }
    }

    /// Set the session's check command. `None` or empty clears it.
    pub async fn set_session_check(
        &self,
        id: &str,
        cmd: Option<String>,
    ) -> Result<Session, DaemonError> {
        self.session_store
            .lock()
            .await
            .set_check_command(id, cmd)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    pub async fn rename_session(&self, id: &str, new_name: &str) -> Result<Session, DaemonError> {
        self.session_store
            .lock()
            .await
            .rename(id, new_name)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    // ── Chats ───────────────────────────────────────────────────────────
    // Thin wrappers around ChatStore. ChatStore does the work — these just
    // mediate Arc<Mutex<…>> access and surface DaemonError for the API.

    pub async fn create_chat(
        &self,
        agent_id: &str,
        name: &str,
    ) -> Result<axocoatl_memory::chat::Chat, DaemonError> {
        // Reject unknown agents up-front rather than letting a "ghost" chat
        // exist that the executor will refuse to run.
        if self.config.agents.iter().all(|a| a.id != agent_id) {
            return Err(DaemonError::AgentSpawn(format!(
                "agent '{agent_id}' not found"
            )));
        }
        self.chat_store
            .lock()
            .await
            .create(agent_id, name)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    pub async fn list_chats(&self) -> Vec<axocoatl_memory::chat::Chat> {
        self.chat_store.lock().await.list()
    }

    pub async fn get_chat(&self, id: &str) -> Option<axocoatl_memory::chat::Chat> {
        self.chat_store.lock().await.get(id)
    }

    pub async fn rename_chat(
        &self,
        id: &str,
        new_name: &str,
    ) -> Result<axocoatl_memory::chat::Chat, DaemonError> {
        self.chat_store
            .lock()
            .await
            .rename(id, new_name)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    pub async fn star_chat(
        &self,
        id: &str,
        starred: bool,
    ) -> Result<axocoatl_memory::chat::Chat, DaemonError> {
        self.chat_store
            .lock()
            .await
            .star(id, starred)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    pub async fn set_chat_overrides(
        &self,
        id: &str,
        system_override: Option<String>,
        model_override: Option<String>,
    ) -> Result<axocoatl_memory::chat::Chat, DaemonError> {
        self.chat_store
            .lock()
            .await
            .set_overrides(id, system_override, model_override)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    pub async fn delete_chat(&self, id: &str) -> Result<(), DaemonError> {
        self.chat_store
            .lock()
            .await
            .remove(id)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    pub async fn fork_chat(
        &self,
        parent_id: &str,
        truncate_at: usize,
        replacement: Option<axocoatl_memory::session::StoredMessage>,
    ) -> Result<axocoatl_memory::chat::Chat, DaemonError> {
        self.chat_store
            .lock()
            .await
            .fork(parent_id, truncate_at, replacement)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    pub async fn search_chats(&self, query: &str) -> Vec<axocoatl_memory::chat::Chat> {
        self.chat_store.lock().await.search(query)
    }

    /// Background tasks running in a session's sandbox container, serialized
    /// for the API. Empty if the session has no live sandbox. PTY terminals
    /// are merged in as `kind: "terminal"` entries so the dashboard's unified
    /// Terminals list can render them alongside log-based tasks.
    pub async fn session_tasks(&self, session_id: &str) -> serde_json::Value {
        let (tasks, terms) = {
            let boxes = self.session_sandboxes.lock().await;
            match boxes.get(session_id) {
                Some(sb) => (sb.list_tasks(), sb.list_terminals()),
                None => (Vec::new(), Vec::new()),
            }
        };
        let mut out: Vec<serde_json::Value> = tasks
            .into_iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "kind": "task",
                    "command": t.command,
                    "status": t.status,
                    "log": t.log,
                })
            })
            .collect();
        for (id, command, alive) in terms {
            out.push(serde_json::json!({
                "id": id,
                "kind": "terminal",
                "command": command,
                "status": if alive { "running" } else { "exited" },
                "log": "",
            }));
        }
        serde_json::Value::Array(out)
    }

    /// Snapshot of every automation in the store. Cheap read — backed by
    /// an in-memory hashmap that's only touched on writes.
    pub async fn list_automations(&self) -> Vec<axocoatl_config::Automation> {
        self.automation_store.read().await.list()
    }

    /// Look up one automation by id.
    pub async fn get_automation(&self, id: &str) -> Option<axocoatl_config::Automation> {
        self.automation_store.read().await.get(id)
    }

    /// Create a new automation. Errors if the id already exists.
    pub async fn create_automation(
        &self,
        a: axocoatl_config::Automation,
    ) -> Result<axocoatl_config::Automation, DaemonError> {
        self.automation_store
            .write()
            .await
            .create(a)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    /// Replace an existing automation (or insert if missing).
    pub async fn upsert_automation(
        &self,
        a: axocoatl_config::Automation,
    ) -> Result<axocoatl_config::Automation, DaemonError> {
        self.automation_store
            .write()
            .await
            .upsert(a)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    // ── MCP runtime management ──────────────────────────────────────
    // The catalog Install button + the Connected panel call into these.

    /// Connect a new MCP server at runtime. Returns the tool count exposed
    /// by the server on success.
    pub async fn connect_mcp_server(
        &self,
        name: &str,
        transport: axocoatl_mcp::McpTransportType,
    ) -> Result<usize, DaemonError> {
        let mut reg = self.mcp_registry.write().await;
        reg.connect_server(name, transport)
            .await
            .map_err(|e| DaemonError::Session(e.to_string()))?;
        Ok(reg
            .servers()
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.tool_count)
            .unwrap_or(0))
    }

    /// Re-dial an already-installed MCP server using its cached transport.
    /// Returns the (possibly new) tool count.
    pub async fn reconnect_mcp_server(&self, name: &str) -> Result<usize, DaemonError> {
        let mut reg = self.mcp_registry.write().await;
        reg.reconnect_server(name)
            .await
            .map_err(|e| DaemonError::Session(e.to_string()))?;
        Ok(reg
            .servers()
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.tool_count)
            .unwrap_or(0))
    }

    /// Remove an MCP server and its tools from the registry.
    pub async fn remove_mcp_server(&self, name: &str) -> Result<bool, DaemonError> {
        let mut reg = self.mcp_registry.write().await;
        Ok(reg.remove_server(name))
    }

    /// Delete an automation. Returns NotFound if it doesn't exist.
    pub async fn delete_automation(&self, id: &str) -> Result<(), DaemonError> {
        self.automation_store
            .write()
            .await
            .delete(id)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    // ── Automation folders ──
    pub async fn list_automation_folders(&self) -> Vec<axocoatl_config::AutomationFolder> {
        self.automation_store.read().await.list_folders()
    }
    pub async fn create_automation_folder(
        &self,
        path: &str,
        name: Option<String>,
    ) -> Result<axocoatl_config::AutomationFolder, DaemonError> {
        self.automation_store
            .write()
            .await
            .create_folder(path, name)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }
    pub async fn rename_automation_folder(
        &self,
        old_path: &str,
        new_path: &str,
        new_name: Option<String>,
    ) -> Result<axocoatl_config::AutomationFolder, DaemonError> {
        self.automation_store
            .write()
            .await
            .rename_folder(old_path, new_path, new_name)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }
    pub async fn delete_automation_folder(
        &self,
        path: &str,
        keep_contents: bool,
    ) -> Result<usize, DaemonError> {
        self.automation_store
            .write()
            .await
            .delete_folder(path, keep_contents)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }
    /// Move a single automation into a folder (or to root when `folder = None`).
    pub async fn set_automation_folder(
        &self,
        id: &str,
        folder: Option<String>,
    ) -> Result<axocoatl_config::Automation, DaemonError> {
        let mut store = self.automation_store.write().await;
        let mut auto = store
            .get(id)
            .ok_or_else(|| DaemonError::Session(format!("automation {id} not found")))?;
        // If the target folder doesn't exist as an explicit entity, create
        // it (and its ancestors) so the UI's "move into a new folder" flow
        // doesn't need a separate "create folder first" call.
        if let Some(f) = &folder {
            if !f.is_empty() {
                store
                    .create_folder(f, None)
                    .map_err(|e| DaemonError::Session(e.to_string()))?;
            }
        }
        auto.folder = folder;
        store
            .upsert(auto)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    /// Run an automation by id. The single execution path that schedules,
    /// proactives, and user-fired workflows all converge through.
    pub async fn execute_automation(
        &self,
        id: &str,
        input: &str,
    ) -> Result<crate::workflow::WorkflowOutput, DaemonError> {
        self.execute_automation_with_inputs(id, input, &std::collections::HashMap::new())
            .await
    }

    /// Run an automation with explicit per-`TextInput` values.  The map
    /// keys are node ids; missing entries fall back to each node's saved
    /// `default_value`.  Used by the dashboard's run-input form.
    pub async fn execute_automation_with_inputs(
        &self,
        id: &str,
        input: &str,
        text_inputs: &std::collections::HashMap<String, String>,
    ) -> Result<crate::workflow::WorkflowOutput, DaemonError> {
        let automation = self
            .get_automation(id)
            .await
            .ok_or_else(|| DaemonError::WorkflowNotFound(format!("automation '{id}'")))?;
        crate::automation_executor::execute_automation_with_inputs(
            self,
            &automation,
            input,
            text_inputs,
        )
        .await
    }

    /// List the run history for an automation.
    pub async fn list_runs(
        &self,
        automation_id: &str,
    ) -> Result<Vec<crate::automation_runs::Run>, DaemonError> {
        self.run_store
            .list(automation_id)
            .await
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    /// Load one run by id.
    pub fn get_run(
        &self,
        automation_id: &str,
        run_id: &str,
    ) -> Result<crate::automation_runs::Run, DaemonError> {
        self.run_store
            .load(automation_id, run_id)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    /// Start a user-supplied command as a background task in the session's
    /// sandbox container. Boots the sandbox if it isn't running yet.
    pub async fn spawn_session_task(
        &self,
        session_id: &str,
        command: &str,
    ) -> Result<String, DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("unknown session {session_id}")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        Ok(sandbox.spawn_background(command))
    }

    /// Start an interactive PTY-backed terminal in the session's sandbox.
    /// Returns the terminal id; the WebSocket route handles live IO.
    pub async fn spawn_session_terminal(
        &self,
        session_id: &str,
        command: &str,
        rows: u16,
        cols: u16,
    ) -> Result<String, DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("unknown session {session_id}")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let term = sandbox
            .spawn_pty(command, rows, cols)
            .map_err(DaemonError::Session)?;
        Ok(term.id.clone())
    }

    /// Look up a live PTY terminal by id (for the WS bridge).
    pub async fn session_terminal(
        &self,
        session_id: &str,
        terminal_id: &str,
    ) -> Option<std::sync::Arc<axocoatl_isolation::pty::PtyTerminal>> {
        let boxes = self.session_sandboxes.lock().await;
        boxes
            .get(session_id)
            .and_then(|sb| sb.get_terminal(terminal_id))
    }

    /// Snapshot of interactive terminals in a session.
    pub async fn list_session_terminals(&self, session_id: &str) -> Vec<(String, String, bool)> {
        let boxes = self.session_sandboxes.lock().await;
        boxes
            .get(session_id)
            .map(|sb| sb.list_terminals())
            .unwrap_or_default()
    }

    /// Ensure the session's isolation instance is running, returning it. The
    /// backend (local Podman container or remote E2B-compatible microVM) is
    /// chosen by `sandbox.backend` in config — a per-project/session choice.
    async fn ensure_sandbox(&self, session: &Session) -> Result<Arc<dyn Sandbox>, DaemonError> {
        // Fast path: already started. Take and drop the lock immediately.
        if let Some(sb) = self.session_sandboxes.lock().await.get(&session.id) {
            return Ok(sb.clone());
        }
        let start = {
            let mut starts = self.sandbox_starts.lock().await;
            starts
                .entry(session.id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _start = start.lock().await;
        // Another caller may have completed while this one waited.
        if let Some(sb) = self.session_sandboxes.lock().await.get(&session.id) {
            return Ok(sb.clone());
        }
        // Start the instance WITHOUT holding the global map lock — a remote boot
        // + git clone can take minutes, and holding the lock would serialize every
        // other session's file/git/exec op behind it (head-of-line blocking).
        let sc = &self.config.sandbox;
        let sandbox: Arc<dyn Sandbox> = match sc.backend.as_str() {
            "e2b" => self.start_e2b_sandbox(session).await?,
            "podman" | "" => {
                let policy = axocoatl_isolation::session_sandbox::SandboxPolicy {
                    allow_post_create: sc.allow_post_create_command,
                    allow_untrusted_image: sc.allow_untrusted_images,
                    network: match sc.network.as_str() {
                        "none" => axocoatl_isolation::session_sandbox::SandboxNetwork::None,
                        _ => axocoatl_isolation::session_sandbox::SandboxNetwork::Bridge,
                    },
                    require_resource_limits: sc.require_resource_limits,
                };
                let sandbox = SessionSandbox::start(
                    &session.id,
                    &session.working_dir,
                    session.image.as_deref(),
                    &session.exposed_ports,
                    &session.post_create_commands,
                    &policy,
                )
                .await
                .map_err(|e| DaemonError::Session(format!("starting session sandbox: {e}")))?;
                Arc::new(sandbox)
            }
            other => {
                return Err(DaemonError::Session(format!(
                    "unknown isolation backend '{other}' (expected 'podman' or 'e2b')"
                )));
            }
        };
        self.session_sandboxes
            .lock()
            .await
            .insert(session.id.clone(), sandbox.clone());
        Ok(sandbox)
    }

    /// Recreate the primary local sandbox for an unresolved attempt without
    /// running project setup again. After a daemon crash the workspace may be
    /// between Keep journal entries; postCreate formatters/installers must not
    /// get a chance to turn that classifiable pre/post state into a third one.
    async fn ensure_attempt_recovery_sandbox(
        &self,
        session: &Session,
    ) -> Result<Arc<dyn Sandbox>, DaemonError> {
        if let Some(sandbox) = self.session_sandboxes.lock().await.get(&session.id) {
            return Ok(sandbox.clone());
        }
        Self::require_attempt_resolution_backend(&self.config.sandbox.backend)?;
        let start = {
            let mut starts = self.sandbox_starts.lock().await;
            starts
                .entry(session.id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _start = start.lock().await;
        if let Some(sandbox) = self.session_sandboxes.lock().await.get(&session.id) {
            return Ok(sandbox.clone());
        }
        let config = &self.config.sandbox;
        let policy = axocoatl_isolation::session_sandbox::SandboxPolicy {
            allow_post_create: false,
            allow_untrusted_image: config.allow_untrusted_images,
            network: match config.network.as_str() {
                "none" => axocoatl_isolation::session_sandbox::SandboxNetwork::None,
                _ => axocoatl_isolation::session_sandbox::SandboxNetwork::Bridge,
            },
            require_resource_limits: config.require_resource_limits,
        };
        let sandbox: Arc<dyn Sandbox> = Arc::new(
            SessionSandbox::start(
                &session.id,
                &session.working_dir,
                session.image.as_deref(),
                &session.exposed_ports,
                &[],
                &policy,
            )
            .await
            .map_err(|error| {
                DaemonError::Session(format!(
                    "starting attempt recovery sandbox without postCreate: {error}"
                ))
            })?,
        );
        self.session_sandboxes
            .lock()
            .await
            .insert(session.id.clone(), sandbox.clone());
        Ok(sandbox)
    }

    /// Start a remote E2B-compatible microVM as a session's isolation instance
    /// (E2B cloud or a self-hosted CubeSandbox, per the `sandbox.e2b` config).
    ///
    /// A git-repo session is reproduced **git-natively**: the VM clones the repo
    /// (from a clean, committed branch) over https, authenticated by an in-VM
    /// credential helper that reads a token injected as a sandbox secret — the
    /// token never touches the repo's git config or any command line. A
    /// scratch/code-execution session (no repo) just gets a fresh workspace.
    async fn start_e2b_sandbox(&self, session: &Session) -> Result<Arc<dyn Sandbox>, DaemonError> {
        use axocoatl_isolation::e2b::{E2bConfig, E2bSandbox};
        let e = self.config.sandbox.e2b.as_ref().ok_or_else(|| {
            DaemonError::Session(
                "sandbox.backend is 'e2b' but no `sandbox.e2b` block is configured".to_string(),
            )
        })?;
        if e.api_key.is_empty() {
            return Err(DaemonError::Session(
                "the e2b backend needs an api_key (set `sandbox.e2b.api_key`, e.g. `${E2B_API_KEY}`)"
                    .to_string(),
            ));
        }
        let cfg = E2bConfig {
            api_url: e.api_url.clone(),
            api_key: e.api_key.expose_secret().to_string(),
            template: e.template.clone(),
            domain: e.domain.clone(),
        };

        // Sandbox env: no interactive git prompts, plus the git token (if set) —
        // set at create time so it persists into the agent's own later commands.
        let mut env = std::collections::BTreeMap::new();
        env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
        if !e.git_token.is_empty() {
            env.insert(
                "AXO_GIT_TOKEN".to_string(),
                e.git_token.expose_secret().to_string(),
            );
        }

        // A non-git (scratch) session: a fresh workspace, nothing to clone.
        if !session.working_dir.join(".git").exists() {
            let root = "/home/user";
            let sandbox = E2bSandbox::start(cfg, E2B_SESSION_TIMEOUT_SECS, root, &env)
                .await
                .map_err(|err| DaemonError::Session(format!("starting E2B sandbox: {err}")))?;
            let _ = sandbox
                .exec(&["mkdir", "-p", root], Duration::from_secs(15))
                .await;
            return Ok(Arc::new(sandbox));
        }

        // A git-repo session: derive a clean https clone spec from the HOST repo,
        // then reproduce it inside the VM.
        let spec = crate::git_host::remote_repo_spec(&session.working_dir)
            .await
            .map_err(DaemonError::Session)?;
        let dest = format!("/home/user/{}", spec.name);

        // Boot rooted at /home/user (a dir that exists) so setup execs have a
        // valid cwd; re-root at the clone afterwards.
        let base = E2bSandbox::start(cfg, E2B_SESSION_TIMEOUT_SECS, "/home/user", &env)
            .await
            .map_err(|err| DaemonError::Session(format!("starting E2B sandbox: {err}")))?;

        // Provision the repo. On ANY failure — transport error, non-zero exit,
        // timeout — stop the VM before returning so a half-set-up remote sandbox
        // is never leaked (it would otherwise run until its TTL).
        match self.provision_e2b_git(&base, &spec, &dest).await {
            Ok(()) => Ok(base.with_root(std::path::Path::new(&dest))),
            Err(e) => {
                base.stop().await;
                Err(e)
            }
        }
    }

    /// Configure git auth and clone the repo inside a freshly-started E2B sandbox.
    /// Every path returns `Err` on failure without touching the VM lifecycle — the
    /// caller stops the VM — so no error path can leak it.
    async fn provision_e2b_git(
        &self,
        sandbox: &dyn Sandbox,
        spec: &crate::git_host::RemoteRepoSpec,
        dest: &str,
    ) -> Result<(), DaemonError> {
        // Configure git in the VM: a github.com-scoped credential helper that
        // reads $AXO_GIT_TOKEN at fill-time (the config stores only the literal
        // string, never the token), plus an agent commit identity. Single quotes
        // keep the shell from expanding $AXO_GIT_TOKEN now.
        let setup = "git config --global 'credential.https://github.com.helper' \
             '!f() { echo username=x-access-token; echo password=$AXO_GIT_TOKEN; }; f' && \
             git config --global 'credential.https://github.com.useHttpPath' false && \
             git config --global user.email 'agent@axocoatl.local' && \
             git config --global user.name 'Axocoatl Agent'";
        let r = sandbox
            .exec(&["sh", "-c", setup], Duration::from_secs(30))
            .await
            .map_err(|err| DaemonError::Session(format!("configuring git in sandbox: {err}")))?;
        if r.exit_code != 0 {
            return Err(DaemonError::Session(format!(
                "configuring git in sandbox failed: {}",
                r.stderr.trim()
            )));
        }

        // Clone the clean https URL on its branch; the helper supplies the token.
        // Direct argv (no shell): branch/url/dest are data, never parsed by a
        // shell, so a branch name with shell metacharacters can't inject.
        let r = sandbox
            .exec(
                &[
                    "git",
                    "clone",
                    "--branch",
                    spec.branch.as_str(),
                    "--single-branch",
                    spec.https_url.as_str(),
                    dest,
                ],
                Duration::from_secs(600),
            )
            .await
            .map_err(|err| DaemonError::Session(format!("cloning repo into sandbox: {err}")))?;
        if r.exit_code != 0 {
            return Err(DaemonError::Session(format!(
                "cloning {} into the E2B sandbox failed: {}",
                spec.https_url,
                r.stderr.trim()
            )));
        }
        Ok(())
    }

    /// Execute an instruction inside a session.
    pub async fn execute_session(
        &self,
        session_id: &str,
        input: &str,
    ) -> Result<axocoatl_core::AgentOutput, DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        self.require_no_unresolved_attempt(session_id).await?;
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;

        let actor = match &session.mode {
            SessionMode::SingleAgent { agent_id } => self.session_actor(&session, agent_id).await?,
            SessionMode::Lattice { .. } | SessionMode::Custom { .. } => {
                return Err(DaemonError::Session(
                    "multi-agent sessions require the streaming API — call \
                     execute_session_streaming instead"
                        .to_string(),
                ));
            }
        };

        // Routed through the streaming runner even though this caller does not
        // stream. A session turn must mean the same thing however it was
        // started: run it two different ways and "what did the agent just
        // change" is recorded for one and silently absent for the other, which
        // makes the answer depend on which button was pressed.
        let output = self
            .run_session_agent_streamed(&actor, session_id, "session", input, None)
            .await?;

        let _ = self.session_store.lock().await.touch(session_id);
        Ok(output)
    }

    /// The persisted conversation transcript for a session — read from the
    /// session agent's latest checkpoint (keyed by the scoped `{session}:{agent}`
    /// id). Lets the cockpit rehydrate prior turns when a session is reopened,
    /// and makes the transcript addressable for rewind. Empty when the session
    /// has never run a turn, or for multi-agent sessions (no single transcript).
    pub async fn session_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<axocoatl_memory::session::StoredMessage>, DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let agent_id = match &session.mode {
            SessionMode::SingleAgent { agent_id } => agent_id.clone(),
            _ => return Ok(Vec::new()),
        };
        let scoped = AgentId::new(format!("{}:{}", session.id, agent_id));
        let ckpt = self
            .checkpoint_store
            .load_latest(&scoped)
            .await
            .map_err(|e| DaemonError::Session(e.to_string()))?;
        Ok(ckpt.map(|c| c.session_messages).unwrap_or_default())
    }

    /// Rewind a session's conversation to keep only the first `keep` transcript
    /// messages, dropping everything after. Persists a new checkpoint and drops
    /// the live actor so the next turn re-spawns from the truncated state. The
    /// caller computes `keep` from the transcript returned by `session_messages`
    /// (a count of raw `StoredMessage`s), landing on a turn boundary.
    pub async fn rewind_session(&self, session_id: &str, keep: usize) -> Result<(), DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        self.require_no_unresolved_attempt(session_id).await?;
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let agent_id = match &session.mode {
            SessionMode::SingleAgent { agent_id } => agent_id.clone(),
            _ => {
                return Err(DaemonError::Session(
                    "rewind is only supported for single-agent sessions".to_string(),
                ))
            }
        };
        let scoped = AgentId::new(format!("{}:{}", session.id, agent_id));
        let mut ckpt = self
            .checkpoint_store
            .load_latest(&scoped)
            .await
            .map_err(|e| DaemonError::Session(e.to_string()))?
            .ok_or_else(|| DaemonError::Session("no checkpoint to rewind".to_string()))?;

        if keep >= ckpt.session_messages.len() {
            return Ok(()); // nothing to drop
        }
        ckpt.session_messages.truncate(keep);
        ckpt.version += 1;
        ckpt.checkpoint_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.checkpoint_store
            .save(&ckpt)
            .await
            .map_err(|e| DaemonError::Session(e.to_string()))?;

        // Drop the live actor (if any) so the next `execute_session` re-spawns
        // it and restores from the truncated checkpoint. The scoped id isn't in
        // `config.agents`, so `restart_agent` can't be reused here.
        if let Some(actor) = self.agent_registry.get(&scoped).await {
            let _ = actor
                .stop_and_wait(None, Some(Duration::from_secs(10)))
                .await;
        }
        self.agent_registry.remove(&scoped).await;
        let _ = self.session_store.lock().await.touch(session_id);
        Ok(())
    }

    // ── Git: a session is (optionally auto-) a git repo ─────────────────
    // git runs INSIDE the session sandbox container; the working dir is bind-
    // mounted there, so it operates on the real folder. `safe.directory=*`
    // sidesteps podman's "dubious ownership" guard on the mounted tree.

    /// Run `git -C {working_dir} <args>` inside the session's sandbox.
    pub async fn session_git(
        &self,
        session_id: &str,
        args: &[&str],
    ) -> Result<ExecResult, DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        // Root git at the sandbox's working dir, not the host path: for Podman
        // they're identical (bind mount); for a remote clone it's the in-VM path.
        let dir = self
            .ensure_sandbox(&session)
            .await?
            .root()
            .to_string_lossy()
            .to_string();
        self.session_git_at(session_id, &dir, args).await
    }

    /// Run `git -C {cwd} <args>` inside the session's sandbox, where `cwd` is
    /// any path under the session mount (the session root, or a variant
    /// worktree). All git for a session goes through here.
    pub async fn session_git_at(
        &self,
        session_id: &str,
        cwd: &str,
        args: &[&str],
    ) -> Result<ExecResult, DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let mut argv: Vec<&str> = vec![
            "git",
            "-c",
            "safe.directory=*",
            // The sandbox has no git identity and does not inherit the host's,
            // so any commit would fail with "Author identity unknown" — which
            // silently broke adopting a variant. Passing it per command rather
            // than writing it into the user's repo config keeps this
            // side-effect-free, and a repo that has its own identity is
            // unaffected because committed metadata still comes from the repo.
            "-c",
            "user.email=agent@axocoatl.local",
            "-c",
            "user.name=Axocoatl",
            "-C",
            cwd,
        ];
        argv.extend_from_slice(args);
        sandbox
            .exec(&argv, Duration::from_secs(60))
            .await
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    /// Run git with an alternate index, leaving the user's real index and
    /// working tree untouched. This is the basis of an attempt snapshot: a
    /// hidden commit can describe staged, unstaged, and untracked work without
    /// staging or committing any of it in the session checkout.
    async fn session_git_with_index(
        &self,
        session_id: &str,
        cwd: &str,
        index_path: &str,
        args: &[&str],
    ) -> Result<ExecResult, DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let mut argv = vec![
            "env".to_string(),
            format!("GIT_INDEX_FILE={index_path}"),
            "git".to_string(),
            "-c".to_string(),
            "safe.directory=*".to_string(),
            "-c".to_string(),
            "user.email=agent@axocoatl.local".to_string(),
            "-c".to_string(),
            "user.name=Axocoatl".to_string(),
            "-c".to_string(),
            "core.filemode=true".to_string(),
            "-c".to_string(),
            "core.symlinks=true".to_string(),
            "-C".to_string(),
            cwd.to_string(),
        ];
        argv.extend(args.iter().map(|arg| (*arg).to_string()));
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        sandbox
            .exec(&refs, Duration::from_secs(300))
            .await
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    /// Materialize selected paths from an alternate index into a separate
    /// worktree. Both locations are daemon-derived; the user's real index and
    /// primary files are not consulted or mutated by this command.
    async fn session_git_stdin_with_index_work_tree(
        &self,
        session_id: &str,
        git_dir: &str,
        index_path: &str,
        work_tree: &str,
        args: &[&str],
        stdin: &str,
    ) -> Result<ExecResult, DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let mut argv = vec![
            "env".to_string(),
            format!("GIT_DIR={git_dir}"),
            format!("GIT_INDEX_FILE={index_path}"),
            format!("GIT_WORK_TREE={work_tree}"),
            "git".to_string(),
            "-c".to_string(),
            "safe.directory=*".to_string(),
            "-c".to_string(),
            "core.filemode=true".to_string(),
            "-c".to_string(),
            "core.symlinks=true".to_string(),
        ];
        argv.extend(args.iter().map(|arg| (*arg).to_string()));
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        sandbox
            .exec_stdin(&refs, stdin, Duration::from_secs(300))
            .await
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    /// Ensure the session directory is a git repo — initialize it with a
    /// baseline commit if it isn't. Idempotent; a no-op for existing repos.
    pub async fn ensure_session_git(&self, session_id: &str) -> Result<(), DaemonError> {
        let probe = self
            .session_git(session_id, &["rev-parse", "--is-inside-work-tree"])
            .await?;
        if probe.ok() && probe.stdout.trim() == "true" {
            return Ok(());
        }
        // Not a repo — initialize with a local identity + a baseline commit so
        // HEAD always exists (diffs need a reference point).
        self.session_git(session_id, &["init", "-q"]).await?;
        self.session_git(
            session_id,
            &["config", "user.email", "agent@axocoatl.local"],
        )
        .await?;
        self.session_git(session_id, &["config", "user.name", "Axocoatl"])
            .await?;
        self.session_git(session_id, &["add", "-A"]).await?;
        self.session_git(
            session_id,
            &["commit", "-q", "-m", "axocoatl: baseline", "--allow-empty"],
        )
        .await?;
        Ok(())
    }

    /// Working-tree status (current branch + changed files).
    pub async fn git_status(&self, session_id: &str) -> Result<crate::git::GitStatus, DaemonError> {
        self.ensure_session_git(session_id).await?;
        let r = self
            .session_git(
                session_id,
                &["status", "--porcelain=v1", "-b", "--untracked-files=all"],
            )
            .await?;
        // A failed status must not read as a clean tree. `session_git_at`
        // returns Ok for any exit code, so a git that could not run at all —
        // sandbox gone, directory not a repo — produced empty stdout, and
        // `parse_status("")` yields `clean: true`. That is the worst possible
        // wrong answer here: a reviewer concludes the agent changed nothing.
        if !r.ok() {
            let why = r.stderr.trim();
            return Err(DaemonError::Session(format!(
                "cannot read the working tree{}",
                if why.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", why.lines().next().unwrap_or(why))
                }
            )));
        }
        let mut status = crate::git::parse_status(&r.stdout);
        // Size the changes. `--numstat` covers tracked files; untracked ones are
        // absent from it, so `HEAD --` picks up staged and unstaged together and
        // anything still uncounted keeps `None` rather than a misleading zero.
        if let Ok(n) = self
            .session_git(session_id, &["diff", "--numstat", "HEAD", "--"])
            .await
        {
            crate::git::apply_numstat(&mut status, &n.stdout);
        }
        crate::git::mark_last_turn(&mut status, &self.session_last_turn_files(session_id));
        Ok(status)
    }

    /// Before/after content for one file (for the diff editor). `old` is the
    /// HEAD version (empty for a new file); `new` is the working-tree content.
    /// Both are read from inside the container so they match exactly what
    /// `git status`/`checkout` see (the host↔container bind mount is only
    /// eventually-consistent on macOS podman).
    pub async fn git_diff(
        &self,
        session_id: &str,
        path: &str,
    ) -> Result<crate::git::GitDiff, DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        // In-sandbox path: == working_dir for Podman, the in-VM clone for E2B.
        let dir = self
            .ensure_sandbox(&session)
            .await?
            .root()
            .to_string_lossy()
            .to_string();
        self.git_diff_at(session_id, &dir, "HEAD", path).await
    }

    /// Before/after for one file **inside a variant lane's worktree**.
    ///
    /// Each lane is its own checkout, so the same path legitimately differs from
    /// lane to lane — that difference *is* the thing being compared when you pick
    /// a winner.
    pub async fn variant_diff(
        &self,
        session_id: &str,
        set_id: &str,
        index: usize,
        path: &str,
    ) -> Result<crate::git::GitDiff, DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        let set = self.require_attempt_set(session_id, set_id).await?;
        Self::require_review_storage(&set)?;
        let lane = set
            .lanes
            .iter()
            .find(|lane| lane.index == index)
            .ok_or_else(|| DaemonError::Session(format!("attempt {} does not exist", index + 1)))?;
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let lane_sandbox = self.attempt_sandbox(&session, set_id, lane.index).await?;
        let worktree = lane_sandbox.root().to_string_lossy().to_string();
        Self::git_diff_in_sandbox(&lane_sandbox, &worktree, &set.base_sha, path).await
    }

    /// The diff machinery, rooted at any checkout inside the sandbox — the
    /// session's primary tree or one variant lane's worktree.
    async fn git_diff_at(
        &self,
        session_id: &str,
        dir: &str,
        reference: &str,
        path: &str,
    ) -> Result<crate::git::GitDiff, DaemonError> {
        if path.contains("..") {
            return Err(DaemonError::Session("invalid path".to_string()));
        }
        self.ensure_session_git(session_id).await?;
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        Self::git_diff_in_sandbox(&sandbox, dir, reference, path).await
    }

    async fn git_diff_in_sandbox(
        sandbox: &Arc<dyn Sandbox>,
        dir: &str,
        reference: &str,
        path: &str,
    ) -> Result<crate::git::GitDiff, DaemonError> {
        if path.contains("..") {
            return Err(DaemonError::Session("invalid path".to_string()));
        }
        let head_ref = format!("{reference}:{path}");
        let old = sandbox
            .exec(
                &[
                    "git",
                    "-c",
                    "safe.directory=*",
                    "-C",
                    dir,
                    "show",
                    head_ref.as_str(),
                ],
                Duration::from_secs(30),
            )
            .await
            .map(|r| if r.ok() { r.stdout } else { String::new() })
            .unwrap_or_default();
        let full = format!("{dir}/{path}");
        // Size-gate the working file before reading it — don't pull a huge blob
        // through the sandbox just to discard it. `wc -c` prints "<bytes> <path>".
        let new_size = sandbox
            .exec(&["wc", "-c", full.as_str()], Duration::from_secs(10))
            .await
            .ok()
            .filter(|r| r.ok())
            .and_then(|r| {
                r.stdout
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse::<usize>().ok())
            })
            .unwrap_or(0);
        let too_large =
            old.len() > crate::git::DIFF_MAX_BYTES || new_size > crate::git::DIFF_MAX_BYTES;
        let new = if too_large {
            String::new()
        } else {
            sandbox
                .exec(&["cat", full.as_str()], Duration::from_secs(30))
                .await
                .map(|r| if r.ok() { r.stdout } else { String::new() })
                .unwrap_or_default()
        };
        let binary =
            !too_large && (crate::git::looks_binary(&old) || crate::git::looks_binary(&new));
        // When the file can't be shown inline, blank both sides so neither raw
        // bytes nor a megabyte blob ride along in the response.
        let (old, new) = if too_large || binary {
            (String::new(), String::new())
        } else {
            (old, new)
        };
        Ok(crate::git::GitDiff {
            path: path.to_string(),
            old,
            new,
            binary,
            too_large,
        })
    }

    /// Branch list + current branch.
    pub async fn git_branches(
        &self,
        session_id: &str,
    ) -> Result<crate::git::GitBranches, DaemonError> {
        self.ensure_session_git(session_id).await?;
        let cur = self
            .session_git(session_id, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await?;
        let list = self
            .session_git(session_id, &["branch", "--format=%(refname:short)"])
            .await?;
        Ok(crate::git::parse_branches(&cur.stdout, &list.stdout))
    }

    /// Commit. Returns the fresh status. A no-op commit (nothing staged) is not
    /// an error — the status just comes back unchanged.
    ///
    /// `stage_all` decides which of two different acts this is. `false` commits
    /// the index exactly as the user built it, which is the whole point of
    /// staging a file or a hunk at a time; an earlier version always ran
    /// `add -A` first, so every staging decision was silently discarded at the
    /// moment it was meant to take effect. `true` is the deliberate
    /// stage-everything-and-commit, still worth having as one motion.
    pub async fn git_commit(
        &self,
        session_id: &str,
        message: &str,
        stage_all: bool,
    ) -> Result<crate::git::GitStatus, DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        self.require_no_unresolved_attempt(session_id).await?;
        self.ensure_session_git(session_id).await?;
        if stage_all {
            self.session_git(session_id, &["add", "-A"]).await?;
        }
        let msg = if message.trim().is_empty() {
            "axocoatl: snapshot"
        } else {
            message
        };
        let _ = self
            .session_git(session_id, &["commit", "-q", "-m", msg])
            .await;
        self.git_status(session_id).await
    }

    /// Discard one hunk of a file's unstaged diff, in the working tree.
    ///
    /// The counterpart to staging a hunk. Reviewing a turn means keeping some of
    /// what the agent did and dropping the rest, and without this the smallest
    /// thing you could drop was a whole file — so a file with one good change
    /// and one bad one had no answer except editing it by hand.
    ///
    /// Reverse-applies the hunk to the worktree rather than to the index, which
    /// is the one-character difference from unstaging (`--cached`) and a
    /// completely different act: unstaging moves a change back to the working
    /// tree, this destroys it.
    pub async fn git_revert_hunk(
        &self,
        session_id: &str,
        path: &str,
        index: usize,
    ) -> Result<crate::git::GitStatus, DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        self.require_no_unresolved_attempt(session_id).await?;
        // Always the unstaged diff: a staged change is not in the working tree
        // to revert, so unstage it first and then decide.
        let hunks = self.git_hunks(session_id, path, false).await?;
        let hunk = hunks
            .get(index)
            .ok_or_else(|| DaemonError::Session(format!("no hunk {index} in '{path}'")))?;

        let raw = self
            .session_git(session_id, &["diff", "--no-color", "--", path])
            .await?;
        let (preamble, _) = crate::git::parse_hunks(&raw.stdout);
        let patch = crate::git::one_hunk_patch(&preamble, hunk);

        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let dir = sandbox.root().to_string_lossy().to_string();
        let argv: Vec<&str> = vec![
            "git",
            "-c",
            "safe.directory=*",
            "-C",
            &dir,
            "apply",
            "--reverse",
            "-",
        ];
        let r = sandbox
            .exec_stdin(&argv, &patch, Duration::from_secs(30))
            .await
            .map_err(|e| DaemonError::Session(e.to_string()))?;
        if !r.ok() {
            let why = r.stderr.trim();
            return Err(DaemonError::Session(format!(
                "could not discard that change{}",
                if why.is_empty() {
                    String::new()
                } else {
                    format!(": {}", why.lines().next().unwrap_or(why))
                }
            )));
        }
        self.git_status(session_id).await
    }

    /// Discard working changes — one file (`Some(path)`) or all (`None`,
    /// including untracked). Returns the fresh status.
    /// The hunks of one file's unstaged diff, or of its staged diff.
    ///
    /// Two different questions: "what could I stage" and "what could I unstage",
    /// and they are different sets of hunks against different baselines.
    pub async fn git_hunks(
        &self,
        session_id: &str,
        path: &str,
        staged: bool,
    ) -> Result<Vec<crate::git::Hunk>, DaemonError> {
        if path.contains("..") {
            return Err(DaemonError::Session("invalid path".to_string()));
        }
        self.ensure_session_git(session_id).await?;
        let mut args = vec!["diff"];
        if staged {
            args.push("--cached");
        }
        args.extend_from_slice(&["--no-color", "--", path]);
        let r = self.session_git(session_id, &args).await?;
        Ok(crate::git::parse_hunks(&r.stdout).1)
    }

    /// Stage or unstage a single hunk of a file.
    ///
    /// Rebuilds a patch containing that hunk alone and hands it to `git apply
    /// --cached`, which is how git itself does this. Unstaging applies the
    /// staged hunk in reverse, so the working tree is untouched either way —
    /// this moves a change between the index and the tree, and nothing else.
    pub async fn git_apply_hunk(
        &self,
        session_id: &str,
        path: &str,
        index: usize,
        stage: bool,
    ) -> Result<crate::git::GitStatus, DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        self.require_no_unresolved_attempt(session_id).await?;
        let hunks = self.git_hunks(session_id, path, !stage).await?;
        let hunk = hunks
            .get(index)
            .ok_or_else(|| DaemonError::Session(format!("no hunk {index} in '{path}'")))?;

        // Re-read the preamble from the same diff the hunk came from, so the
        // patch names the file exactly as git just described it.
        let mut args = vec!["diff"];
        if !stage {
            args.push("--cached");
        }
        args.extend_from_slice(&["--no-color", "--", path]);
        let raw = self.session_git(session_id, &args).await?;
        let (preamble, _) = crate::git::parse_hunks(&raw.stdout);
        let patch = crate::git::one_hunk_patch(&preamble, hunk);

        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let dir = sandbox.root().to_string_lossy().to_string();
        let mut argv: Vec<&str> = vec![
            "git",
            "-c",
            "safe.directory=*",
            "-C",
            &dir,
            "apply",
            "--cached",
        ];
        if !stage {
            argv.push("--reverse");
        }
        argv.push("-");
        let r = sandbox
            .exec_stdin(&argv, &patch, Duration::from_secs(30))
            .await
            .map_err(|e| DaemonError::Session(e.to_string()))?;
        if !r.ok() {
            // git's own message says why — a patch that does not apply usually
            // means the file moved under us, and paraphrasing that helps nobody.
            let why = r.stderr.trim();
            return Err(DaemonError::Session(format!(
                "could not {} that change{}",
                if stage { "stage" } else { "unstage" },
                if why.is_empty() {
                    String::new()
                } else {
                    format!(": {}", why.lines().next().unwrap_or(why))
                }
            )));
        }
        self.git_status(session_id).await
    }

    /// Stage paths, or everything when none are given.
    ///
    /// Git's own verb, deliberately. "Accept" and "reject" are an invented
    /// vocabulary that maps onto nothing a user can inspect afterwards; staging
    /// is a real thing in a real index, visible from any other tool and
    /// reversible by the obvious means. Returns the fresh status so a caller
    /// never has to guess what the index now holds.
    pub async fn git_stage(
        &self,
        session_id: &str,
        paths: &[String],
    ) -> Result<crate::git::GitStatus, DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        self.require_no_unresolved_attempt(session_id).await?;
        self.ensure_session_git(session_id).await?;
        if paths.is_empty() {
            self.session_git(session_id, &["add", "-A"]).await?;
        } else {
            for p in paths {
                if p.contains("..") {
                    return Err(DaemonError::Session(format!("invalid path '{p}'")));
                }
                self.session_git(session_id, &["add", "--", p]).await?;
            }
        }
        self.git_status(session_id).await
    }

    /// Unstage paths, or everything when none are given.
    ///
    /// `reset` leaves the working tree alone: unstaging is about the index, and
    /// silently reverting the file as well would destroy work the user only
    /// meant to un-mark. Discarding is a separate verb because it is a separate
    /// intent.
    pub async fn git_unstage(
        &self,
        session_id: &str,
        paths: &[String],
    ) -> Result<crate::git::GitStatus, DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        self.require_no_unresolved_attempt(session_id).await?;
        self.ensure_session_git(session_id).await?;
        if paths.is_empty() {
            let _ = self.session_git(session_id, &["reset", "-q"]).await;
        } else {
            for p in paths {
                if p.contains("..") {
                    return Err(DaemonError::Session(format!("invalid path '{p}'")));
                }
                // Ignore the exit code: `reset` reports non-zero on a repo with
                // no commits yet, where there is nothing to reset *to* but the
                // file is still correctly removed from the index.
                let _ = self
                    .session_git(session_id, &["reset", "-q", "--", p])
                    .await;
            }
        }
        self.git_status(session_id).await
    }

    pub async fn git_discard(
        &self,
        session_id: &str,
        path: Option<&str>,
    ) -> Result<crate::git::GitStatus, DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        self.require_no_unresolved_attempt(session_id).await?;
        self.ensure_session_git(session_id).await?;
        match path {
            Some(p) => {
                if p.contains("..") {
                    return Err(DaemonError::Session("invalid path".to_string()));
                }
                let _ = self.session_git(session_id, &["checkout", "--", p]).await;
                let _ = self
                    .session_git(session_id, &["clean", "-fd", "--", p])
                    .await;
            }
            None => {
                let _ = self.session_git(session_id, &["checkout", "--", "."]).await;
                let _ = self.session_git(session_id, &["clean", "-fd"]).await;
            }
        }
        self.git_status(session_id).await
    }

    /// Switch branches / checkout a ref. Returns the fresh status.
    pub async fn git_checkout(
        &self,
        session_id: &str,
        reference: &str,
    ) -> Result<crate::git::GitStatus, DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        self.require_no_unresolved_attempt(session_id).await?;
        self.ensure_session_git(session_id).await?;
        let r = self
            .session_git(session_id, &["checkout", reference])
            .await?;
        if !r.ok() {
            return Err(DaemonError::Session(format!(
                "checkout failed: {}",
                r.stderr.trim()
            )));
        }
        self.git_status(session_id).await
    }

    // ── Variants: parallel branch exploration ───────────────────────────
    // Every attempt is an independent clone on a set-scoped branch, stored
    // below `{root}/.axo-variants/{session-key}/{set-key}/{index}` and mounted
    // alone into its own Podman container. The reserved root is git-excluded so
    // attempt metadata never appears as a primary workspace change.

    /// The in-sandbox working dir of a session, as a string. This is the
    /// sandbox root (== working_dir for Podman, the in-VM clone path for E2B),
    /// so all variant paths built from it are addressable inside the instance.
    async fn session_dir(&self, session_id: &str) -> Result<String, DaemonError> {
        let s = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        Ok(self
            .ensure_sandbox(&s)
            .await?
            .root()
            .to_string_lossy()
            .to_string())
    }

    fn require_git_output(result: ExecResult, action: &str) -> Result<String, DaemonError> {
        if result.ok() {
            Ok(result.stdout.trim().to_string())
        } else {
            Err(DaemonError::Session(format!(
                "{action}: {}",
                result.stderr.trim()
            )))
        }
    }

    fn require_raw_git_output(result: ExecResult, action: &str) -> Result<String, DaemonError> {
        if result.ok() {
            Ok(result.stdout)
        } else {
            Err(DaemonError::Session(format!(
                "{action}: {}",
                result.stderr.trim()
            )))
        }
    }

    /// Materialize the session's exact working state as an unreachable commit.
    ///
    /// An alternate index makes this side-effect-free for the user: their real
    /// staged/unstaged split, current branch, and working files are unchanged.
    /// The resulting commit becomes reachable only through the attempt branches
    /// created immediately afterward.
    async fn snapshot_attempt_base(
        &self,
        session_id: &str,
        set_id: &str,
    ) -> Result<(String, String), DaemonError> {
        self.ensure_session_git(session_id).await?;
        let dir = self.session_dir(session_id).await?;
        let git_dir = Self::require_git_output(
            self.session_git(session_id, &["rev-parse", "--absolute-git-dir"])
                .await?,
            "locating the git directory for the attempt snapshot",
        )?;
        let common_git_dir = Self::require_git_output(
            self.session_git(
                session_id,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .await?,
            "locating the common git directory for attempt exclusions",
        )?;
        let index_path = format!(
            "{git_dir}/axo-attempt-index-{}",
            crate::attempts::set_key(set_id)
        );
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let tracked_internal = Self::require_git_output(
            self.session_git(session_id, &["ls-files", "--", ".axo-variants"])
                .await?,
            "checking the reserved attempt metadata path",
        )?;
        if !tracked_internal.trim().is_empty() {
            return Err(DaemonError::AttemptConflict(
                "the repository tracks .axo-variants, which Axocoatl reserves for attempt metadata"
                    .to_string(),
            ));
        }
        let exclude = format!("{common_git_dir}/info/exclude");
        let excluded = sandbox
            .exec(
                &["grep", "-qxF", ".axo-variants/", &exclude],
                Duration::from_secs(10),
            )
            .await;
        if !matches!(excluded, Ok(ref result) if result.ok()) {
            let append = sandbox
                .exec_stdin(
                    &["tee", "-a", &exclude],
                    "\n.axo-variants/\n",
                    Duration::from_secs(10),
                )
                .await
                .map_err(|error| DaemonError::Session(error.to_string()))?;
            if !append.ok() {
                return Err(DaemonError::Session(format!(
                    "excluding attempt worktrees from the session snapshot: {}",
                    append.stderr.trim()
                )));
            }
        }
        let _ = sandbox
            .exec(&["rm", "-f", &index_path], Duration::from_secs(10))
            .await;

        let outcome = async {
            // A repository can be perfectly valid before its first commit.
            // Seed from an empty tree there; otherwise preserve HEAD as the
            // snapshot's parent so the attempt branches retain normal history.
            let head_result = self
                .session_git(session_id, &["rev-parse", "--verify", "HEAD"])
                .await?;
            let head = head_result
                .ok()
                .then(|| head_result.stdout.trim().to_string())
                .filter(|value| !value.is_empty());
            let seed = match head.as_deref() {
                Some(parent) => {
                    self.session_git_with_index(
                        session_id,
                        &dir,
                        &index_path,
                        &["read-tree", parent],
                    )
                    .await?
                }
                None => {
                    self.session_git_with_index(
                        session_id,
                        &dir,
                        &index_path,
                        &["read-tree", "--empty"],
                    )
                    .await?
                }
            };
            Self::require_git_output(seed, "seeding the attempt snapshot index")?;
            Self::require_git_output(
                self.session_git_with_index(session_id, &dir, &index_path, &["add", "-A"])
                    .await?,
                "capturing the session working tree for attempts",
            )?;
            let tree = Self::require_git_output(
                self.session_git_with_index(session_id, &dir, &index_path, &["write-tree"])
                    .await?,
                "writing the attempt snapshot tree",
            )?;
            let message = format!(
                "axocoatl attempt snapshot {}",
                crate::attempts::set_key(set_id)
            );
            let commit_result = match head.as_deref() {
                Some(parent) => {
                    self.session_git_with_index(
                        session_id,
                        &dir,
                        &index_path,
                        &["commit-tree", &tree, "-p", parent, "-m", &message],
                    )
                    .await?
                }
                None => {
                    self.session_git_with_index(
                        session_id,
                        &dir,
                        &index_path,
                        &["commit-tree", &tree, "-m", &message],
                    )
                    .await?
                }
            };
            let commit =
                Self::require_git_output(commit_result, "creating the attempt snapshot commit")?;
            Ok((commit, tree))
        }
        .await;

        // This path is derived from git's own absolute git-dir plus a UUID key;
        // removing it cannot touch the user's real index.
        let _ = sandbox
            .exec(&["rm", "-f", &index_path], Duration::from_secs(10))
            .await;
        outcome
    }

    async fn write_json_file<T: serde::Serialize>(
        sandbox: &Arc<dyn Sandbox>,
        path: &str,
        value: &T,
    ) -> Result<(), DaemonError> {
        let parent = std::path::Path::new(path)
            .parent()
            .ok_or_else(|| DaemonError::Session(format!("invalid metadata path '{path}'")))?
            .to_string_lossy()
            .to_string();
        let mk = sandbox
            .exec(&["mkdir", "-p", &parent], Duration::from_secs(10))
            .await
            .map_err(|e| DaemonError::Session(e.to_string()))?;
        if !mk.ok() {
            return Err(DaemonError::Session(format!(
                "creating attempt metadata directory: {}",
                mk.stderr.trim()
            )));
        }
        let json = serde_json::to_string(value)
            .map_err(|e| DaemonError::Session(format!("serializing attempt metadata: {e}")))?;
        let tmp = format!("{path}.tmp");
        let write = sandbox
            .exec_stdin(&["tee", &tmp], &json, Duration::from_secs(10))
            .await
            .map_err(|e| DaemonError::Session(e.to_string()))?;
        if !write.ok() {
            return Err(DaemonError::Session(format!(
                "writing attempt metadata: {}",
                write.stderr.trim()
            )));
        }
        let mv = sandbox
            .exec(&["mv", &tmp, path], Duration::from_secs(10))
            .await
            .map_err(|e| DaemonError::Session(e.to_string()))?;
        if !mv.ok() {
            return Err(DaemonError::Session(format!(
                "publishing attempt metadata: {}",
                mv.stderr.trim()
            )));
        }
        Ok(())
    }

    async fn read_json_file<T: serde::de::DeserializeOwned>(
        sandbox: &Arc<dyn Sandbox>,
        path: &str,
    ) -> Result<Option<T>, DaemonError> {
        let exists = sandbox
            .exec(&["test", "-f", path], Duration::from_secs(10))
            .await
            .map_err(|e| DaemonError::Session(e.to_string()))?;
        if !exists.ok() {
            if exists.exit_code == 1 && exists.stderr.trim().is_empty() {
                return Ok(None);
            }
            return Err(DaemonError::Session(format!(
                "checking attempt metadata at '{path}': {}",
                if exists.stderr.trim().is_empty() {
                    format!("exit code {}", exists.exit_code)
                } else {
                    exists.stderr.trim().to_string()
                }
            )));
        }
        let result = sandbox
            .exec(&["cat", path], Duration::from_secs(10))
            .await
            .map_err(|e| DaemonError::Session(e.to_string()))?;
        if !result.ok() {
            return Err(DaemonError::Session(format!(
                "reading attempt metadata at '{path}': {}",
                result.stderr.trim()
            )));
        }
        serde_json::from_str(&result.stdout).map(Some).map_err(|e| {
            DaemonError::Session(format!(
                "attempt metadata at '{path}' is corrupt; refusing to overwrite it: {e}"
            ))
        })
    }

    /// Read attempt metadata directly from the host without provisioning a
    /// session sandbox. Background/reconnect reads use this path so observing
    /// a soft-closed session cannot restart Podman or rerun postCreate hooks.
    async fn read_host_json_file<T: serde::de::DeserializeOwned>(
        path: &std::path::Path,
    ) -> Result<Option<T>, DaemonError> {
        let metadata = match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(DaemonError::Session(format!(
                    "checking attempt metadata at '{}': {error}",
                    path.display()
                )))
            }
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(DaemonError::Session(format!(
                "attempt metadata at '{}' is not a regular file",
                path.display()
            )));
        }
        let json = tokio::fs::read_to_string(path).await.map_err(|error| {
            DaemonError::Session(format!(
                "reading attempt metadata at '{}': {error}",
                path.display()
            ))
        })?;
        serde_json::from_str(&json).map(Some).map_err(|error| {
            DaemonError::Session(format!(
                "attempt metadata at '{}' is corrupt; refusing to overwrite it: {error}",
                path.display()
            ))
        })
    }

    async fn persist_attempt_set(
        &self,
        sandbox: &Arc<dyn Sandbox>,
        set: &crate::git::AttemptSet,
    ) -> Result<(), DaemonError> {
        let root = sandbox.root().to_string_lossy();
        let attempt_root = crate::attempts::attempt_root(
            std::path::Path::new(root.as_ref()),
            &set.session_id,
            &set.id,
        );
        let current_root = crate::attempts::session_attempts_root(
            std::path::Path::new(root.as_ref()),
            &set.session_id,
        );
        Self::write_json_file(
            sandbox,
            &attempt_root.join("set.json").to_string_lossy(),
            set,
        )
        .await?;
        Self::write_json_file(
            sandbox,
            &current_root.join("current.json").to_string_lossy(),
            set,
        )
        .await
    }

    async fn rollback_attempt_setup(
        &self,
        sandbox: &Arc<dyn Sandbox>,
        session_id: &str,
        set: &mut crate::git::AttemptSet,
        original: DaemonError,
    ) -> DaemonError {
        set.state = crate::git::AttemptSetState::Discarding;
        if let Err(mark_error) = self.persist_attempt_set(sandbox, set).await {
            return DaemonError::Session(format!(
                "{original}; rollback was not started because its durable cleanup marker could not be written: {mark_error}"
            ));
        }
        match self.remove_attempt_worktrees(session_id, set).await {
            Ok(()) => original,
            Err(cleanup_error) => DaemonError::Session(format!(
                "{original}; rollback is marked for retry after incomplete cleanup: {cleanup_error}"
            )),
        }
    }

    /// Read the durable current-set pointer without provisioning or attaching a
    /// sandbox. This is the only safe probe for ordinary turns, Close/Delete,
    /// and peer-session conflict checks: observing state must never create a
    /// container, run postCreate commands, or allocate a paid remote VM.
    async fn peek_current_attempt_set(
        &self,
        session_id: &str,
    ) -> Result<Option<crate::git::AttemptSet>, DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        // Attempt sets are created only by the local Podman backend, so their
        // durable metadata always lives below the host workspace. Probe that
        // host path even if configuration changed to E2B after a restart: an
        // unresolved local set must keep owning the workspace rather than
        // becoming invisible merely because the current backend is remote.
        Self::read_current_attempt_set_host(&session.working_dir, session_id).await
    }

    async fn read_current_attempt_set_host(
        workspace: &std::path::Path,
        session_id: &str,
    ) -> Result<Option<crate::git::AttemptSet>, DaemonError> {
        let path =
            crate::attempts::session_attempts_root(workspace, session_id).join("current.json");
        let metadata = match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(DaemonError::Session(format!(
                    "checking attempt metadata at '{}': {error}",
                    path.display()
                )))
            }
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(DaemonError::Session(format!(
                "attempt metadata at '{}' is not a regular file",
                path.display()
            )));
        }
        let json = tokio::fs::read_to_string(&path).await.map_err(|error| {
            DaemonError::Session(format!(
                "reading attempt metadata at '{}': {error}",
                path.display()
            ))
        })?;
        let set: crate::git::AttemptSet = serde_json::from_str(&json).map_err(|error| {
            DaemonError::Session(format!(
                "attempt metadata at '{}' is corrupt; refusing to overwrite it: {error}",
                path.display()
            ))
        })?;
        if set.session_id != session_id {
            return Err(DaemonError::Session(format!(
                "attempt metadata belongs to session '{}', not '{session_id}'",
                set.session_id
            )));
        }
        Ok(Some(set))
    }

    async fn current_attempt_set(
        &self,
        session_id: &str,
    ) -> Result<Option<crate::git::AttemptSet>, DaemonError> {
        let set = self.peek_current_attempt_set(session_id).await?;
        if let Some(set) = &set {
            Self::require_attempt_resolution_backend(&self.config.sandbox.backend)?;
            let session = self
                .get_session(session_id)
                .await
                .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
            let sandbox = self.ensure_attempt_recovery_sandbox(&session).await?;
            self.validate_attempt_set(&sandbox, set).await?;
        }
        Ok(set)
    }

    fn require_attempt_resolution_backend(backend: &str) -> Result<(), DaemonError> {
        if backend == "e2b" {
            Err(DaemonError::AttemptConflict(
                "this workspace has an unresolved local attempt set; switch the sandbox backend back to Podman to review, Keep, or Discard it"
                    .to_string(),
            ))
        } else {
            Ok(())
        }
    }

    fn validate_attempt_set_identity(
        root: &std::path::Path,
        set: &crate::git::AttemptSet,
    ) -> Result<(), DaemonError> {
        let corrupt = |detail: String| {
            DaemonError::Session(format!(
                "attempt metadata is inconsistent; refusing to execute it: {detail}"
            ))
        };
        if uuid::Uuid::parse_str(&set.id).is_err() {
            return Err(corrupt("set id is not a UUID".to_string()));
        }
        if set.lanes.is_empty() || set.lanes.len() > 100 {
            return Err(corrupt("lane count is outside 1..=100".to_string()));
        }
        let valid_oid = |value: &str| {
            matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        };
        if !valid_oid(&set.base_sha) || !valid_oid(&set.base_tree) {
            return Err(corrupt("snapshot object ids are invalid".to_string()));
        }
        for (expected, lane) in set.lanes.iter().enumerate() {
            if lane.index != expected {
                return Err(corrupt(format!(
                    "lane indices must be unique and contiguous; expected {expected}, found {}",
                    lane.index
                )));
            }
            let branch = crate::attempts::branch_name(&set.id, expected);
            let worktree = crate::attempts::worktree_path(root, &set.session_id, &set.id, expected)
                .to_string_lossy()
                .to_string();
            if lane.branch != branch || lane.worktree != worktree {
                return Err(corrupt(format!(
                    "lane {} storage identity does not match its set",
                    expected + 1
                )));
            }
        }
        let keep_phase = matches!(
            set.state,
            crate::git::AttemptSetState::Applying
                | crate::git::AttemptSetState::Applied
                | crate::git::AttemptSetState::TranscriptRecorded
        );
        match (keep_phase, set.kept_index) {
            (true, None) => Err(corrupt(
                "Keep transaction is missing its selected lane".to_string(),
            )),
            (false, Some(_)) => Err(corrupt(
                "selected Keep lane exists outside a Keep transaction".to_string(),
            )),
            (_, Some(index)) if index >= set.lanes.len() => Err(corrupt(format!(
                "selected Keep lane {index} does not exist"
            ))),
            _ => Ok(()),
        }
    }

    async fn validate_attempt_set(
        &self,
        sandbox: &Arc<dyn Sandbox>,
        set: &crate::git::AttemptSet,
    ) -> Result<(), DaemonError> {
        Self::validate_attempt_set_identity(sandbox.root(), set)?;
        let corrupt = |detail: String| {
            DaemonError::Session(format!(
                "attempt metadata is inconsistent; refusing to execute it: {detail}"
            ))
        };

        // Cleanup can remove the protected ref and set directory before a
        // failing current-pointer unlink is retried. At that final durable
        // phase, identity validation above is sufficient and deletion remains
        // derived rather than manifest-directed.
        if !matches!(
            set.state,
            crate::git::AttemptSetState::Discarding
                | crate::git::AttemptSetState::TranscriptRecorded
        ) {
            let protected = Self::require_git_output(
                self.session_git(
                    &set.session_id,
                    &["rev-parse", "--verify", &crate::attempts::base_ref(&set.id)],
                )
                .await?,
                "validating the protected attempt snapshot",
            )?;
            if protected != set.base_sha {
                return Err(corrupt("protected snapshot ref changed".to_string()));
            }
            let tree_expr = format!("{}^{{tree}}", set.base_sha);
            let tree = Self::require_git_output(
                self.session_git(&set.session_id, &["rev-parse", &tree_expr])
                    .await?,
                "validating the attempt snapshot tree",
            )?;
            if tree != set.base_tree {
                return Err(corrupt(
                    "snapshot tree does not match its commit".to_string(),
                ));
            }
        }
        Ok(())
    }

    async fn require_attempt_set(
        &self,
        session_id: &str,
        set_id: &str,
    ) -> Result<crate::git::AttemptSet, DaemonError> {
        let Some(set) = self.current_attempt_set(session_id).await? else {
            return Err(DaemonError::Session(
                "this session has no current attempt set".to_string(),
            ));
        };
        if set.id != set_id {
            return Err(DaemonError::AttemptConflict(format!(
                "attempt set '{set_id}' is stale; the current set is '{}'",
                set.id
            )));
        }
        Ok(set)
    }

    async fn attempt_operation(&self, session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        // SessionStore canonicalizes working_dir. Keying the lease by that
        // path—not by session id—prevents two sessions opened on the same
        // folder from snapshotting, checking, or applying concurrently.
        let key = self
            .get_session(session_id)
            .await
            .map(|session| format!("workspace:{}", session.working_dir.display()))
            .unwrap_or_else(|| format!("session:{session_id}"));
        let mut operations = self.attempt_operations.lock().await;
        operations
            .entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Resolve the lane's own container boundary for daemon-controlled review
    /// commands. A live run must already own a running container; after a
    /// daemon restart, a clean replacement may be created over the durable
    /// independent clone so status, diff, and Discard remain recoverable.
    async fn attempt_sandbox(
        &self,
        session: &Session,
        set_id: &str,
        index: usize,
    ) -> Result<Arc<dyn Sandbox>, DaemonError> {
        let primary = self.ensure_sandbox(session).await?;
        let worktree = crate::attempts::worktree_path(primary.root(), &session.id, set_id, index);
        let container_id = crate::attempts::container_id(&session.id, set_id, index);
        let running = SessionSandbox::named_running(&container_id)
            .await
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        if running {
            return Ok(Arc::new(SessionSandbox::attach_named(
                &container_id,
                &worktree,
            )));
        }
        let live = self
            .active_attempts
            .lock()
            .await
            .get(&session.id)
            .is_some_and(|run| run.set_id == set_id);
        if live {
            return Err(DaemonError::Session(format!(
                "attempt {} lost its isolated sandbox while it was running",
                index + 1
            )));
        }
        let sc = &self.config.sandbox;
        let policy = axocoatl_isolation::session_sandbox::SandboxPolicy {
            // Recreate the same explicitly trusted project setup used by the
            // execution container. Checks should run in a clean process/container
            // boundary, but not in a different dependency environment.
            allow_post_create: sc.allow_post_create_command,
            allow_untrusted_image: sc.allow_untrusted_images,
            network: match sc.network.as_str() {
                "none" => axocoatl_isolation::session_sandbox::SandboxNetwork::None,
                _ => axocoatl_isolation::session_sandbox::SandboxNetwork::Bridge,
            },
            require_resource_limits: sc.require_resource_limits,
        };
        Ok(Arc::new(
            SessionSandbox::start(
                &container_id,
                &worktree,
                session.image.as_deref(),
                &[],
                &session.post_create_commands,
                &policy,
            )
            .await
            .map_err(|error| {
                DaemonError::Session(format!(
                    "restoring isolated review sandbox for attempt {}: {error}",
                    index + 1
                ))
            })?,
        ))
    }

    async fn attempt_git(
        sandbox: &Arc<dyn Sandbox>,
        args: &[&str],
    ) -> Result<ExecResult, DaemonError> {
        let dir = sandbox.root().to_string_lossy().to_string();
        let mut argv = vec![
            "env",
            "GIT_CONFIG_NOSYSTEM=1",
            "GIT_CONFIG_GLOBAL=/dev/null",
            "git",
            "-c",
            "safe.directory=*",
            "-c",
            "user.email=agent@axocoatl.local",
            "-c",
            "user.name=Axocoatl",
            "-C",
            &dir,
        ];
        argv.extend_from_slice(args);
        sandbox
            .exec(&argv, Duration::from_secs(60))
            .await
            .map_err(|error| DaemonError::Session(error.to_string()))
    }

    /// Remove candidate-controlled Git commands before daemon review. Agents
    /// can edit `.git/config`; clean/smudge filters and diff drivers there are
    /// arbitrary programs. Review still runs in the lane container, but a
    /// sanitized local config also makes the captured patch a passive read of
    /// the checked files rather than another candidate execution step.
    async fn sanitize_attempt_git_config_at(
        attempt_root: &std::path::Path,
        base_sha: &str,
    ) -> Result<(), DaemonError> {
        use tokio::io::AsyncWriteExt;

        let git_dir = attempt_root.join(".git");
        let metadata = tokio::fs::symlink_metadata(&git_dir)
            .await
            .map_err(|error| {
                DaemonError::Session(format!("reading attempt Git directory: {error}"))
            })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(DaemonError::Session(
                "attempt replaced its Git directory; this lane cannot be reviewed".to_string(),
            ));
        }
        let config = if base_sha.len() == 64 {
            "[core]\n\trepositoryformatversion = 1\n\tfilemode = true\n\tsymlinks = true\n\tbare = false\n\tlogallrefupdates = true\n[extensions]\n\tobjectFormat = sha256\n"
        } else {
            "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tsymlinks = true\n\tbare = false\n\tlogallrefupdates = true\n"
        };
        let path = git_dir.join("config");
        match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                tokio::fs::remove_file(&path).await.map_err(|error| {
                    DaemonError::Session(format!("removing attempt Git config: {error}"))
                })?;
            }
            Ok(_) => {
                return Err(DaemonError::Session(
                    "attempt replaced its Git config with a non-file".to_string(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(DaemonError::Session(format!(
                    "reading attempt Git config: {error}"
                )));
            }
        }
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map_err(|error| {
                DaemonError::Session(format!("creating safe attempt Git config: {error}"))
            })?;
        file.write_all(config.as_bytes()).await.map_err(|error| {
            DaemonError::Session(format!("writing safe attempt Git config: {error}"))
        })?;
        file.sync_all().await.map_err(|error| {
            DaemonError::Session(format!("syncing safe attempt Git config: {error}"))
        })
    }

    /// Freeze a lane's checked working tree into Git objects without reviving
    /// its container. The primary sandbox runs plumbing directly against the
    /// stopped host clone, after its candidate-controlled config is replaced.
    /// Only ASCII object ids cross the sandbox String boundary. The display
    /// patch is written to a file and hashed as raw bytes.
    async fn capture_attempt_candidate(
        &self,
        session_id: &str,
        set: &crate::git::AttemptSet,
        index: usize,
        publish_in_primary: bool,
    ) -> Result<CapturedCandidate, DaemonError> {
        if !set.lanes.iter().any(|lane| lane.index == index) {
            return Err(DaemonError::Session(format!(
                "attempt {} does not exist",
                index + 1
            )));
        }
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let primary = self.ensure_sandbox(&session).await?;
        let worktree = crate::attempts::worktree_path(primary.root(), session_id, &set.id, index);
        Self::sanitize_attempt_git_config_at(&worktree, &set.base_sha).await?;
        let git_dir = worktree.join(".git");
        let key = crate::attempts::set_key(&set.id);
        let index_path = git_dir.join(format!("axo-checked-index-{key}-{index}"));
        let raw_path = git_dir.join(format!("axo-checked-raw-{key}-{index}"));
        let patch_path = git_dir.join(format!("axo-checked-patch-{key}-{index}"));
        let worktree_string = worktree.to_string_lossy().to_string();
        let index_string = index_path.to_string_lossy().to_string();
        let raw_output = format!("--output={}", raw_path.to_string_lossy());
        let patch_output = format!("--output={}", patch_path.to_string_lossy());

        for path in [
            index_path.clone(),
            index_path.with_extension("lock"),
            raw_path.clone(),
            patch_path.clone(),
        ] {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(DaemonError::Session(format!(
                        "removing stale checked-tree file '{}': {error}",
                        path.display()
                    )))
                }
            }
        }

        let outcome = async {
            Self::require_git_output(
                self.session_git_with_index(
                    session_id,
                    &worktree_string,
                    &index_string,
                    &["read-tree", &set.base_sha],
                )
                .await?,
                &format!("seeding attempt {} checked-tree index", index + 1),
            )?;
            Self::require_git_output(
                self.session_git_with_index(
                    session_id,
                    &worktree_string,
                    &index_string,
                    &["add", "-A"],
                )
                .await?,
                &format!("capturing attempt {} checked tree", index + 1),
            )?;
            let tree_oid = Self::require_git_output(
                self.session_git_with_index(
                    session_id,
                    &worktree_string,
                    &index_string,
                    &["write-tree"],
                )
                .await?,
                &format!("writing attempt {} checked tree", index + 1),
            )?;
            let message = format!("axocoatl checked attempt {key} {index}");
            let commit_oid = Self::require_git_output(
                self.session_git_at(
                    session_id,
                    &worktree_string,
                    &[
                        "commit-tree",
                        &tree_oid,
                        "-p",
                        &set.base_sha,
                        "-m",
                        &message,
                    ],
                )
                .await?,
                &format!("protecting attempt {} checked tree", index + 1),
            )?;
            let checked_ref = crate::attempts::checked_candidate_ref(&set.id, index);
            Self::require_git_output(
                self.session_git_at(
                    session_id,
                    &worktree_string,
                    &["update-ref", &checked_ref, &commit_oid],
                )
                .await?,
                &format!("publishing attempt {} checked tree", index + 1),
            )?;
            Self::require_git_output(
                self.session_git_at(
                    session_id,
                    &worktree_string,
                    &[
                        "diff",
                        "--raw",
                        "-z",
                        "--no-renames",
                        &raw_output,
                        &set.base_sha,
                        &tree_oid,
                    ],
                )
                .await?,
                &format!("enumerating attempt {} checked paths", index + 1),
            )?;
            Self::require_git_output(
                self.session_git_at(
                    session_id,
                    &worktree_string,
                    &[
                        "-c",
                        "diff.algorithm=myers",
                        "diff",
                        "--binary",
                        "--full-index",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--no-renames",
                        &patch_output,
                        &set.base_sha,
                        &tree_oid,
                    ],
                )
                .await?,
                &format!("hashing attempt {} checked delta", index + 1),
            )?;
            let raw = tokio::fs::read(&raw_path).await.map_err(|error| {
                DaemonError::Session(format!(
                    "reading attempt {} raw checked paths: {error}",
                    index + 1
                ))
            })?;
            let patch = tokio::fs::read(&patch_path).await.map_err(|error| {
                DaemonError::Session(format!(
                    "reading attempt {} raw checked delta: {error}",
                    index + 1
                ))
            })?;
            let (paths, changes_gitlink) = Self::parse_raw_tree_diff(&raw)?;
            let patch_sha256 = Self::bytes_sha256(&patch);

            if publish_in_primary {
                let refspec = format!("+{checked_ref}:{checked_ref}");
                Self::require_git_output(
                    self.session_git(
                        session_id,
                        &[
                            "fetch",
                            "--no-tags",
                            "--no-write-fetch-head",
                            "--force",
                            &worktree_string,
                            &refspec,
                        ],
                    )
                    .await?,
                    &format!("importing attempt {} checked tree", index + 1),
                )?;
                let imported = Self::require_git_output(
                    self.session_git(session_id, &["rev-parse", "--verify", &checked_ref])
                        .await?,
                    &format!("validating attempt {} checked commit", index + 1),
                )?;
                let tree_expression = format!("{checked_ref}^{{tree}}");
                let imported_tree = Self::require_git_output(
                    self.session_git(session_id, &["rev-parse", &tree_expression])
                        .await?,
                    &format!("validating attempt {} checked tree", index + 1),
                )?;
                if imported != commit_oid || imported_tree != tree_oid {
                    return Err(DaemonError::Session(format!(
                        "attempt {} checked object import changed identity",
                        index + 1
                    )));
                }
            }

            Ok(CapturedCandidate {
                checked: StoredCheckedTree {
                    index,
                    commit_oid,
                    tree_oid,
                    patch_sha256,
                    changes_gitlink,
                },
                paths,
            })
        }
        .await;

        for path in [
            index_path.clone(),
            index_path.with_extension("lock"),
            raw_path,
            patch_path,
        ] {
            let _ = tokio::fs::remove_file(path).await;
        }
        outcome
    }

    fn bytes_sha256(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(bytes))
    }

    fn patch_sha256(patch: &str) -> String {
        Self::bytes_sha256(patch.as_bytes())
    }

    fn validate_keep_path(path: &str) -> Result<(), DaemonError> {
        use std::path::Component;

        let candidate = std::path::Path::new(path);
        if path.is_empty()
            || path.contains('\u{fffd}')
            || candidate.is_absolute()
            || !candidate
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            || path == ".axo-variants"
            || path.starts_with(".axo-variants/")
        {
            return Err(DaemonError::AttemptConflict(format!(
                "attempt changed unsupported or reserved path {path:?}"
            )));
        }
        Ok(())
    }

    /// Parse `git diff --raw -z --no-renames`. Strict UTF-8 is intentional:
    /// public path wire types are strings, so an unrepresentable filename must
    /// be rejected rather than silently replaced and installed at another path.
    fn parse_raw_tree_diff(raw: &[u8]) -> Result<(Vec<String>, bool), DaemonError> {
        let mut fields = raw.split(|byte| *byte == 0).peekable();
        let mut paths = Vec::new();
        let mut changes_gitlink = false;
        while let Some(header) = fields.next() {
            if header.is_empty() {
                if fields.peek().is_none() {
                    break;
                }
                return Err(DaemonError::Session(
                    "Git returned malformed raw attempt metadata".to_string(),
                ));
            }
            let path = fields.next().ok_or_else(|| {
                DaemonError::Session("Git omitted a path from raw attempt metadata".to_string())
            })?;
            if path.is_empty() {
                return Err(DaemonError::Session(
                    "Git returned an empty changed path".to_string(),
                ));
            }
            let header = std::str::from_utf8(header).map_err(|_| {
                DaemonError::Session("Git returned non-ASCII raw diff metadata".to_string())
            })?;
            let columns: Vec<&str> = header.split_ascii_whitespace().collect();
            if columns.len() != 5
                || !columns[0].starts_with(':')
                || columns[0].len() != 7
                || columns[1].len() != 6
                || columns[4].len() != 1
            {
                return Err(DaemonError::Session(format!(
                    "Git returned malformed raw diff header {header:?}"
                )));
            }
            changes_gitlink |= &columns[0][1..] == "160000" || columns[1] == "160000";
            let path = std::str::from_utf8(path).map_err(|_| {
                DaemonError::AttemptConflict(
                    "attempt changed a filename that is not valid UTF-8 and cannot be kept"
                        .to_string(),
                )
            })?;
            Self::validate_keep_path(path)?;
            paths.push(path.to_string());
        }
        paths.sort();
        paths.dedup();
        Ok((paths, changes_gitlink))
    }

    fn parse_tree_entries(output: &str) -> Result<HashMap<String, String>, DaemonError> {
        let mut entries = HashMap::new();
        for record in output.split('\0').filter(|record| !record.is_empty()) {
            let (identity, path) = record.split_once('\t').ok_or_else(|| {
                DaemonError::Session("Git returned malformed tree metadata".to_string())
            })?;
            Self::validate_keep_path(path)?;
            if entries
                .insert(path.to_string(), identity.to_string())
                .is_some()
            {
                return Err(DaemonError::Session(format!(
                    "Git returned duplicate tree path {path:?}"
                )));
            }
        }
        Ok(entries)
    }

    fn keep_tree_leaf_matches_fingerprint(
        identity: &str,
        fingerprint: &Option<StoredFileFingerprint>,
    ) -> bool {
        let Some(fingerprint) = fingerprint.as_ref() else {
            return false;
        };
        match identity.split_ascii_whitespace().next() {
            Some("100644") => fingerprint.kind == "file" && !fingerprint.executable,
            Some("100755") => fingerprint.kind == "file" && fingerprint.executable,
            Some("120000") => fingerprint.kind == "symlink" && !fingerprint.executable,
            _ => false,
        }
    }

    async fn fingerprint_file(
        path: &std::path::Path,
    ) -> Result<Option<StoredFileFingerprint>, DaemonError> {
        let metadata = match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) => metadata,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None)
            }
            Err(error) => {
                return Err(DaemonError::Session(format!(
                    "reading Keep path '{}': {error}",
                    path.display()
                )))
            }
        };
        if metadata.file_type().is_symlink() {
            let target = tokio::fs::read_link(path).await.map_err(|error| {
                DaemonError::Session(format!(
                    "reading Keep symlink '{}': {error}",
                    path.display()
                ))
            })?;
            return Ok(Some(StoredFileFingerprint {
                kind: "symlink".to_string(),
                sha256: Self::bytes_sha256(target.as_os_str().as_encoded_bytes()),
                executable: false,
            }));
        }
        if metadata.is_dir() {
            return Ok(Some(StoredFileFingerprint {
                kind: "directory".to_string(),
                sha256: String::new(),
                executable: false,
            }));
        }
        if !metadata.is_file() {
            return Err(DaemonError::AttemptConflict(format!(
                "Keep path '{}' is not a regular file or symlink",
                path.display()
            )));
        }
        let bytes = tokio::fs::read(path).await.map_err(|error| {
            DaemonError::Session(format!("reading Keep path '{}': {error}", path.display()))
        })?;
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        Ok(Some(StoredFileFingerprint {
            kind: "file".to_string(),
            sha256: Self::bytes_sha256(&bytes),
            executable,
        }))
    }

    /// Fingerprint a workspace leaf without ever traversing an unexpected
    /// symlink or non-directory parent. A non-directory parent that is itself
    /// in the journal is a legitimate file/symlink→directory transition, so a
    /// descendant is logically absent on that side of the transaction.
    async fn fingerprint_keep_workspace_path(
        workspace: &std::path::Path,
        relative: &std::path::Path,
        affected: &HashSet<String>,
    ) -> Result<Option<StoredFileFingerprint>, DaemonError> {
        let mut current = workspace.to_path_buf();
        let mut logical = std::path::PathBuf::new();
        if let Some(parent) = relative.parent() {
            for component in parent.components() {
                logical.push(component.as_os_str());
                current.push(component.as_os_str());
                match tokio::fs::symlink_metadata(&current).await {
                    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                    Ok(_) => {
                        let logical = logical.to_str().ok_or_else(|| {
                            DaemonError::AttemptConflict("Keep path is not valid UTF-8".to_string())
                        })?;
                        if affected.contains(logical) {
                            return Ok(None);
                        }
                        return Err(DaemonError::AttemptConflict(format!(
                            "Keep path '{}' has a non-directory or symlink parent",
                            workspace.join(relative).display()
                        )));
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) =>
                    {
                        return Ok(None)
                    }
                    Err(error) => {
                        return Err(DaemonError::Session(format!(
                            "checking Keep parent '{}': {error}",
                            current.display()
                        )))
                    }
                }
            }
        }
        Self::fingerprint_file(&workspace.join(relative)).await
    }

    fn require_review_storage(set: &crate::git::AttemptSet) -> Result<(), DaemonError> {
        if matches!(
            set.state,
            crate::git::AttemptSetState::Discarding
                | crate::git::AttemptSetState::Applying
                | crate::git::AttemptSetState::Applied
                | crate::git::AttemptSetState::TranscriptRecorded
        ) {
            return Err(DaemonError::AttemptConflict(
                "this attempt set is in decision cleanup; retry the active Keep or Discard action"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn attempt_cancel_key(session_id: &str, set_id: &str) -> String {
        format!("{session_id}\0{set_id}")
    }

    async fn attempt_cancelled(&self, session_id: &str, set_id: &str) -> bool {
        self.attempt_cancellations
            .lock()
            .await
            .contains(&Self::attempt_cancel_key(session_id, set_id))
    }

    async fn clear_attempt_cancellation(&self, session_id: &str, set_id: &str) {
        self.attempt_cancellations
            .lock()
            .await
            .remove(&Self::attempt_cancel_key(session_id, set_id));
    }

    fn attempt_state_is_interruptible(state: crate::git::AttemptSetState) -> bool {
        matches!(
            state,
            crate::git::AttemptSetState::Running | crate::git::AttemptSetState::Checking
        )
    }

    fn attempt_is_interrupt_target(
        expected_set_id: Option<&str>,
        current_set_id: &str,
        state: crate::git::AttemptSetState,
    ) -> bool {
        expected_set_id == Some(current_set_id) && Self::attempt_state_is_interruptible(state)
    }

    async fn wait_for_attempt_operation_after_interrupt<F>(
        waiter: F,
        timeout: Duration,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, DaemonError>
    where
        F: std::future::Future<Output = tokio::sync::OwnedMutexGuard<()>>,
    {
        tokio::time::timeout(timeout, waiter).await.map_err(|_| {
            DaemonError::AttemptConflict(format!(
                "attempt cancellation was requested, but another workspace operation did not release within {} seconds; retry the cleanup action",
                timeout.as_secs()
            ))
        })
    }

    async fn request_attempt_cancellation(
        &self,
        session_id: &str,
        set_id: &str,
    ) -> Result<(), DaemonError> {
        let set = self.require_attempt_set(session_id, set_id).await?;
        if !Self::attempt_state_is_interruptible(set.state) {
            return Err(DaemonError::AttemptConflict(
                "only live attempts or an in-progress Checks run can be interrupted; wait for the current attempt operation to finish"
                    .to_string(),
            ));
        }
        self.attempt_cancellations
            .lock()
            .await
            .insert(Self::attempt_cancel_key(session_id, set_id));
        // This phase deliberately runs before the workspace lease is acquired.
        // A live status/diff request can own that lease while blocked in a lane
        // container; killing the exact set's actors and containers is what lets
        // that request return. Clone/worktree deletion still happens only after
        // the lease is held by `discard_attempt_locked`.
        self.interrupt_attempt_runtime(session_id, &set).await
    }

    async fn lock_attempt_operation_for_cleanup(
        &self,
        session_id: &str,
        expected_set_id: Option<&str>,
    ) -> Result<(tokio::sync::OwnedMutexGuard<()>, bool), DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let mut waiter = Box::pin(operation.lock_owned());
        let mut cancellation_requested = false;
        loop {
            tokio::select! {
                guard = &mut waiter => return Ok((guard, cancellation_requested)),
                _ = tokio::time::sleep(Duration::from_millis(100)),
                    if expected_set_id.is_some() && !cancellation_requested =>
                {
                    let Some(set) = self.peek_current_attempt_set(session_id).await? else {
                        continue;
                    };
                    if !Self::attempt_is_interrupt_target(
                        expected_set_id,
                        &set.id,
                        set.state,
                    ) {
                        continue;
                    }
                    match self.request_attempt_cancellation(session_id, &set.id).await {
                        Ok(()) => {
                            cancellation_requested = true;
                            let guard = Self::wait_for_attempt_operation_after_interrupt(
                                &mut waiter,
                                ATTEMPT_OPERATION_RELEASE_TIMEOUT,
                            )
                            .await?;
                            return Ok((guard, cancellation_requested));
                        }
                        // Lane execution or Checks may have completed between
                        // the state read and cancellation. The lease will then
                        // become available normally; never cancel the next phase.
                        Err(DaemonError::AttemptConflict(_)) => {}
                        // Keep the marker on a partial teardown failure. It
                        // prevents Checks from restarting against a half-stopped
                        // set and makes a retry continue the same exact cleanup.
                        Err(error) => return Err(error),
                    }
                }
            }
        }
    }

    /// A session turn and an unresolved attempt set cannot safely own the same
    /// workspace at once. Besides producing an incoherent transcript, Keep
    /// would otherwise have to stop a possibly-running canonical session actor
    /// before it could append the chosen attempt. Waiting on the runtime lock
    /// also serializes this check with attempt-set setup and teardown.
    async fn require_no_unresolved_attempt(&self, session_id: &str) -> Result<(), DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let peers: Vec<String> = self
            .list_sessions()
            .await
            .into_iter()
            // Close preserves unresolved attempts for recovery, so closed peer
            // sessions still own the workspace until Keep or explicit Discard.
            .filter(|candidate| candidate.working_dir == session.working_dir)
            .map(|candidate| candidate.id)
            .collect();
        let active = self.active_attempts.lock().await;
        if let Some((owner, run)) = peers
            .iter()
            .find_map(|owner| active.get(owner).map(|run| (owner, run)))
        {
            return Err(DaemonError::AttemptConflict(format!(
                "attempt set '{}' in session '{}' owns this workspace; keep or discard it before continuing",
                run.set_id, owner
            )));
        }
        drop(active);
        for owner in peers {
            if let Some(set) = self.peek_current_attempt_set(&owner).await? {
                return Err(DaemonError::AttemptConflict(format!(
                    "attempt set '{}' in session '{}' owns this workspace; keep or discard it before continuing",
                    set.id, owner
                )));
            }
        }
        Ok(())
    }

    async fn create_attempt_worktrees(
        &self,
        session_id: &str,
        set: &crate::git::AttemptSet,
    ) -> Result<(), DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let root = sandbox.root().to_string_lossy().to_string();
        let base_ref = crate::attempts::base_ref(&set.id);
        let clone_branch = crate::attempts::clone_branch(&set.id);
        let clone_ref = format!("refs/heads/{clone_branch}");

        // Keep the snapshot reachable for the lifetime of the set, and expose
        // it briefly as a branch because `git clone --single-branch` only
        // advertises heads. Every lane then removes its origin and owns a fully
        // independent object database—no linked worktree/common Git directory
        // is writable from an attempt container.
        for (reference, action) in [
            (base_ref.as_str(), "protecting the attempt snapshot"),
            (
                clone_ref.as_str(),
                "advertising the attempt snapshot for clone",
            ),
        ] {
            Self::require_git_output(
                self.session_git(session_id, &["update-ref", reference, &set.base_sha])
                    .await?,
                action,
            )?;
        }

        let outcome = async {
            for variant in &set.lanes {
                let worktree = crate::attempts::worktree_path(
                    std::path::Path::new(&root),
                    session_id,
                    &set.id,
                    variant.index,
                )
                .to_string_lossy()
                .to_string();
                let clone = sandbox
                    .exec(
                        &[
                            "git",
                            "-c",
                            "safe.directory=*",
                            "clone",
                            "-q",
                            "--no-hardlinks",
                            "--dissociate",
                            "--no-checkout",
                            "--single-branch",
                            "--branch",
                            &clone_branch,
                            &root,
                            &worktree,
                        ],
                        Duration::from_secs(120),
                    )
                    .await
                    .map_err(|error| DaemonError::Session(error.to_string()))?;
                if !clone.ok() {
                    return Err(DaemonError::Session(format!(
                        "couldn't clone attempt {} of {}: {}",
                        variant.index + 1,
                        set.lanes.len(),
                        clone.stderr.trim()
                    )));
                }
                let lane_exclude = format!("{worktree}/.git/info/exclude");
                let exclude = sandbox
                    .exec_stdin(
                        &["tee", "-a", &lane_exclude],
                        "\n/.axo-variants/\n",
                        Duration::from_secs(10),
                    )
                    .await
                    .map_err(|error| DaemonError::Session(error.to_string()))?;
                if !exclude.ok() {
                    return Err(DaemonError::Session(format!(
                        "reserving attempt {} internal paths: {}",
                        variant.index + 1,
                        exclude.stderr.trim()
                    )));
                }
                let branch = crate::attempts::branch_name(&set.id, variant.index);
                Self::require_git_output(
                    self.session_git_at(
                        session_id,
                        &worktree,
                        &["checkout", "-q", "-b", &branch, &set.base_sha],
                    )
                    .await?,
                    &format!("checking out attempt {}", variant.index + 1),
                )?;
                Self::require_git_output(
                    self.session_git_at(session_id, &worktree, &["branch", "-D", &clone_branch])
                        .await?,
                    &format!("removing attempt {}'s staging branch", variant.index + 1),
                )?;
                Self::require_git_output(
                    self.session_git_at(session_id, &worktree, &["remote", "remove", "origin"])
                        .await?,
                    &format!(
                        "disconnecting attempt {} from the workspace",
                        variant.index + 1
                    ),
                )?;
            }
            Ok(())
        }
        .await;

        // Never leave the primary checkout advertising a lane-setup branch.
        let remove_clone_ref = self
            .session_git(session_id, &["update-ref", "-d", &clone_ref])
            .await?;
        if !remove_clone_ref.ok() && outcome.is_ok() {
            return Err(DaemonError::Session(format!(
                "removing the temporary attempt clone ref: {}",
                remove_clone_ref.stderr.trim()
            )));
        }
        outcome
    }

    /// Remove only the named set's worktrees, branches, and metadata. Never
    /// scan/delete every `.axo-variants` path: two sessions may anchor the same
    /// repository and must not be able to erase one another's attempts.
    async fn remove_attempt_worktrees(
        &self,
        session_id: &str,
        set: &crate::git::AttemptSet,
    ) -> Result<(), DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let root = sandbox.root().to_string_lossy();
        let mut failures = Vec::new();
        // Names come only from the validated session/set/index identity. Never
        // execute a serialized path or branch from agent-adjacent metadata.
        for lane in &set.lanes {
            let container = crate::attempts::container_id(session_id, &set.id, lane.index);
            if let Err(error) = SessionSandbox::remove_named(&container).await {
                failures.push(format!("stop attempt {} sandbox: {error}", lane.index + 1));
            }
        }
        if !failures.is_empty() {
            return Err(DaemonError::Session(format!(
                "attempt cleanup stopped before deleting files: {}",
                failures.join("; ")
            )));
        }

        let mut references = vec![
            crate::attempts::base_ref(&set.id),
            format!("refs/heads/{}", crate::attempts::clone_branch(&set.id)),
            crate::attempts::keep_preimage_ref(&set.id),
            crate::attempts::keep_postimage_ref(&set.id),
        ];
        references.extend(
            set.lanes
                .iter()
                .map(|lane| crate::attempts::checked_candidate_ref(&set.id, lane.index)),
        );
        for reference in references {
            match self
                .session_git(session_id, &["update-ref", "-d", &reference])
                .await
            {
                Ok(result) if result.ok() => {}
                Ok(result) => failures.push(format!(
                    "remove attempt ref '{reference}': {}",
                    result.stderr.trim()
                )),
                Err(error) => failures.push(format!("remove attempt ref '{reference}': {error}")),
            }
        }
        if !failures.is_empty() {
            return Err(DaemonError::Session(format!(
                "attempt cleanup stopped before deleting metadata: {}",
                failures.join("; ")
            )));
        }

        let set_root =
            crate::attempts::attempt_root(std::path::Path::new(root.as_ref()), session_id, &set.id)
                .to_string_lossy()
                .to_string();
        match sandbox
            .exec(&["rm", "-rf", &set_root], Duration::from_secs(15))
            .await
        {
            Ok(result) if result.ok() => {}
            Ok(result) => {
                failures.push(format!("remove attempt metadata: {}", result.stderr.trim()))
            }
            Err(error) => failures.push(format!("remove attempt metadata: {error}")),
        }
        if !failures.is_empty() {
            return Err(DaemonError::Session(format!(
                "attempt cleanup kept its current pointer for retry: {}",
                failures.join("; ")
            )));
        }
        let session_root =
            crate::attempts::session_attempts_root(std::path::Path::new(root.as_ref()), session_id)
                .to_string_lossy()
                .to_string();
        let current_path = format!("{session_root}/current.json");
        match Self::read_json_file::<crate::git::AttemptSet>(&sandbox, &current_path).await {
            Ok(Some(current)) if current.id == set.id => {
                match sandbox
                    .exec(&["rm", "-f", &current_path], Duration::from_secs(10))
                    .await
                {
                    Ok(result) if result.ok() => {}
                    Ok(result) => failures.push(format!(
                        "clear current attempt pointer: {}",
                        result.stderr.trim()
                    )),
                    Err(error) => failures.push(format!("clear current attempt pointer: {error}")),
                }
            }
            Ok(_) => {}
            Err(error) => failures.push(error.to_string()),
        }
        let _ = sandbox
            .exec(&["rmdir", &session_root], Duration::from_secs(10))
            .await;

        if failures.is_empty() {
            Ok(())
        } else {
            Err(DaemonError::Session(format!(
                "attempt cleanup was incomplete: {}",
                failures.join("; ")
            )))
        }
    }

    /// Backwards-compatible internal entry point used by older callers. New
    /// callers must send the set id to [`Self::discard_attempt`] so a stale
    /// request cannot delete a replacement set.
    pub async fn remove_variant_worktrees(&self, session_id: &str) -> Result<(), DaemonError> {
        let current_set = self
            .peek_current_attempt_set(session_id)
            .await?
            .map(|set| set.id);
        let (_operation, _cancellation_requested) = self
            .lock_attempt_operation_for_cleanup(session_id, current_set.as_deref())
            .await?;
        let result = self.remove_variant_worktrees_locked(session_id).await;
        if let Some(set_id) = current_set {
            self.clear_attempt_cancellation(session_id, &set_id).await;
        }
        result
    }

    async fn remove_variant_worktrees_locked(&self, session_id: &str) -> Result<(), DaemonError> {
        let Some(set) = self.current_attempt_set(session_id).await? else {
            return Ok(());
        };
        self.discard_attempt_locked(session_id, set).await
    }

    async fn quiesce_attempt_locked(&self, session_id: &str) -> Result<(), DaemonError> {
        let Some(set) = self.peek_current_attempt_set(session_id).await? else {
            return Ok(());
        };
        if set.lanes.is_empty() || set.lanes.len() > 100 {
            return Err(DaemonError::Session(
                "attempt metadata has an invalid way count; refusing sandbox teardown".to_string(),
            ));
        }
        let mut indexes: Vec<usize> = set.lanes.iter().map(|lane| lane.index).collect();
        indexes.sort_unstable();
        if indexes.iter().copied().ne(0..indexes.len()) {
            return Err(DaemonError::Session(
                "attempt metadata has invalid way indexes; refusing sandbox teardown".to_string(),
            ));
        }
        self.stop_attempt_runtime(session_id, &set).await?;
        // A daemon restart loses the in-memory runtime while intentionally
        // preserving deterministic containers. Remove every exact lane name
        // even when there was no runtime handle to join.
        for lane in &set.lanes {
            let container = crate::attempts::container_id(session_id, &set.id, lane.index);
            SessionSandbox::remove_named(&container)
                .await
                .map_err(|error| DaemonError::Session(error.to_string()))?;
        }
        Ok(())
    }

    /// The single agent each attempt runs. Multi-agent session transcripts do
    /// not yet have one canonical checkpoint to append a kept turn to, so the
    /// public Explore path rejects those modes instead of silently choosing an
    /// entry agent and losing the permanent chat on Keep.
    fn primary_session_agent(&self, session: &Session) -> Result<String, DaemonError> {
        match &session.mode {
            SessionMode::SingleAgent { agent_id } => Ok(agent_id.clone()),
            SessionMode::Custom { .. } | SessionMode::Lattice { .. } => {
                Err(DaemonError::AttemptConflict(
                    "Explore several ways currently requires a single-agent session".to_string(),
                ))
            }
        }
    }

    /// Spawn a fresh attempt actor jailed to one set-scoped worktree.
    ///
    /// Attempt actors are never reused: their scope includes the set id, and a
    /// registry collision is an error. Reuse here would revive an earlier
    /// attempt's checkpoint/core memory or return a dead actor after a failed
    /// lane.
    async fn variant_actor(
        &self,
        session: &Session,
        set_id: &str,
        agent_id: &str,
        variant: &crate::git::Variant,
    ) -> Result<
        (
            AgentId,
            ractor::ActorRef<axocoatl_actor::AgentMessage>,
            Arc<dyn Sandbox>,
        ),
        DaemonError,
    > {
        let scoped = crate::attempts::actor_scope(&session.id, set_id, variant.index, agent_id);
        let sid = AgentId::new(&scoped);
        if self.agent_registry.get(&sid).await.is_some() {
            return Err(DaemonError::AttemptConflict(format!(
                "attempt actor identity '{scoped}' already exists"
            )));
        }
        let agent_yaml = self
            .config
            .agents
            .iter()
            .find(|a| a.id == agent_id)
            .ok_or_else(|| {
                DaemonError::Session(format!("agent '{agent_id}' is not in the config"))
            })?
            .clone();
        let root = self.session_dir(&session.id).await?;
        let worktree = crate::attempts::worktree_path(
            std::path::Path::new(&root),
            &session.id,
            set_id,
            variant.index,
        );
        let container_id = crate::attempts::container_id(&session.id, set_id, variant.index);
        let sc = &self.config.sandbox;
        let policy = axocoatl_isolation::session_sandbox::SandboxPolicy {
            allow_post_create: sc.allow_post_create_command,
            allow_untrusted_image: sc.allow_untrusted_images,
            network: match sc.network.as_str() {
                "none" => axocoatl_isolation::session_sandbox::SandboxNetwork::None,
                _ => axocoatl_isolation::session_sandbox::SandboxNetwork::Bridge,
            },
            require_resource_limits: sc.require_resource_limits,
        };
        // This is a real filesystem boundary, not `with_root` on the primary
        // session container. Only the independent lane clone is mounted, so an
        // arbitrary shell command cannot reach the workspace, sibling lanes,
        // their metadata, or the primary repository's Git directory.
        let sandbox: Arc<dyn Sandbox> = Arc::new(
            SessionSandbox::start(
                &container_id,
                &worktree,
                session.image.as_deref(),
                &[],
                &session.post_create_commands,
                &policy,
            )
            .await
            .map_err(|error| {
                DaemonError::Session(format!(
                    "starting isolated sandbox for attempt {}: {error}",
                    variant.index + 1
                ))
            })?,
        );
        let executor = self
            .build_session_executor(session, sandbox.clone(), false)
            .await;
        // Context path = the in-sandbox worktree (where the tools operate);
        // project instructions still come from the primary session's host repo.
        let actor = match self
            .spawn_session_agent(
                session,
                &agent_yaml,
                &scoped,
                Arc::new(executor),
                &worktree,
                false,
            )
            .await
        {
            Ok(actor) => actor,
            Err(error) => {
                sandbox.stop().await;
                return Err(error);
            }
        };
        Ok((sid, actor, sandbox))
    }

    /// Run one attempt per entry in `lanes`, in parallel — each in an independent
    /// clone/container, each streamed to the bus under run key `{session}#{i}`.
    /// Returns the durable attempt set immediately; lane tasks remain owned by
    /// `active_attempts` until Keep or Discard joins them.
    ///
    /// Lanes are **heterogeneous**: each carries its own model override, so an
    /// expensive model can plan once and several cheaper ones execute that plan
    /// concurrently — and running the same task against different models is
    /// itself a quality strategy. A lane with no model uses the agent's own.
    pub async fn execute_session_variants(
        &self,
        session_id: &str,
        task: &str,
        instruction: &str,
        lanes: &[crate::git::LaneConfig],
    ) -> Result<crate::git::AttemptSet, DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        // A generous ceiling — the user configures the count. Beyond a handful
        // it gets slow on local models, but we let them push it and degrade
        // gracefully (a failed lane errors on its own; a failed worktree set
        // rolls back) rather than capping low.
        const MAX_VARIANTS: usize = 100;
        let n = lanes.len();
        if !(1..=MAX_VARIANTS).contains(&n) {
            return Err(DaemonError::Session(format!(
                "variant count must be between 1 and {MAX_VARIANTS}"
            )));
        }
        if self.config.sandbox.backend == "e2b" {
            return Err(DaemonError::Session(
                "parallel attempts currently require the local Podman backend so every way can receive its own filesystem boundary"
                    .to_string(),
            ));
        }
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        if !matches!(session.mode, SessionMode::SingleAgent { .. }) {
            return Err(DaemonError::AttemptConflict(
                "Explore several ways currently requires a single-agent session so the kept task and answer remain in the permanent chat"
                    .to_string(),
            ));
        }
        let attempt_history: Vec<axocoatl_core::ChatMessage> = self
            .session_messages(session_id)
            .await?
            .into_iter()
            .map(|message| axocoatl_core::ChatMessage {
                role: message.role,
                content: axocoatl_core::MessageContent::Text(message.content),
                name: message.name,
                tool_calls: message
                    .tool_calls
                    .into_iter()
                    .map(|call| axocoatl_core::ToolCall {
                        id: call.id,
                        name: call.name,
                        arguments: serde_json::from_str(&call.arguments_json)
                            .unwrap_or(serde_json::Value::Null),
                    })
                    .collect(),
                tool_call_id: message.tool_call_id,
            })
            .collect();
        let default_agent = self.primary_session_agent(&session)?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let set_id = uuid::Uuid::new_v4().to_string();
        let root = sandbox.root().to_path_buf();

        // Resolve every lane now. Persisting `None` for defaults makes costs and
        // labels unknowable after reload; the set records what actually ran.
        let mut variants = Vec::with_capacity(n);
        for (index, lane) in lanes.iter().enumerate() {
            let lane_agent = lane.agent.as_deref().unwrap_or(&default_agent);
            let agent = self
                .config
                .agents
                .iter()
                .find(|candidate| candidate.id == lane_agent)
                .ok_or_else(|| {
                    DaemonError::Session(format!("lane agent '{lane_agent}' is not in the config"))
                })?;
            let model = lane
                .model
                .as_deref()
                .filter(|model| !model.trim().is_empty())
                .unwrap_or(&agent.model)
                .to_string();
            variants.push(crate::git::Variant {
                index,
                branch: crate::attempts::branch_name(&set_id, index),
                worktree: crate::attempts::worktree_path(&root, session_id, &set_id, index)
                    .to_string_lossy()
                    .to_string(),
                model: Some(model),
                agent: Some(lane_agent.to_string()),
                provider: Some(agent.provider.clone()),
            });
        }

        self.require_no_unresolved_attempt(session_id).await?;
        if self
            .active_runs
            .lock()
            .is_ok_and(|runs| runs.contains_key(session_id))
        {
            return Err(DaemonError::AttemptConflict(
                "this session is still running a turn; wait for it to finish before exploring several ways"
                    .to_string(),
            ));
        }
        let (base_sha, base_tree) = self.snapshot_attempt_base(session_id, &set_id).await?;
        Self::require_git_output(
            self.session_git(
                session_id,
                &["update-ref", &crate::attempts::base_ref(&set_id), &base_sha],
            )
            .await?,
            "protecting the attempt snapshot",
        )?;
        let mut set = crate::git::AttemptSet {
            id: set_id.clone(),
            session_id: session_id.to_string(),
            task: if task.trim().is_empty() {
                instruction.to_string()
            } else {
                task.to_string()
            },
            instruction: instruction.to_string(),
            base_sha,
            base_tree,
            state: crate::git::AttemptSetState::Preparing,
            kept_index: None,
            created_at: unix_now(),
            lanes: variants,
        };
        if let Err(error) = self.persist_attempt_set(&sandbox, &set).await {
            return Err(self
                .rollback_attempt_setup(&sandbox, session_id, &mut set, error)
                .await);
        }
        if let Err(error) = self.create_attempt_worktrees(session_id, &set).await {
            return Err(self
                .rollback_attempt_setup(&sandbox, session_id, &mut set, error)
                .await);
        }

        let attempt_root = crate::attempts::attempt_root(&root, session_id, &set_id)
            .to_string_lossy()
            .to_string();
        for lane in &set.lanes {
            if let Err(error) = Self::record_lane_state(
                &sandbox,
                &attempt_root,
                crate::git::AttemptLaneStatus {
                    index: lane.index,
                    state: crate::git::AttemptLaneState::Queued,
                    error: None,
                    started_at: None,
                    finished_at: None,
                },
            )
            .await
            {
                return Err(self
                    .rollback_attempt_setup(&sandbox, session_id, &mut set, error)
                    .await);
            }
        }
        set.state = crate::git::AttemptSetState::Running;
        if let Err(error) = self.persist_attempt_set(&sandbox, &set).await {
            return Err(self
                .rollback_attempt_setup(&sandbox, session_id, &mut set, error)
                .await);
        }

        // Build every variant's actor *before* spawning any lane. If one fails
        // (e.g. a missing agent), nothing is running yet, so we can tear the
        // whole worktree set down and surface the error cleanly — rather than
        // returning with half the lanes streaming against orphaned worktrees.
        let mut runtime = ActiveAttemptRun::new(&set_id);
        let mut prepared = Vec::with_capacity(set.lanes.len());
        for variant in &set.lanes {
            let lane_agent = variant.agent.as_deref().unwrap_or(&default_agent);
            match self
                .variant_actor(&session, &set_id, lane_agent, variant)
                .await
            {
                Ok((actor_id, actor, lane_sandbox)) => {
                    runtime.actors.push((actor_id, actor.clone()));
                    runtime.sandboxes.push(lane_sandbox);
                    prepared.push((variant.clone(), actor));
                }
                Err(e) => {
                    for (actor_id, actor) in &runtime.actors {
                        let _ = actor.kill_and_wait(Some(Duration::from_secs(10))).await;
                        self.agent_registry.remove(actor_id).await;
                        self.remove_attempt_memory(actor_id).await;
                    }
                    for lane_sandbox in &runtime.sandboxes {
                        lane_sandbox.stop().await;
                    }
                    return Err(self
                        .rollback_attempt_setup(&sandbox, session_id, &mut set, e)
                        .await);
                }
            }
        }

        for (variant, actor) in prepared {
            let index = variant.index;
            let run_id = crate::attempts::run_id(&session.id, index);
            let bus = self.stream_bus.clone();
            let rid = run_id.clone();
            let aid = variant
                .agent
                .clone()
                .unwrap_or_else(|| default_agent.clone());
            let inp = instruction.to_string();
            let mo = variant.model.clone();
            let usage_sandbox = sandbox.clone();
            let usage_key = session.id.clone();
            let lane_model = mo.clone();
            let lane_provider = variant.provider.clone().unwrap_or_default();
            let configured_price = lane_model
                .as_deref()
                .and_then(|model| self.config.pricing.get(model))
                .map(|price| crate::git::ModelPrice {
                    input_per_mtok: price.input_per_mtok,
                    output_per_mtok: price.output_per_mtok,
                });
            let cost_known = lane_provider == "ollama" || configured_price.is_some();
            let price = if lane_provider == "ollama" {
                crate::git::ModelPrice::default()
            } else {
                configured_price.unwrap_or_default()
            };
            let lane_root = attempt_root.clone();
            let lane_history = attempt_history.clone();
            let lane_attempt_set_id = set_id.clone();
            let trace: Arc<StdMutex<Vec<crate::trajectory::Action>>> =
                Arc::new(StdMutex::new(Vec::new()));
            let handle = tokio::spawn(async move {
                let started = std::time::Instant::now();
                let started_at = unix_now();
                if let Err(error) = Self::record_lane_state(
                    &usage_sandbox,
                    &lane_root,
                    crate::git::AttemptLaneStatus {
                        index,
                        state: crate::git::AttemptLaneState::Running,
                        error: None,
                        started_at: Some(started_at),
                        finished_at: None,
                    },
                )
                .await
                {
                    let message = format!("could not persist attempt start: {error}");
                    let _ = Self::record_lane_state(
                        &usage_sandbox,
                        &lane_root,
                        crate::git::AttemptLaneStatus {
                            index,
                            state: crate::git::AttemptLaneState::Failed,
                            error: Some(message.clone()),
                            started_at: Some(started_at),
                            finished_at: Some(unix_now()),
                        },
                    )
                    .await;
                    let _ = bus.send(crate::stream::StreamFrame::SessionError {
                        session: rid,
                        error: message,
                    });
                    return;
                }
                // Announce the lane's identity once, so a viewer can attribute
                // every later frame to the right lane and model without parsing
                // the run key.
                let _ = bus.send(crate::stream::StreamFrame::LaneStarted {
                    run: rid.clone(),
                    attempt_set_id: lane_attempt_set_id.clone(),
                    session: usage_key.clone(),
                    index,
                    model: lane_model.clone(),
                    agent: aid.clone(),
                });
                let _ = bus.send(crate::stream::StreamFrame::SessionStart {
                    session: rid.clone(),
                });
                let outcome = Self::stream_agent_run(
                    bus.clone(),
                    actor,
                    rid.clone(),
                    aid,
                    inp,
                    StreamAgentRunOptions {
                        model_override: mo,
                        trace: Some(trace.clone()),
                        supplied_history: Some(lane_history),
                    },
                )
                .await;
                let duration_ms = started.elapsed().as_millis() as u64;
                // Written on both paths: a lane that errored still took a route,
                // and *where* it went wrong is the most useful trajectory there
                // is. Persisting only successes would lose exactly that.
                let trajectory_result = Self::record_lane_trajectory(
                    &usage_sandbox,
                    &lane_root,
                    crate::trajectory::Trajectory {
                        index,
                        actions: trace.lock().map(|s| s.clone()).unwrap_or_default(),
                    },
                )
                .await;
                match outcome {
                    Ok(out) => {
                        let mut persistence_errors = Vec::new();
                        if let Err(error) = trajectory_result {
                            persistence_errors.push(error.to_string());
                        }
                        let cost = price.cost(
                            out.token_usage.input_tokens as u64,
                            out.token_usage.output_tokens as u64,
                        );
                        // Record what this lane spent, so the run's economics
                        // outlive the stream that reported them.
                        if let Err(error) = Self::record_lane_usage(
                            &usage_sandbox,
                            &lane_root,
                            crate::git::LaneUsage {
                                index,
                                model: lane_model,
                                input_tokens: out.token_usage.input_tokens as u64,
                                output_tokens: out.token_usage.output_tokens as u64,
                                token_usage_known: true,
                                cost_usd: cost,
                                cost_known,
                                duration_ms,
                            },
                        )
                        .await
                        {
                            persistence_errors.push(error.to_string());
                        }
                        if let Err(error) = Self::record_lane_output(
                            &usage_sandbox,
                            &lane_root,
                            index,
                            &out.content,
                        )
                        .await
                        {
                            persistence_errors.push(error.to_string());
                        }
                        let persistence_error =
                            (!persistence_errors.is_empty()).then(|| persistence_errors.join("; "));
                        let final_state = if persistence_error.is_some() {
                            crate::git::AttemptLaneState::Failed
                        } else {
                            crate::git::AttemptLaneState::Completed
                        };
                        let state_result = Self::record_lane_state(
                            &usage_sandbox,
                            &lane_root,
                            crate::git::AttemptLaneStatus {
                                index,
                                state: final_state,
                                error: persistence_error.clone(),
                                started_at: Some(started_at),
                                finished_at: Some(unix_now()),
                            },
                        )
                        .await;
                        match (persistence_error, state_result) {
                            (None, Ok(())) => {
                                let _ = bus.send(crate::stream::StreamFrame::SessionDone {
                                    session: rid,
                                    input_tokens: out.token_usage.input_tokens as u64,
                                    output_tokens: out.token_usage.output_tokens as u64,
                                });
                            }
                            (error, state_result) => {
                                let mut message = error.unwrap_or_else(|| {
                                    "attempt result metadata could not be persisted".to_string()
                                });
                                if let Err(error) = state_result {
                                    message.push_str(&format!("; final state: {error}"));
                                }
                                let _ = bus.send(crate::stream::StreamFrame::SessionError {
                                    session: rid,
                                    error: message,
                                });
                            }
                        }
                    }
                    Err(e) => {
                        let mut errors = vec![e.to_string()];
                        if let Err(error) = trajectory_result {
                            errors.push(format!("trajectory persistence: {error}"));
                        }
                        // A lane that failed still ran for a while and is still
                        // one of the things being compared. Record it with zero
                        // tokens — unknown, not free — so the scoreboard can say
                        // how long it burned instead of showing a blank column.
                        if let Err(error) = Self::record_lane_usage(
                            &usage_sandbox,
                            &lane_root,
                            crate::git::LaneUsage {
                                index,
                                model: lane_model,
                                input_tokens: 0,
                                output_tokens: 0,
                                token_usage_known: false,
                                cost_usd: 0.0,
                                cost_known: lane_provider == "ollama",
                                duration_ms,
                            },
                        )
                        .await
                        {
                            errors.push(format!("usage persistence: {error}"));
                        }
                        let message = errors.join("; ");
                        if let Err(error) = Self::record_lane_state(
                            &usage_sandbox,
                            &lane_root,
                            crate::git::AttemptLaneStatus {
                                index,
                                state: crate::git::AttemptLaneState::Failed,
                                error: Some(message.clone()),
                                started_at: Some(started_at),
                                finished_at: Some(unix_now()),
                            },
                        )
                        .await
                        {
                            tracing::error!(attempt = index, error = %error, "failed to persist terminal attempt state");
                        }
                        let _ = bus.send(crate::stream::StreamFrame::SessionError {
                            session: rid,
                            error: message,
                        });
                    }
                }
            });
            runtime.tasks.push(handle);
        }
        self.active_attempts
            .lock()
            .await
            .insert(session_id.to_string(), runtime);
        let _ = self.session_store.lock().await.touch(session_id);
        Ok(set)
    }

    /// Record what one lane spent, in that lane's own file.
    ///
    /// Static because it is called from the lane's spawned task, which outlives
    /// any borrow of the daemon.
    async fn record_lane_usage(
        sandbox: &Arc<dyn Sandbox>,
        attempt_root: &str,
        usage: crate::git::LaneUsage,
    ) -> Result<(), DaemonError> {
        let path = format!("{attempt_root}/usage-{}.json", usage.index);
        Self::write_json_file(sandbox, &path, &usage).await
    }

    /// Persist the latest lifecycle fact for one lane. Each lane owns a file,
    /// so concurrent completions cannot overwrite one another.
    async fn record_lane_state(
        sandbox: &Arc<dyn Sandbox>,
        attempt_root: &str,
        state: crate::git::AttemptLaneStatus,
    ) -> Result<(), DaemonError> {
        let path = format!("{attempt_root}/state-{}.json", state.index);
        Self::write_json_file(sandbox, &path, &state).await
    }

    /// Persist the lane's final natural-language answer. Git is the source of
    /// truth for code, but the answer is needed when Keep reconnects that chosen
    /// turn to the session's permanent chat spine.
    async fn record_lane_output(
        sandbox: &Arc<dyn Sandbox>,
        attempt_root: &str,
        index: usize,
        content: &str,
    ) -> Result<(), DaemonError> {
        let path = format!("{attempt_root}/output-{index}.json");
        let value = serde_json::json!({ "index": index, "content": content });
        Self::write_json_file(sandbox, &path, &value).await
    }

    /// Record one lane's normalised trajectory, in that lane's own file.
    ///
    /// Same shape and same reasoning as [`Self::record_lane_usage`]: one file per
    /// lane because lanes finish concurrently, stored beside the worktrees so the
    /// comparison outlives the process that produced it.
    async fn record_lane_trajectory(
        sandbox: &Arc<dyn Sandbox>,
        attempt_root: &str,
        trajectory: crate::trajectory::Trajectory,
    ) -> Result<(), DaemonError> {
        let path = format!("{attempt_root}/trace-{}.json", trajectory.index);
        Self::write_json_file(sandbox, &path, &trajectory).await
    }

    /// Write one of the comparison's result files beside the worktrees.
    ///
    /// `.axo-variants/` is git-excluded and already holds the lane roster, so
    /// the answers live with the thing they describe: they survive a reload, a
    /// rebuild and a daemon restart, and they are deleted by the same teardown
    /// that removes the lanes. Best-effort — a failed write must never fail the
    /// verify or judge that produced the result.
    async fn write_variant_meta<T: serde::Serialize>(
        &self,
        session_id: &str,
        set_id: &str,
        name: &str,
        value: &T,
    ) -> Result<(), DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let path = crate::attempts::attempt_root(sandbox.root(), session_id, set_id)
            .join(name)
            .to_string_lossy()
            .to_string();
        Self::write_json_file(&sandbox, &path, value).await
    }

    /// Read back what `write_variant_meta` stored. `None` for anything absent —
    /// a comparison that has not been verified or judged yet is normal.
    async fn read_variant_meta<T: serde::de::DeserializeOwned>(
        &self,
        sandbox: &Arc<dyn Sandbox>,
        attempt_root: &str,
        name: &str,
    ) -> Result<Option<T>, DaemonError> {
        let path = format!("{attempt_root}/{name}");
        Self::read_json_file(sandbox, &path).await
    }

    async fn attempt_lane_states(
        &self,
        sandbox: &Arc<dyn Sandbox>,
        set: &crate::git::AttemptSet,
    ) -> Result<Vec<crate::git::AttemptLaneStatus>, DaemonError> {
        let root = crate::attempts::attempt_root(sandbox.root(), &set.session_id, &set.id)
            .to_string_lossy()
            .to_string();
        let is_live = self
            .active_attempts
            .lock()
            .await
            .get(&set.session_id)
            .is_some_and(|run| run.set_id == set.id);
        let mut states = Vec::with_capacity(set.lanes.len());
        for lane in &set.lanes {
            let mut state = self
                .read_variant_meta::<crate::git::AttemptLaneStatus>(
                    sandbox,
                    &root,
                    &format!("state-{}.json", lane.index),
                )
                .await?
                .unwrap_or(crate::git::AttemptLaneStatus {
                    index: lane.index,
                    state: if is_live {
                        crate::git::AttemptLaneState::Queued
                    } else {
                        crate::git::AttemptLaneState::Interrupted
                    },
                    error: None,
                    started_at: None,
                    finished_at: None,
                });
            if !is_live
                && matches!(
                    state.state,
                    crate::git::AttemptLaneState::Queued | crate::git::AttemptLaneState::Running
                )
            {
                state.state = crate::git::AttemptLaneState::Interrupted;
                state.error.get_or_insert_with(|| {
                    "the daemon restarted before this attempt finished".to_string()
                });
                state.finished_at.get_or_insert_with(unix_now);
            }
            states.push(state);
        }
        states.sort_by_key(|state| state.index);
        Ok(states)
    }

    async fn attempt_lane_states_host(
        &self,
        attempt_root: &std::path::Path,
        set: &crate::git::AttemptSet,
    ) -> Result<Vec<crate::git::AttemptLaneStatus>, DaemonError> {
        let is_live = self
            .active_attempts
            .lock()
            .await
            .get(&set.session_id)
            .is_some_and(|run| run.set_id == set.id);
        let mut states = Vec::with_capacity(set.lanes.len());
        for lane in &set.lanes {
            let mut state = Self::read_host_json_file::<crate::git::AttemptLaneStatus>(
                &attempt_root.join(format!("state-{}.json", lane.index)),
            )
            .await?
            .unwrap_or(crate::git::AttemptLaneStatus {
                index: lane.index,
                state: if is_live {
                    crate::git::AttemptLaneState::Queued
                } else {
                    crate::git::AttemptLaneState::Interrupted
                },
                error: None,
                started_at: None,
                finished_at: None,
            });
            if !is_live
                && matches!(
                    state.state,
                    crate::git::AttemptLaneState::Queued | crate::git::AttemptLaneState::Running
                )
            {
                state.state = crate::git::AttemptLaneState::Interrupted;
                state.error.get_or_insert_with(|| {
                    "the daemon restarted before this attempt finished".to_string()
                });
                state.finished_at.get_or_insert_with(unix_now);
            }
            states.push(state);
        }
        states.sort_by_key(|state| state.index);
        Ok(states)
    }

    async fn read_lane_usage_host(
        attempt_root: &std::path::Path,
        indexes: &[usize],
    ) -> Result<Vec<crate::git::LaneUsage>, DaemonError> {
        let mut usage = Vec::new();
        for index in indexes {
            if let Some(record) = Self::read_host_json_file::<crate::git::LaneUsage>(
                &attempt_root.join(format!("usage-{index}.json")),
            )
            .await?
            {
                usage.push(record);
            }
        }
        usage.sort_by_key(|record| record.index);
        Ok(usage)
    }

    async fn read_lane_outputs_host(
        attempt_root: &std::path::Path,
        indexes: &[usize],
    ) -> Result<Vec<crate::git::AttemptLaneOutput>, DaemonError> {
        let mut outputs = Vec::new();
        for index in indexes {
            if let Some(output) = Self::read_host_json_file::<crate::git::AttemptLaneOutput>(
                &attempt_root.join(format!("output-{index}.json")),
            )
            .await?
            {
                if output.index != *index {
                    return Err(DaemonError::Session(format!(
                        "attempt output {} claims lane {}",
                        attempt_root.join(format!("output-{index}.json")).display(),
                        output.index
                    )));
                }
                outputs.push(output);
            }
        }
        outputs.sort_by_key(|output| output.index);
        Ok(outputs)
    }

    fn attempt_lanes_terminal(states: &[crate::git::AttemptLaneStatus]) -> bool {
        !states.is_empty()
            && states.iter().all(|state| {
                matches!(
                    state.state,
                    crate::git::AttemptLaneState::Completed
                        | crate::git::AttemptLaneState::Failed
                        | crate::git::AttemptLaneState::Cancelled
                        | crate::git::AttemptLaneState::Interrupted
                )
            })
    }

    async fn require_terminal_attempt(
        &self,
        session_id: &str,
        set_id: &str,
    ) -> Result<(crate::git::AttemptSet, Vec<crate::git::AttemptLaneStatus>), DaemonError> {
        let set = self.require_attempt_set(session_id, set_id).await?;
        if set.state == crate::git::AttemptSetState::Discarding {
            return Err(DaemonError::AttemptConflict(
                "this attempt set is being discarded; retry Discard to finish cleanup".to_string(),
            ));
        }
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let states = self.attempt_lane_states(&sandbox, &set).await?;
        if !Self::attempt_lanes_terminal(&states) {
            return Err(DaemonError::AttemptConflict(
                "attempts are still running; wait for every way to finish before reviewing"
                    .to_string(),
            ));
        }
        Ok((set, states))
    }

    /// The lanes' trajectories, aligned against `baseline`.
    ///
    /// `baseline` names the lane every other lane is read against — the same
    /// choice the scoreboard exposes, so re-basing there re-bases this. An
    /// unknown baseline is an error rather than a silent fallback to lane 0: the
    /// whole reading of the table depends on which column is the reference, and
    /// quietly answering a different question than the one asked is worse than
    /// refusing.
    pub async fn variants_trajectories(
        &self,
        session_id: &str,
        set_id: &str,
        baseline: usize,
    ) -> Result<crate::trajectory::Alignment, DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        let set = self.require_attempt_set(session_id, set_id).await?;
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let indexes: Vec<usize> = set.lanes.iter().map(|lane| lane.index).collect();
        if indexes.is_empty() {
            return Ok(crate::trajectory::Alignment::default());
        }
        if !indexes.contains(&baseline) {
            return Err(DaemonError::Session(format!(
                "lane {baseline} is not part of this comparison"
            )));
        }
        // Baseline first, then the rest in lane order — the column order the
        // scoreboard uses, so the two views cannot disagree about which is which.
        let order = std::iter::once(baseline).chain(indexes.into_iter().filter(|i| *i != baseline));
        let attempt_root = crate::attempts::attempt_root(sandbox.root(), session_id, set_id)
            .to_string_lossy()
            .to_string();
        let mut trajectories = Vec::new();
        for i in order {
            trajectories.push(
                self.read_variant_meta::<crate::trajectory::Trajectory>(
                    &sandbox,
                    &attempt_root,
                    &format!("trace-{i}.json"),
                )
                .await?
                // A lane with no recorded trajectory is a lane that took no
                // steps, which is a real and reportable outcome — it must still
                // hold a column.
                .unwrap_or(crate::trajectory::Trajectory {
                    index: i,
                    actions: Vec::new(),
                }),
            );
        }
        Ok(crate::trajectory::align(&trajectories))
    }

    /// Everything known about the session's current comparison, in one read.
    ///
    /// Identity, lifecycle, answers, verdicts, spend and ranking all come from
    /// durable metadata. This read deliberately stays on the host: reconnecting
    /// to a soft-closed session must not start Podman or rerun postCreate. A
    /// session with no variants returns an empty set rather than an error —
    /// "nothing to compare" is a normal state, not a failure.
    pub async fn run_results(
        &self,
        session_id: &str,
    ) -> Result<crate::git::RunResults, DaemonError> {
        // Attempt metadata is published with write+rename, so readers do not
        // need the workspace mutation lease. In particular, a long Checks run
        // must remain observable (including its partial verdicts) and
        // cancellable from a reconnected browser.
        let Some(mut set) = self.peek_current_attempt_set(session_id).await? else {
            return Ok(crate::git::RunResults::default());
        };
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        Self::validate_attempt_set_identity(&session.working_dir, &set)?;
        let attempt_root = crate::attempts::attempt_root(&session.working_dir, session_id, &set.id);
        let verdicts: Vec<crate::git::LaneVerdict> =
            Self::read_host_json_file(&attempt_root.join("verdicts.json"))
                .await?
                .unwrap_or_default();
        // Checks invalidate any earlier ranking by atomically publishing JSON
        // `null`; reading as Option<Judgment> accepts both that tombstone and a
        // later judgment object, while the outer Option represents no file yet.
        let judgment: Option<crate::git::Judgment> = Self::read_host_json_file::<
            Option<crate::git::Judgment>,
        >(&attempt_root.join("judgment.json"))
        .await?
        .flatten();
        let lane_states = self.attempt_lane_states_host(&attempt_root, &set).await?;
        if !matches!(
            set.state,
            crate::git::AttemptSetState::Checking
                | crate::git::AttemptSetState::Discarding
                | crate::git::AttemptSetState::Applying
                | crate::git::AttemptSetState::Applied
                | crate::git::AttemptSetState::TranscriptRecorded
        ) {
            set.state =
                crate::git::derive_attempt_set_state(&lane_states, &verdicts, judgment.as_ref());
        }
        let indexes: Vec<usize> = set.lanes.iter().map(|lane| lane.index).collect();
        let usage = Self::read_lane_usage_host(&attempt_root, &indexes).await?;
        let outputs = Self::read_lane_outputs_host(&attempt_root, &indexes).await?;
        Ok(crate::git::RunResults {
            attempt_set: Some(set.clone()),
            lanes: set.lanes,
            lane_states,
            verdicts,
            usage,
            outputs,
            judgment,
        })
    }

    /// The working-tree status of every live variant worktree — what each
    /// Compare lane shows as its changes.
    /// Run `check` inside every variant lane's worktree and report which lanes
    /// survive — the fan-in half of a variants run.
    ///
    /// Generating N candidates is easy; the cost is that a human then has N
    /// diffs to read. Running the repository's own checks (tests, build,
    /// typecheck) in each lane eliminates the failures first, so only survivors
    /// reach review. `check` is the project's command, run through `sh` in the
    /// lane's worktree.
    ///
    /// Lanes are checked **sequentially**: unlike the agent runs (which are
    /// IO-bound on the model and genuinely parallel), check commands are
    /// CPU-bound, and N test suites at once contend for the same cores.
    pub async fn verify_variants(
        &self,
        session_id: &str,
        set_id: &str,
        check: &str,
    ) -> Result<Vec<crate::git::LaneVerdict>, DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        /// Test suites are slow; give a lane's check real room before killing it.
        const CHECK_TIMEOUT: Duration = Duration::from_secs(900);

        if self.attempt_cancelled(session_id, set_id).await {
            return Err(DaemonError::AttemptConflict(
                "Checks cancellation is pending; retry Discard or close the session".to_string(),
            ));
        }

        // An empty request means "use the project's own command", which lives on
        // the session. Only when neither exists is this actually unanswerable —
        // guessing here would rule attempts out with a command the project never
        // runs.
        let owned;
        let check = if check.trim().is_empty() {
            owned = self
                .get_session(session_id)
                .await
                .and_then(|s| s.check_command)
                .ok_or_else(|| {
                    DaemonError::Session(
                        "no check command: set one on the session, or pass one".to_string(),
                    )
                })?;
            owned.as_str()
        } else {
            check
        };
        let (mut set, states) = self.require_terminal_attempt(session_id, set_id).await?;
        if matches!(
            set.state,
            crate::git::AttemptSetState::Applying
                | crate::git::AttemptSetState::Applied
                | crate::git::AttemptSetState::TranscriptRecorded
        ) {
            return Err(DaemonError::AttemptConflict(
                "Keep is already in progress; retry Keep to finish it".to_string(),
            ));
        }
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        set.state = crate::git::AttemptSetState::Checking;
        self.persist_attempt_set(&sandbox, &set).await?;

        // Publish the new generation before any slow command. A cancelled or
        // restarted request then exposes partial current verdicts under the
        // explicit Checking state, never an old ranking over new results.
        let mut verdicts = Vec::new();
        let mut checked_trees: Vec<StoredCheckedTree> = Vec::new();
        self.write_variant_meta(
            session_id,
            set_id,
            "judgment.json",
            &Option::<crate::git::Judgment>::None,
        )
        .await?;
        self.write_variant_meta(session_id, set_id, "verdicts.json", &verdicts)
            .await?;
        self.write_variant_meta(session_id, set_id, "checked-trees.json", &checked_trees)
            .await?;
        for lane in &set.lanes {
            let reference = crate::attempts::checked_candidate_ref(set_id, lane.index);
            Self::require_git_output(
                self.session_git(session_id, &["update-ref", "-d", &reference])
                    .await?,
                &format!("clearing attempt {} previous checked tree", lane.index + 1),
            )?;
        }
        self.stop_attempt_runtime(session_id, &set).await?;

        for lane in &set.lanes {
            if self.attempt_cancelled(session_id, set_id).await {
                return Err(DaemonError::AttemptConflict(
                    "Checks were cancelled; retry Discard or run Checks again".to_string(),
                ));
            }
            let index = lane.index;
            let state = states.iter().find(|state| state.index == index);
            if !matches!(
                state.map(|state| state.state),
                Some(crate::git::AttemptLaneState::Completed)
            ) {
                let output = state
                    .and_then(|state| state.error.clone())
                    .unwrap_or_else(|| "this attempt did not complete".to_string());
                verdicts.push(crate::git::LaneVerdict {
                    index,
                    passed: false,
                    exit_code: -1,
                    output,
                    changed_files: 0,
                    touched_tests: Vec::new(),
                    patch_sha256: None,
                });
                self.write_variant_meta(session_id, set_id, "verdicts.json", &verdicts)
                    .await?;
                continue;
            }
            let container_id = crate::attempts::container_id(session_id, set_id, lane.index);
            // A previous cancelled Checks request may have left its command
            // container alive. Remove it before creating this generation.
            SessionSandbox::remove_named(&container_id)
                .await
                .map_err(|error| DaemonError::Session(error.to_string()))?;
            let lane_sandbox = self.attempt_sandbox(&session, set_id, lane.index).await?;
            let check_args = ["sh", "-c", check];
            let check_result = tokio::select! {
                result = lane_sandbox.exec(&check_args, CHECK_TIMEOUT) => Some(result),
                _ = async {
                    loop {
                        if self.attempt_cancelled(session_id, set_id).await {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                } => None,
            };
            // Freeze the exact filesystem state the check left behind and kill
            // any watcher/background process before patch capture.
            SessionSandbox::remove_named(&container_id)
                .await
                .map_err(|error| DaemonError::Session(error.to_string()))?;
            let Some(check_result) = check_result else {
                return Err(DaemonError::AttemptConflict(
                    "Checks were cancelled; retry Discard or run Checks again".to_string(),
                ));
            };
            let r = match check_result {
                Ok(result) => result,
                Err(error) => {
                    verdicts.push(crate::git::LaneVerdict {
                        index,
                        passed: false,
                        exit_code: -1,
                        output: format!("running check in attempt {}: {error}", index + 1),
                        changed_files: 0,
                        touched_tests: Vec::new(),
                        patch_sha256: None,
                    });
                    self.write_variant_meta(session_id, set_id, "verdicts.json", &verdicts)
                        .await?;
                    continue;
                }
            };
            let combined = if r.stderr.trim().is_empty() {
                r.stdout
            } else {
                format!("{}{}", r.stdout, r.stderr)
            };
            // A green check is only evidence if the tests judging this lane were
            // not written by it. Report any it changed, so "passed" can be read
            // with that in view rather than taken at face value.
            let capture_result = self
                .capture_attempt_candidate(session_id, &set, index, true)
                .await;
            if self.attempt_cancelled(session_id, set_id).await {
                return Err(DaemonError::AttemptConflict(
                    "Checks were cancelled; retry Discard or run Checks again".to_string(),
                ));
            }
            let capture = match capture_result {
                Ok(capture) => capture,
                Err(error) => {
                    let _ = self
                        .stream_bus
                        .send(crate::stream::StreamFrame::LaneVerified {
                            attempt_set_id: set_id.to_string(),
                            session: session_id.to_string(),
                            index,
                            passed: false,
                            changed_files: 0,
                            touched_tests: Vec::new(),
                        });
                    verdicts.push(crate::git::LaneVerdict {
                        index,
                        passed: false,
                        exit_code: -1,
                        output: error.to_string(),
                        changed_files: 0,
                        touched_tests: Vec::new(),
                        patch_sha256: None,
                    });
                    self.write_variant_meta(session_id, set_id, "verdicts.json", &verdicts)
                        .await?;
                    continue;
                }
            };
            let changed = &capture.paths;
            let touched_tests: Vec<String> = changed
                .iter()
                .filter(|p| crate::git::looks_like_test(p))
                .cloned()
                .collect();
            // Report this lane the moment it resolves rather than holding
            // every verdict until the last check finishes.
            let _ = self
                .stream_bus
                .send(crate::stream::StreamFrame::LaneVerified {
                    attempt_set_id: set_id.to_string(),
                    session: session_id.to_string(),
                    index,
                    passed: r.exit_code == 0 && !capture.checked.changes_gitlink,
                    changed_files: changed.len(),
                    touched_tests: touched_tests.clone(),
                });
            let passed = r.exit_code == 0 && !capture.checked.changes_gitlink;
            let output = if capture.checked.changes_gitlink {
                format!(
                    "{}\nAxocoatl cannot safely Keep submodule/gitlink changes yet.",
                    combined
                )
            } else {
                combined
            };
            let patch_sha256 = capture.checked.patch_sha256.clone();
            checked_trees.push(capture.checked);
            checked_trees.sort_by_key(|checked| checked.index);
            self.write_variant_meta(session_id, set_id, "checked-trees.json", &checked_trees)
                .await?;
            verdicts.push(crate::git::LaneVerdict {
                index,
                passed,
                exit_code: r.exit_code,
                output: crate::git::verdict_tail(&output),
                changed_files: changed.len(),
                touched_tests,
                patch_sha256: Some(patch_sha256),
            });
            self.write_variant_meta(session_id, set_id, "verdicts.json", &verdicts)
                .await?;
        }
        if self.attempt_cancelled(session_id, set_id).await {
            return Err(DaemonError::AttemptConflict(
                "Checks were cancelled; retry Discard or run Checks again".to_string(),
            ));
        }
        set.state = crate::git::AttemptSetState::Verified;
        self.persist_attempt_set(&sandbox, &set).await?;
        Ok(verdicts)
    }

    /// Check that a model can drive a lane before a run is spent on it.
    ///
    /// Offers one trivial tool and asks for it. A model that answers in prose —
    /// even prose containing a correct-looking call — cannot edit files here, and
    /// a lane using it will burn minutes to produce an empty diff that then
    /// passes every check. One call, seconds, versus discovering it afterwards.
    pub async fn probe_lane_model(
        &self,
        provider_id: &str,
        model: &str,
    ) -> Result<crate::git::ModelProbe, DaemonError> {
        let provider = self.resolve_provider(provider_id, Some(model))?;
        let mut req = axocoatl_llm::ChatRequest::simple(
            "Read the file README.md. Call the tool — do not describe the call.",
        );
        req.tools = vec![axocoatl_llm::ToolDefinition {
            name: "read_file".to_string(),
            description: "Read a file from the project".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
            concurrency: Default::default(),
        }];
        req.model_override = Some(model.to_string());

        match provider.chat(req).await {
            Ok(r) if !r.tool_calls.is_empty() => Ok(crate::git::ModelProbe {
                model: model.to_string(),
                usable: true,
                detail: "Returns structured tool calls.".to_string(),
            }),
            Ok(r) => {
                // Distinguish "tried and was not parsed" from "did not try": the
                // first is a template/runtime mismatch the user can route around
                // by choosing another model, and it is by far the more confusing.
                let looked_like_a_call = r.content.contains("\"name\"")
                    && (r.content.contains("read_file") || r.content.contains("arguments"));
                Ok(crate::git::ModelProbe {
                    model: model.to_string(),
                    usable: false,
                    detail: if looked_like_a_call {
                        "Emits tool calls as plain text rather than structured calls, so the \
                         runtime never sees them. A lane using this model will edit nothing. \
                         Choose a model with agentic tool-calling support."
                            .to_string()
                    } else {
                        "Did not call the tool when asked. A lane using this model will \
                         probably edit nothing."
                            .to_string()
                    },
                })
            }
            Err(e) => Ok(crate::git::ModelProbe {
                model: model.to_string(),
                usable: false,
                detail: format!("The model could not be reached: {e}"),
            }),
        }
    }

    /// Turn a task into a spec precise enough for cheap models to execute.
    ///
    /// Two bounded calls rather than one: the planner is first shown the
    /// repository's tracked files and asked which it needs, those are read, and
    /// only then does it write the plan. That keeps the context proportional to
    /// the task instead of the repository, so this works on a large codebase,
    /// and it reads through the sandbox so it works on a remote backend where
    /// the tree only exists inside the VM.
    ///
    /// This is the expensive half of "expensive brains, cheap hands", and the
    /// cheapest place for a human to intervene: one plan reviewed beats N
    /// executions of a bad one.
    pub async fn plan_task(
        &self,
        session_id: &str,
        task: &str,
        provider_id: &str,
        model: Option<String>,
    ) -> Result<crate::git::Plan, DaemonError> {
        /// Files listed to the planner when choosing what to read.
        const MAX_LISTED: usize = 400;
        /// Files it may ask for, and how much of each is shown.
        const MAX_READ: usize = 8;
        const MAX_FILE_BYTES: usize = 16 * 1024;

        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let provider = self.resolve_provider(provider_id, model.as_deref())?;

        // Tracked files only — respects .gitignore, so no node_modules or build
        // output drowns the list.
        let listing = self
            .session_git(session_id, &["ls-files"])
            .await
            .map(|r| r.stdout)
            .unwrap_or_default();
        let files: Vec<&str> = listing
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(MAX_LISTED)
            .collect();
        if files.is_empty() {
            return Err(DaemonError::Session(
                "the session has no tracked files to plan against".to_string(),
            ));
        }

        // Call 1 — which files matter for this task?
        let scope_prompt = format!(
            "You are planning a change to a codebase. Here are its tracked files:\n\n{}\n\n\
             The task is:\n{task}\n\n\
             Name the files you must read to write a precise plan — at most {MAX_READ}, \
             fewest that suffice. Reply with JSON only: {{\"files\": [\"path\", …]}}",
            files.join("\n"),
        );
        let mut req = axocoatl_llm::ChatRequest::simple(scope_prompt);
        req.response_format = Some(axocoatl_core::ResponseFormat::Json);
        req.model_override = model.clone();
        let scope = provider
            .chat(req)
            .await
            .map_err(|e| DaemonError::Provider(format!("planning (scope): {e}")))?;
        #[derive(serde::Deserialize, Default)]
        struct Scope {
            #[serde(default)]
            files: Vec<String>,
        }
        let wanted: Scope =
            serde_json::from_str(crate::git::unfence_json(&scope.content)).unwrap_or_default();

        // Read them through the sandbox — the tree may only exist inside the VM.
        let mut context = String::new();
        for path in wanted
            .files
            .iter()
            .filter(|p| files.contains(&p.as_str()))
            .take(MAX_READ)
        {
            if let Ok(r) = sandbox
                .exec(&["cat", path.as_str()], Duration::from_secs(20))
                .await
            {
                if r.ok() {
                    let body = if r.stdout.len() > MAX_FILE_BYTES {
                        let mut end = MAX_FILE_BYTES;
                        while !r.stdout.is_char_boundary(end) {
                            end -= 1;
                        }
                        &r.stdout[..end]
                    } else {
                        &r.stdout
                    };
                    context.push_str(&format!("\n--- {path} ---\n{body}\n"));
                }
            }
        }

        // Call 2 — the plan itself.
        let plan_prompt = format!(
            "Write an implementation plan for this task, precise enough that a small \
             model can execute it without inferring anything.\n\n\
             # Task\n{task}\n\n\
             # Relevant files{}\n\n\
             Name exact files and exact changes — signatures, names, where code goes. \
             State how one would know the work is done.\n\n\
             CRITICAL: the project's existing tests are the independent arbiter that \
             decides whether an attempt succeeded. Never plan to add, modify or delete \
             a test file, and never make acceptance depend on new tests — several \
             models will execute this plan separately and be graded by those tests, so \
             a plan that has them write their own lets each one grade its own work. \
             Plan implementation only, and list touching tests under constraints.\n\n\
             Reply with JSON only:\n\
             {{\"summary\": \"<one sentence>\", \"steps\": [{{\"path\": \"<file>\", \
             \"change\": \"<specific change>\"}}], \"constraints\": [\"<do not …>\"], \
             \"acceptance\": [\"<done when …>\"]}}",
            if context.is_empty() {
                "\n(none could be read)".to_string()
            } else {
                format!("\n{context}")
            },
        );
        let mut req = axocoatl_llm::ChatRequest::simple(plan_prompt);
        req.response_format = Some(axocoatl_core::ResponseFormat::Json);
        req.model_override = model;
        let out = provider
            .chat(req)
            .await
            .map_err(|e| DaemonError::Provider(format!("planning: {e}")))?;
        serde_json::from_str(crate::git::unfence_json(&out.content)).map_err(|e| {
            DaemonError::Session(format!(
                "the planner did not return a usable plan ({e}); got: {}",
                crate::git::verdict_tail(&out.content)
            ))
        })
    }

    /// What a variants run cost, and what it would have cost on one model.
    ///
    /// The comparison is the point: fanning out to several cheap models is only
    /// worth doing if the arithmetic favours it, so this reports real token
    /// counts priced from config rather than an estimate. A missing remote price
    /// remains unknown; configured Ollama lanes are explicitly known-free.
    fn complete_lane_token_volume(lanes: &[crate::git::LaneUsage], expected: usize) -> bool {
        expected > 0 && lanes.len() == expected && lanes.iter().all(|lane| lane.token_usage_known)
    }

    pub async fn variants_cost(
        &self,
        session_id: &str,
        set_id: &str,
        baseline_model: &str,
        baseline_provider: Option<&str>,
    ) -> Result<crate::git::RunCost, DaemonError> {
        let price = |model: &str| -> crate::git::ModelPrice {
            self.config
                .pricing
                .get(model)
                .map(|p| crate::git::ModelPrice {
                    input_per_mtok: p.input_per_mtok,
                    output_per_mtok: p.output_per_mtok,
                })
                .unwrap_or_default()
        };
        let baseline_is_local = match baseline_provider {
            Some(provider) => provider == "ollama",
            None => self
                .config
                .agents
                .iter()
                .any(|agent| agent.provider == "ollama" && agent.model == baseline_model),
        };
        let baseline = if baseline_is_local {
            crate::git::ModelPrice::default()
        } else {
            price(baseline_model)
        };
        let baseline_price_known = baseline_is_local
            || (!matches!(baseline_provider, Some("ollama"))
                && self.config.pricing.contains_key(baseline_model));

        let set = self
            .peek_current_attempt_set(session_id)
            .await?
            .ok_or_else(|| {
                DaemonError::AttemptConflict(
                    "this session has no current attempt set to price".to_string(),
                )
            })?;
        if set.id != set_id {
            return Err(DaemonError::AttemptConflict(format!(
                "attempt set '{set_id}' is stale; current set is '{}'",
                set.id
            )));
        }
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        Self::validate_attempt_set_identity(&session.working_dir, &set)?;
        let attempt_root = crate::attempts::attempt_root(&session.working_dir, session_id, set_id);
        let indexes: Vec<usize> = set.lanes.iter().map(|lane| lane.index).collect();
        let lanes = Self::read_lane_usage_host(&attempt_root, &indexes).await?;

        let mut total = 0.0;
        let mut baseline_total = 0.0;
        for lane in &lanes {
            total += lane.cost_usd;
            baseline_total += baseline.cost(lane.input_tokens, lane.output_tokens);
        }
        let complete = lanes.len() == set.lanes.len();
        let token_volume_known = Self::complete_lane_token_volume(&lanes, set.lanes.len());
        let all_local = complete
            && !set.lanes.is_empty()
            && set
                .lanes
                .iter()
                .all(|lane| lane.provider.as_deref() == Some("ollama"));
        Ok(crate::git::RunCost {
            all_local,
            total_usd: total,
            actual_cost_known: complete
                && !lanes.is_empty()
                && lanes.iter().all(|lane| lane.cost_known),
            baseline_model: baseline_model.to_string(),
            baseline_usd: baseline_total,
            baseline_cost_known: baseline_price_known && token_volume_known,
            saved_usd: (baseline_total - total).max(0.0),
            lanes,
        })
    }

    /// Resolve a provider by id, for callers that need one outside an agent.
    ///
    /// Ollama is deliberately **not** in the registry: its model is chosen per
    /// caller, so it is constructed on use. Everything else lives in the
    /// registry. Keeping that fork in one place matters — it existing in two
    /// places is exactly why judging could not reach Ollama, the provider this
    /// product cares most about.
    pub fn resolve_provider(
        &self,
        provider_id: &str,
        model: Option<&str>,
    ) -> Result<Arc<dyn axocoatl_llm::LlmProvider>, DaemonError> {
        if provider_id == "ollama" {
            let ollama = self.config.providers.ollama.as_ref().ok_or_else(|| {
                DaemonError::Provider("Ollama provider not configured".to_string())
            })?;
            let model = model
                .filter(|m| !m.is_empty())
                .or(ollama.model.as_deref())
                .unwrap_or("llama3.2");
            return Ok(Arc::new(
                axocoatl_llm_ollama::OllamaProvider::with_base_url(&ollama.base_url, model),
            ));
        }
        self.provider_registry
            .get(provider_id)
            .cloned()
            .ok_or_else(|| {
                DaemonError::Provider(format!("Provider '{provider_id}' not configured"))
            })
    }

    /// The unified patch for one lane — what that candidate actually changed.
    ///
    /// Tracked changes come from a base-relative binary diff. Untracked files
    /// are appended as independent no-index patches, so review never mutates
    /// the candidate's Git index merely to make new files visible.
    pub async fn variant_patch(
        &self,
        session_id: &str,
        set_id: &str,
        index: usize,
    ) -> Result<String, DaemonError> {
        self.variant_patch_details(session_id, set_id, index)
            .await
            .map(|(patch, _)| patch)
    }

    async fn variant_patch_details(
        &self,
        session_id: &str,
        set_id: &str,
        index: usize,
    ) -> Result<(String, Vec<String>), DaemonError> {
        let set = self.require_attempt_set(session_id, set_id).await?;
        Self::require_review_storage(&set)?;
        let lane = set
            .lanes
            .iter()
            .find(|lane| lane.index == index)
            .ok_or_else(|| {
                DaemonError::Session(format!(
                    "lane {index} is not part of attempt set '{set_id}'"
                ))
            })?;
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let primary = self.ensure_sandbox(&session).await?;
        let worktree =
            crate::attempts::worktree_path(primary.root(), session_id, set_id, lane.index);
        Self::sanitize_attempt_git_config_at(&worktree, &set.base_sha).await?;
        let worktree = worktree.to_string_lossy().to_string();
        let tracked_paths = Self::require_raw_git_output(
            self.session_git_at(
                session_id,
                &worktree,
                &[
                    "diff",
                    "--name-only",
                    "-z",
                    "--no-renames",
                    &set.base_sha,
                    "--",
                ],
            )
            .await?,
            &format!("reading attempt {} changed paths", index + 1),
        )?;
        let untracked_paths = Self::require_raw_git_output(
            self.session_git_at(
                session_id,
                &worktree,
                &["ls-files", "--others", "--exclude-standard", "-z"],
            )
            .await?,
            &format!("reading attempt {} untracked paths", index + 1),
        )?;
        let mut paths: Vec<String> = tracked_paths
            .split('\0')
            .chain(untracked_paths.split('\0'))
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect();
        paths.sort();
        paths.dedup();
        if paths
            .iter()
            .any(|path| path == ".axo-variants" || path.starts_with(".axo-variants/"))
        {
            return Err(DaemonError::AttemptConflict(format!(
                "attempt {} changed Axocoatl's reserved .axo-variants metadata and cannot be reviewed or kept",
                index + 1
            )));
        }
        let tracked = self
            .session_git_at(
                session_id,
                &worktree,
                &[
                    "diff",
                    "--binary",
                    "--full-index",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-renames",
                    &set.base_sha,
                ],
            )
            .await?;
        if !tracked.ok() {
            return Err(DaemonError::Session(format!(
                "reading lane {index}'s patch: {}",
                tracked.stderr.trim()
            )));
        }
        // A patch is a byte-sensitive protocol document. Never trim it.
        let mut patch = tracked.stdout;
        for path in untracked_paths.split('\0').filter(|path| !path.is_empty()) {
            let addition = self
                .session_git_at(
                    session_id,
                    &worktree,
                    &[
                        "diff",
                        "--no-index",
                        "--binary",
                        "--full-index",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--",
                        "/dev/null",
                        path,
                    ],
                )
                .await?;
            // `git diff --no-index` returns 1 when it successfully found a
            // difference; 0 is an empty file/no delta, >1 is an actual error.
            if !matches!(addition.exit_code, 0 | 1) {
                return Err(DaemonError::Session(format!(
                    "reading untracked file '{path}' in attempt {}: {}",
                    index + 1,
                    addition.stderr.trim()
                )));
            }
            patch.push_str(&addition.stdout);
        }
        Ok((patch, paths))
    }

    /// Rank the candidates of a variants run and say **why**.
    ///
    /// This is the second half of fan-in. Verification removes what is wrong;
    /// judging orders what is left, so a reviewer reads one diff with a reason
    /// attached instead of N diffs with none. `only` narrows to the lanes that
    /// survived `verify_variants`; omit it to judge every lane.
    ///
    /// The judge is expected to be a *stronger* model than the ones that
    /// executed — comparing solutions is a different, harder job than producing
    /// one, and it is the step worth spending on.
    pub async fn judge_variants(
        &self,
        session_id: &str,
        set_id: &str,
        provider_id: &str,
        model: Option<String>,
    ) -> Result<crate::git::Judgment, DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        let (set, _) = self.require_terminal_attempt(session_id, set_id).await?;
        if !matches!(
            set.state,
            crate::git::AttemptSetState::Verified | crate::git::AttemptSetState::Judged
        ) {
            return Err(DaemonError::AttemptConflict(
                "run Checks to completion before asking Judge".to_string(),
            ));
        }
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let attempt_root = crate::attempts::attempt_root(sandbox.root(), session_id, set_id)
            .to_string_lossy()
            .to_string();
        let verdicts: Vec<crate::git::LaneVerdict> = self
            .read_variant_meta(&sandbox, &attempt_root, "verdicts.json")
            .await?
            .ok_or_else(|| {
                DaemonError::AttemptConflict(
                    "run Checks before asking a judge to compare attempts".to_string(),
                )
            })?;
        let checked_trees: Vec<StoredCheckedTree> = self
            .read_variant_meta(&sandbox, &attempt_root, "checked-trees.json")
            .await?
            .ok_or_else(|| {
                DaemonError::AttemptConflict(
                    "Checks were recorded without protected candidate trees; run Checks again"
                        .to_string(),
                )
            })?;
        let lanes: Vec<usize> = verdicts
            .iter()
            .filter(|verdict| verdict.passed && verdict.changed_files > 0)
            .map(|verdict| verdict.index)
            .collect();
        if lanes.len() < 2 {
            return Err(DaemonError::Session(
                "Judge needs at least two passing attempts that changed files".to_string(),
            ));
        }

        let mut sections = String::new();
        for index in &lanes {
            let expected = verdicts
                .iter()
                .find(|verdict| verdict.index == *index)
                .and_then(|verdict| verdict.patch_sha256.as_deref())
                .ok_or_else(|| {
                    DaemonError::AttemptConflict(
                        "Checks were recorded without patch identities; run Checks again"
                            .to_string(),
                    )
                })?;
            let checked = checked_trees
                .iter()
                .find(|checked| checked.index == *index)
                .ok_or_else(|| {
                    DaemonError::AttemptConflict(format!(
                        "attempt {} is missing its checked candidate tree; run Checks again",
                        index + 1
                    ))
                })?;
            if checked.patch_sha256 != expected || checked.changes_gitlink {
                return Err(DaemonError::AttemptConflict(format!(
                    "attempt {} checked identity is inconsistent; run Checks again before Judge",
                    index + 1
                )));
            }
            let reference = crate::attempts::checked_candidate_ref(set_id, *index);
            let imported = Self::require_git_output(
                self.session_git(session_id, &["rev-parse", "--verify", &reference])
                    .await?,
                &format!("validating attempt {} checked commit", index + 1),
            )?;
            let tree_expression = format!("{reference}^{{tree}}");
            let imported_tree = Self::require_git_output(
                self.session_git(session_id, &["rev-parse", &tree_expression])
                    .await?,
                &format!("validating attempt {} checked tree", index + 1),
            )?;
            if imported != checked.commit_oid || imported_tree != checked.tree_oid {
                return Err(DaemonError::AttemptConflict(format!(
                    "attempt {} protected checked tree changed; run Checks again",
                    index + 1
                )));
            }
            let patch = Self::require_raw_git_output(
                self.session_git(
                    session_id,
                    &[
                        "-c",
                        "diff.algorithm=myers",
                        "diff",
                        "--binary",
                        "--full-index",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--no-renames",
                        &set.base_sha,
                        &reference,
                    ],
                )
                .await?,
                &format!("rendering attempt {} checked delta", index + 1),
            )?;
            let patch = if patch.trim().is_empty() {
                "(this candidate changed nothing)".to_string()
            } else {
                let (prefix, truncated) = crate::git::truncate_judge_patch(&patch);
                if truncated {
                    format!(
                        "{prefix}\n… (patch truncated at {} bytes)",
                        crate::git::JUDGE_PATCH_MAX
                    )
                } else {
                    patch
                }
            };
            sections.push_str(&format!("\n## Candidate {index}\n```diff\n{patch}\n```\n"));
        }

        let ranking_contract = judge_ranking_contract(&lanes);
        let prompt = format!(
            "You are reviewing {n} independent attempts at the same software task. \
             Every attempt below already passes the project's automated checks, so \
             correctness is not the question — judge them on engineering quality: \
             clarity, how well the approach fits the existing code, edge cases \
             handled, and what each one would cost to live with.\n\n\
             # Task\n{task}\n\n\
             # Candidates\n{sections}\n\n\
             Rank every candidate (rank 1 = best) and name the real trade-off for \
             each — what you gain and what you give up by taking it. Do not \
             summarise the diff; say why one would be chosen over another. \
             {ranking_contract}\n\n\
             Reply with JSON only, no prose outside it:\n\
             {{\"winner\": <candidate index>, \"reasoning\": \"<what separates them>\", \
             \"candidates\": [{{\"index\": <n>, \"rank\": <n>, \"approach\": \"<one sentence>\", \
             \"tradeoffs\": \"<gain vs give up>\"}}]}}",
            n = lanes.len(),
            task = set.task,
        );

        // Resolve through the shared path so a local Ollama judge works — the
        // registry alone never contains it.
        let provider = self.resolve_provider(provider_id, model.as_deref())?;

        let mut request = axocoatl_llm::ChatRequest::simple(prompt);
        request.response_format = Some(axocoatl_core::ResponseFormat::Json);
        // Ollama already carries the model from `resolve_provider`; for registry
        // providers this selects it per request.
        request.model_override = model;
        // Judge owns the workspace decision lease so Keep and Discard cannot
        // race its reviewed patch set. Bound that external call: an unavailable
        // provider must not make the set undiscardable forever.
        let response = tokio::time::timeout(Duration::from_secs(300), provider.chat(request))
            .await
            .map_err(|_| {
                DaemonError::Provider(
                    "judging attempts timed out after 5 minutes; retry Judge or Discard"
                        .to_string(),
                )
            })?
            .map_err(|e| DaemonError::Provider(format!("judging variants: {e}")))?;

        let body = crate::git::unfence_json(&response.content);
        let mut judgment: crate::git::Judgment = serde_json::from_str(body).map_err(|e| {
            DaemonError::Session(format!(
                "the judge did not return usable JSON ({e}); got: {}",
                crate::git::verdict_tail(&response.content)
            ))
        })?;
        crate::git::validate_judgment(&judgment, &lanes).map_err(|error| {
            DaemonError::Session(format!("the judge returned an invalid ranking: {error}"))
        })?;
        // Order best-first regardless of how the model emitted them.
        judgment.candidates.sort_by_key(|c| c.rank);
        self.write_variant_meta(session_id, set_id, "judgment.json", &judgment)
            .await?;
        Ok(judgment)
    }

    pub async fn variants_status(
        &self,
        session_id: &str,
        set_id: &str,
    ) -> Result<Vec<crate::git::VariantStatus>, DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        let set = self.require_attempt_set(session_id, set_id).await?;
        Self::require_review_storage(&set)?;
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let mut out = Vec::new();
        for lane in set.lanes {
            let lane_sandbox = self.attempt_sandbox(&session, set_id, lane.index).await?;
            let worktree = lane_sandbox.root().to_string_lossy().to_string();
            let name_status = Self::require_git_output(
                Self::attempt_git(
                    &lane_sandbox,
                    &[
                        "diff",
                        "--name-status",
                        "--find-renames",
                        &set.base_sha,
                        "--",
                    ],
                )
                .await?,
                &format!("reading attempt {} changed paths", lane.index + 1),
            )?;
            let numstat = Self::require_git_output(
                Self::attempt_git(
                    &lane_sandbox,
                    &["diff", "--numstat", "--find-renames", &set.base_sha, "--"],
                )
                .await?,
                &format!("sizing attempt {} changes", lane.index + 1),
            )?;
            let untracked = Self::require_raw_git_output(
                Self::attempt_git(
                    &lane_sandbox,
                    &["ls-files", "--others", "--exclude-standard", "-z"],
                )
                .await?,
                &format!("reading attempt {} untracked paths", lane.index + 1),
            )?;
            let branch = crate::attempts::branch_name(set_id, lane.index);
            let mut status = crate::git::parse_base_diff_status(&branch, &name_status, &numstat);
            for path in untracked.split('\0').filter(|path| !path.is_empty()) {
                if status.files.iter().all(|file| file.path != path) {
                    status.files.push(crate::git::GitFile {
                        path: path.to_string(),
                        state: "untracked".to_string(),
                        added: None,
                        removed: None,
                        staged: false,
                        unstaged: true,
                        last_turn: false,
                    });
                }
            }
            status
                .files
                .sort_by(|left, right| left.path.cmp(&right.path));
            status.clean = status.files.is_empty();
            out.push(crate::git::VariantStatus {
                index: lane.index,
                branch,
                worktree,
                status,
                model: lane.model,
                agent: lane.agent,
            });
        }
        out.sort_by_key(|lane| lane.index);
        Ok(out)
    }

    async fn remove_attempt_containers_exact(
        session_id: &str,
        set: &crate::git::AttemptSet,
    ) -> Vec<String> {
        let containers = set
            .lanes
            .iter()
            .map(|lane| crate::attempts::container_id(session_id, &set.id, lane.index))
            .collect::<Vec<_>>();
        match SessionSandbox::remove_named_many(&containers, ATTEMPT_CONTAINER_STOP_TIMEOUT).await {
            Ok(()) => Vec::new(),
            Err(error) => vec![format!("stop attempt sandboxes: {error}")],
        }
    }

    /// Signal and await every process that can still target an attempt clone,
    /// then exact-remove its deterministic container. This does not touch the
    /// clone, protected refs, or current pointer, so it is safe to call before
    /// acquiring the workspace operation in order to unblock a review command
    /// that owns that operation.
    async fn interrupt_attempt_runtime(
        &self,
        session_id: &str,
        set: &crate::git::AttemptSet,
    ) -> Result<(), DaemonError> {
        let actors = {
            let mut active = self.active_attempts.lock().await;
            match active.get_mut(session_id) {
                Some(run) if run.set_id != set.id => {
                    return Err(DaemonError::AttemptConflict(format!(
                        "attempt set '{}' is active, not '{}'",
                        run.set_id, set.id
                    )))
                }
                Some(runtime) => {
                    // Abort wrapper tasks and signal every actor while the runtime is
                    // still process-owned. Cancellation at a later await leaves this
                    // entry available to a retry instead of orphaning live actors.
                    for task in &runtime.tasks {
                        task.abort();
                    }
                    for (_, actor) in &runtime.actors {
                        actor.kill();
                    }
                    runtime.actors.clone()
                }
                // A restarted daemon has no actor handles, but exact names
                // still let cleanup stop preserved lane/check containers.
                None => Vec::new(),
            }
        };

        // Force every actor down concurrently before a deterministic container
        // name can be reused for Checks. A signal without the bounded wait has
        // a small but real window where the old actor can target the freshly
        // created review container through its surviving sandbox handle.
        let mut failures = Vec::new();
        let mut actor_shutdowns = tokio::task::JoinSet::new();
        for (actor_id, actor) in &actors {
            let actor_id = actor_id.to_string();
            let actor = actor.clone();
            actor_shutdowns.spawn(async move {
                if matches!(actor.get_status(), ractor::ActorStatus::Stopped) {
                    return (actor_id, Ok(()));
                }
                let result = actor.wait(Some(ATTEMPT_ACTOR_STOP_TIMEOUT)).await;
                if result.is_err() && matches!(actor.get_status(), ractor::ActorStatus::Stopped) {
                    (actor_id, Ok(()))
                } else {
                    (actor_id, result)
                }
            });
        }
        while let Some(result) = actor_shutdowns.join_next().await {
            match result {
                Ok((_, Ok(()))) => {}
                Ok((actor_id, Err(error))) => {
                    failures.push(format!("attempt actor '{actor_id}' did not stop: {error}"));
                }
                Err(error) => failures.push(format!("attempt actor shutdown failed: {error}")),
            }
        }
        failures.extend(Self::remove_attempt_containers_exact(session_id, set).await);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(DaemonError::Session(failures.join("; ")))
        }
    }

    async fn stop_attempt_runtime(
        &self,
        session_id: &str,
        set: &crate::git::AttemptSet,
    ) -> Result<(), DaemonError> {
        self.interrupt_attempt_runtime(session_id, set).await?;

        // Actor death is the commit point for releasing runtime ownership: no
        // surviving executor can target a subsequently recreated container.
        let runtime = {
            let mut active = self.active_attempts.lock().await;
            match active.get(session_id) {
                Some(run) if run.set_id != set.id => {
                    return Err(DaemonError::AttemptConflict(format!(
                        "attempt set '{}' became active while stopping '{}'",
                        run.set_id, set.id
                    )))
                }
                Some(_) => active.remove(session_id),
                None => None,
            }
        };
        let Some(runtime) = runtime else {
            return Ok(());
        };

        let mut failures = Vec::new();
        let mut task_shutdowns = tokio::task::JoinSet::new();
        for task in runtime.tasks {
            task_shutdowns
                .spawn(async move { tokio::time::timeout(ATTEMPT_ACTOR_STOP_TIMEOUT, task).await });
        }
        while let Some(result) = task_shutdowns.join_next().await {
            match result {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) if error.is_cancelled() => {}
                Ok(Ok(Err(error))) => {
                    failures.push(format!("lane task did not join cleanly: {error}"));
                }
                Ok(Err(_)) => failures.push(format!(
                    "lane task did not stop within {} seconds",
                    ATTEMPT_ACTOR_STOP_TIMEOUT.as_secs()
                )),
                Err(error) => failures.push(format!("lane task shutdown failed: {error}")),
            }
        }
        for (actor_id, _) in &runtime.actors {
            self.agent_registry.remove(actor_id).await;
            self.remove_attempt_memory(actor_id).await;
        }
        if let Ok(mut runs) = self.active_runs.lock() {
            for lane in &set.lanes {
                runs.remove(&crate::attempts::run_id(session_id, lane.index));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(DaemonError::Session(failures.join("; ")))
        }
    }

    async fn remove_attempt_memory(&self, actor_id: &AgentId) {
        let id = actor_id.to_string();
        let checkpoint_dir = std::path::Path::new(&self.data_dir)
            .join("checkpoints")
            .join(&id);
        let daily_dir = std::path::Path::new(&self.data_dir)
            .join("memory")
            .join("daily_log")
            .join(&id);
        let core_file = axocoatl_memory::core_store_path(&self.data_dir, &id);
        let _ = tokio::fs::remove_dir_all(checkpoint_dir).await;
        let _ = tokio::fs::remove_dir_all(daily_dir).await;
        let _ = tokio::fs::remove_file(core_file).await;
    }

    async fn read_lane_output(
        &self,
        sandbox: &Arc<dyn Sandbox>,
        set: &crate::git::AttemptSet,
        index: usize,
    ) -> Result<Option<String>, DaemonError> {
        #[derive(serde::Deserialize)]
        struct StoredOutput {
            content: String,
        }
        let root = crate::attempts::attempt_root(sandbox.root(), &set.session_id, &set.id)
            .to_string_lossy()
            .to_string();
        self.read_variant_meta::<StoredOutput>(sandbox, &root, &format!("output-{index}.json"))
            .await
            .map(|output| output.map(|output| output.content))
    }

    async fn append_kept_attempt_to_session(
        &self,
        session: &Session,
        set: &crate::git::AttemptSet,
        index: usize,
        assistant: String,
        sandbox: &Arc<dyn Sandbox>,
    ) -> Result<(), DaemonError> {
        let agent_id = match &session.mode {
            SessionMode::SingleAgent { agent_id } => agent_id,
            // Explore currently rejects these modes at start. Refuse again at
            // the commit point so a forged/legacy set can never be marked
            // TranscriptRecorded while the permanent chat silently loses it.
            SessionMode::Lattice { .. } | SessionMode::Custom { .. } => {
                return Err(DaemonError::AttemptConflict(
                    "Keep cannot record a multi-agent session transcript yet".to_string(),
                ));
            }
        };
        let scoped = AgentId::new(format!("{}:{}", session.id, agent_id));
        if let Some(actor) = self.agent_registry.get(&scoped).await {
            if !matches!(actor.get_status(), ractor::ActorStatus::Stopped) {
                let graceful = actor
                    .stop_and_wait(None, Some(Duration::from_secs(10)))
                    .await;
                if graceful.is_err() && !matches!(actor.get_status(), ractor::ActorStatus::Stopped)
                {
                    actor
                        .kill_and_wait(Some(Duration::from_secs(5)))
                        .await
                        .map_err(|error| {
                            DaemonError::AttemptConflict(format!(
                                "the session chat is still busy, so attempt {} could not be recorded: {error}",
                                index + 1
                            ))
                        })?;
                }
            }
            self.agent_registry.remove(&scoped).await;
        }
        let mut checkpoint = self
            .checkpoint_store
            .load_latest(&scoped)
            .await
            .map_err(|error| DaemonError::Session(error.to_string()))?
            .unwrap_or(axocoatl_memory::AgentCheckpoint {
                version: 0,
                agent_id: scoped.to_string(),
                checkpoint_time: unix_now(),
                session_messages: Vec::new(),
                cumulative_token_usage: axocoatl_core::TokenUsageStats::default(),
                behavior_state: None,
            });
        let task_sha256 = Self::patch_sha256(&set.task);
        let assistant_sha256 = Self::patch_sha256(&assistant);
        let attempt_root = crate::attempts::attempt_root(sandbox.root(), &set.session_id, &set.id)
            .to_string_lossy()
            .to_string();
        let commit: StoredTranscriptCommit = match self
            .read_variant_meta(sandbox, &attempt_root, "transcript-commit.json")
            .await?
        {
            Some(commit) => commit,
            None => {
                let commit = StoredTranscriptCommit {
                    base_checkpoint_version: checkpoint.version,
                    base_message_count: checkpoint.session_messages.len(),
                    task_sha256: task_sha256.clone(),
                    assistant_sha256: assistant_sha256.clone(),
                };
                Self::write_json_file(
                    sandbox,
                    &format!("{attempt_root}/transcript-commit.json"),
                    &commit,
                )
                .await?;
                commit
            }
        };
        if commit.task_sha256 != task_sha256 || commit.assistant_sha256 != assistant_sha256 {
            return Err(DaemonError::Session(
                "Keep transcript metadata does not match the selected attempt".to_string(),
            ));
        }
        let tail_matches = checkpoint.session_messages.len() == commit.base_message_count + 2
            && checkpoint.session_messages[commit.base_message_count].role
                == axocoatl_core::MessageRole::User
            && checkpoint.session_messages[commit.base_message_count].content == set.task
            && checkpoint.session_messages[commit.base_message_count + 1].role
                == axocoatl_core::MessageRole::Assistant
            && checkpoint.session_messages[commit.base_message_count + 1].content == assistant;
        if checkpoint.version == commit.base_checkpoint_version + 1 && tail_matches {
            return Ok(());
        }
        if checkpoint.version != commit.base_checkpoint_version
            || checkpoint.session_messages.len() != commit.base_message_count
        {
            return Err(DaemonError::AttemptConflict(
                "the session transcript changed during Keep; retry after restoring the recorded session checkpoint"
                    .to_string(),
            ));
        }
        checkpoint.version += 1;
        checkpoint.checkpoint_time = unix_now();
        checkpoint
            .session_messages
            .push(axocoatl_memory::StoredMessage {
                role: axocoatl_core::MessageRole::User,
                content: set.task.clone(),
                timestamp: unix_now(),
                token_count: self.counter.count_text(&set.task),
                name: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
            });
        checkpoint
            .session_messages
            .push(axocoatl_memory::StoredMessage {
                role: axocoatl_core::MessageRole::Assistant,
                content: assistant.clone(),
                timestamp: unix_now(),
                token_count: self.counter.count_text(&assistant),
                name: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
            });
        self.checkpoint_store
            .save(&checkpoint)
            .await
            .map_err(|error| DaemonError::Session(error.to_string()))
    }

    async fn snapshot_primary_keep_tree(
        &self,
        session_id: &str,
        set: &crate::git::AttemptSet,
        label: &str,
    ) -> Result<String, DaemonError> {
        let dir = self.session_dir(session_id).await?;
        let git_dir = Self::require_git_output(
            self.session_git(session_id, &["rev-parse", "--absolute-git-dir"])
                .await?,
            "locating Git storage for Keep",
        )?;
        let index_path = format!(
            "{git_dir}/axo-keep-{}-{label}.index",
            crate::attempts::set_key(&set.id)
        );
        for path in [&index_path, &format!("{index_path}.lock")] {
            match tokio::fs::remove_file(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(DaemonError::Session(format!(
                        "removing stale Keep index '{path}': {error}"
                    )))
                }
            }
        }
        let outcome = async {
            Self::require_git_output(
                self.session_git_with_index(
                    session_id,
                    &dir,
                    &index_path,
                    &["read-tree", &set.base_sha],
                )
                .await?,
                "seeding the Keep working-tree snapshot",
            )?;
            Self::require_git_output(
                self.session_git_with_index(session_id, &dir, &index_path, &["add", "-A"])
                    .await?,
                "capturing the primary working tree for Keep",
            )?;
            Self::require_git_output(
                self.session_git_with_index(session_id, &dir, &index_path, &["write-tree"])
                    .await?,
                "writing the primary Keep tree",
            )
        }
        .await;
        let _ = tokio::fs::remove_file(&index_path).await;
        let _ = tokio::fs::remove_file(format!("{index_path}.lock")).await;
        outcome
    }

    async fn tree_entries_for_paths(
        &self,
        session_id: &str,
        tree: &str,
        paths: &[String],
    ) -> Result<HashMap<String, String>, DaemonError> {
        let wanted: HashSet<&str> = paths.iter().map(String::as_str).collect();
        let mut entries = HashMap::new();
        for chunk in paths.chunks(128) {
            let mut args = vec![
                "--literal-pathspecs".to_string(),
                "ls-tree".to_string(),
                "-r".to_string(),
                "-z".to_string(),
                "--full-tree".to_string(),
                tree.to_string(),
                "--".to_string(),
            ];
            args.extend(chunk.iter().cloned());
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let output = Self::require_raw_git_output(
                self.session_git(session_id, &refs).await?,
                "reading Keep tree entries",
            )?;
            for (path, identity) in Self::parse_tree_entries(&output)? {
                if !wanted.contains(path.as_str()) {
                    continue;
                }
                if entries
                    .insert(path.clone(), identity.clone())
                    .is_some_and(|prior| prior != identity)
                {
                    return Err(DaemonError::Session(format!(
                        "Git returned inconsistent tree identity for {path:?}"
                    )));
                }
            }
        }
        Ok(entries)
    }

    async fn reset_derived_directory(path: &std::path::Path) -> Result<(), DaemonError> {
        match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                tokio::fs::remove_dir_all(path).await.map_err(|error| {
                    DaemonError::Session(format!(
                        "clearing Keep staging directory '{}': {error}",
                        path.display()
                    ))
                })?;
            }
            Ok(_) => {
                tokio::fs::remove_file(path).await.map_err(|error| {
                    DaemonError::Session(format!(
                        "clearing invalid Keep staging path '{}': {error}",
                        path.display()
                    ))
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(DaemonError::Session(format!(
                    "checking Keep staging path '{}': {error}",
                    path.display()
                )))
            }
        }
        tokio::fs::create_dir_all(path).await.map_err(|error| {
            DaemonError::Session(format!(
                "creating Keep staging directory '{}': {error}",
                path.display()
            ))
        })
    }

    async fn materialize_keep_tree(
        &self,
        session_id: &str,
        set: &crate::git::AttemptSet,
        tree: &str,
        stage_root: &std::path::Path,
        paths: &[String],
    ) -> Result<HashMap<String, String>, DaemonError> {
        Self::reset_derived_directory(stage_root).await?;
        let entries = self.tree_entries_for_paths(session_id, tree, paths).await?;
        let git_dir = Self::require_git_output(
            self.session_git(session_id, &["rev-parse", "--absolute-git-dir"])
                .await?,
            "locating Git storage for Keep staging",
        )?;
        let tree_key = tree.get(..12).unwrap_or(tree);
        let index_path = format!(
            "{git_dir}/axo-keep-{}-stage-{}.index",
            crate::attempts::set_key(&set.id),
            tree_key
        );
        let dir = self.session_dir(session_id).await?;
        let _ = tokio::fs::remove_file(&index_path).await;
        let _ = tokio::fs::remove_file(format!("{index_path}.lock")).await;
        let outcome = async {
            Self::require_git_output(
                self.session_git_with_index(session_id, &dir, &index_path, &["read-tree", tree])
                    .await?,
                "loading the Keep staging tree",
            )?;
            let mut stdin = String::new();
            for path in paths.iter().filter(|path| entries.contains_key(*path)) {
                stdin.push_str(path);
                stdin.push('\0');
            }
            if !stdin.is_empty() {
                Self::require_git_output(
                    self.session_git_stdin_with_index_work_tree(
                        session_id,
                        &git_dir,
                        &index_path,
                        &stage_root.to_string_lossy(),
                        &[
                            "--literal-pathspecs",
                            "checkout-index",
                            "--force",
                            "--stdin",
                            "-z",
                        ],
                        &stdin,
                    )
                    .await?,
                    "materializing the Keep staging tree",
                )?;
            }
            Ok(entries)
        }
        .await;
        let _ = tokio::fs::remove_file(&index_path).await;
        let _ = tokio::fs::remove_file(format!("{index_path}.lock")).await;
        outcome
    }

    async fn copy_keep_leaf(
        source: &std::path::Path,
        destination: &std::path::Path,
    ) -> Result<(), DaemonError> {
        let parent = destination.parent().ok_or_else(|| {
            DaemonError::Session(format!(
                "invalid Keep backup path '{}''",
                destination.display()
            ))
        })?;
        tokio::fs::create_dir_all(parent).await.map_err(|error| {
            DaemonError::Session(format!(
                "creating Keep backup directory '{}': {error}",
                parent.display()
            ))
        })?;
        let metadata = tokio::fs::symlink_metadata(source).await.map_err(|error| {
            DaemonError::Session(format!(
                "reading Keep recovery leaf '{}': {error}",
                source.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            let target = tokio::fs::read_link(source).await.map_err(|error| {
                DaemonError::Session(format!(
                    "reading Keep recovery symlink '{}': {error}",
                    source.display()
                ))
            })?;
            #[cfg(unix)]
            tokio::fs::symlink(target, destination)
                .await
                .map_err(|error| {
                    DaemonError::Session(format!(
                        "copying Keep recovery symlink '{}': {error}",
                        source.display()
                    ))
                })?;
            #[cfg(not(unix))]
            return Err(DaemonError::Session(
                "symlink Keep is supported only on Unix hosts".to_string(),
            ));
        } else if metadata.is_file() {
            tokio::fs::copy(source, destination)
                .await
                .map_err(|error| {
                    DaemonError::Session(format!(
                        "copying Keep recovery path '{}': {error}",
                        source.display()
                    ))
                })?;
            tokio::fs::set_permissions(destination, metadata.permissions())
                .await
                .map_err(|error| {
                    DaemonError::Session(format!(
                        "preserving Keep recovery mode '{}': {error}",
                        source.display()
                    ))
                })?;
        } else {
            return Err(DaemonError::AttemptConflict(format!(
                "Keep recovery leaf '{}' is not a file or symlink",
                source.display()
            )));
        }
        Ok(())
    }

    /// Rebuild the consumable staging directory only from the durable raw
    /// postimage store. The store is fully validated before `stage` is reset,
    /// and every rebuilt leaf is validated before the caller can mutate the
    /// workspace. Git checkout/filter behavior is intentionally absent here.
    async fn rebuild_keep_stage_from_postimage(
        apply_root: &std::path::Path,
        plans: &[StoredKeepPath],
        post_entries: &HashMap<String, String>,
    ) -> Result<std::path::PathBuf, DaemonError> {
        let postimage_root = apply_root.join("postimage");
        let stage_root = apply_root.join("stage");
        let affected: HashSet<String> = plans.iter().map(|plan| plan.path.clone()).collect();

        // Validate every durable leaf before touching even derived staging
        // state. This also ensures a later workspace delete cannot precede
        // discovery of a corrupt/missing postimage for another path.
        for plan in plans {
            let relative = std::path::Path::new(&plan.path);
            let stored =
                Self::fingerprint_keep_workspace_path(&postimage_root, relative, &affected).await?;
            if !Self::keep_fingerprint_matches(
                &stored,
                &plan.postimage,
                !post_entries.contains_key(&plan.path),
            ) {
                return Err(DaemonError::Session(format!(
                    "durable Keep postimage for {:?} changed bytes",
                    plan.path
                )));
            }
        }

        Self::reset_derived_directory(&stage_root).await?;
        for plan in plans
            .iter()
            .filter(|plan| post_entries.contains_key(&plan.path))
        {
            let relative = std::path::Path::new(&plan.path);
            Self::copy_keep_leaf(&postimage_root.join(relative), &stage_root.join(relative))
                .await?;
        }

        // Preflight all additions as one phase. Reconciliation may only begin
        // deleting/replacing workspace leaves after this loop succeeds.
        for plan in plans {
            let relative = std::path::Path::new(&plan.path);
            let staged =
                Self::fingerprint_keep_workspace_path(&stage_root, relative, &affected).await?;
            if !Self::keep_fingerprint_matches(
                &staged,
                &plan.postimage,
                !post_entries.contains_key(&plan.path),
            ) {
                return Err(DaemonError::Session(format!(
                    "rebuilt Keep postimage for {:?} changed bytes",
                    plan.path
                )));
            }
        }
        Ok(stage_root)
    }

    async fn ensure_keep_parent_dirs(
        workspace: &std::path::Path,
        relative: &std::path::Path,
    ) -> Result<(), DaemonError> {
        let Some(parent) = relative.parent() else {
            return Ok(());
        };
        let mut current = workspace.to_path_buf();
        for component in parent.components() {
            current.push(component.as_os_str());
            match tokio::fs::symlink_metadata(&current).await {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => {
                    return Err(DaemonError::AttemptConflict(format!(
                        "Keep parent '{}' is not a real directory",
                        current.display()
                    )))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::fs::create_dir(&current).await.map_err(|error| {
                        DaemonError::Session(format!(
                            "creating Keep parent '{}': {error}",
                            current.display()
                        ))
                    })?;
                }
                Err(error) => {
                    return Err(DaemonError::Session(format!(
                        "checking Keep parent '{}': {error}",
                        current.display()
                    )))
                }
            }
        }
        Ok(())
    }

    async fn remove_empty_keep_parents(workspace: &std::path::Path, relative: &std::path::Path) {
        let mut current = relative.parent().map(|path| workspace.join(path));
        while let Some(path) = current {
            if path == workspace {
                break;
            }
            match tokio::fs::remove_dir(&path).await {
                Ok(()) => current = path.parent().map(std::path::Path::to_path_buf),
                Err(_) => break,
            }
        }
    }

    fn keep_fingerprint_matches(
        actual: &Option<StoredFileFingerprint>,
        expected: &Option<StoredFileFingerprint>,
        tree_entry_absent: bool,
    ) -> bool {
        actual == expected
            // A file→directory transition has a safe crash point after the old
            // leaf is deleted and before a descendant creates the new directory.
            || (tree_entry_absent
                && actual.is_none()
                && expected
                    .as_ref()
                    .is_some_and(|fingerprint| fingerprint.kind == "directory"))
    }

    /// A completed Keep must not remain unresolved merely because a Git clean
    /// filter from the old container is unavailable while rendering status.
    /// This fallback intentionally reports every journal path as changed and
    /// leaves line counts unknown; it is conservative, durable, and sufficient
    /// for the success receipt. A later normal status read may refine it.
    fn conservative_keep_status(apply: &StoredKeepApply) -> crate::git::GitStatus {
        let files = apply
            .paths
            .iter()
            .map(|plan| {
                let pre_leaf = plan
                    .preimage
                    .as_ref()
                    .is_some_and(|image| matches!(image.kind.as_str(), "file" | "symlink"));
                let post_leaf = plan
                    .postimage
                    .as_ref()
                    .is_some_and(|image| matches!(image.kind.as_str(), "file" | "symlink"));
                let state = match (pre_leaf, post_leaf) {
                    (false, true) => "added",
                    (true, false) => "deleted",
                    _ => "modified",
                };
                crate::git::GitFile {
                    path: plan.path.clone(),
                    state: state.to_string(),
                    added: None,
                    removed: None,
                    staged: false,
                    unstaged: true,
                    last_turn: false,
                }
            })
            .collect();
        crate::git::GitStatus {
            // The branch is presentation-only here. Avoid another fallible Git
            // read between a completed transaction and its durable receipt.
            branch: "HEAD".to_string(),
            files,
            clean: false,
        }
    }

    fn completed_keep_receipt_allows_cleanup(
        set: &crate::git::AttemptSet,
        index: usize,
    ) -> Result<bool, DaemonError> {
        if set.state != crate::git::AttemptSetState::TranscriptRecorded {
            return Ok(false);
        }
        if set.kept_index != Some(index) {
            return Err(DaemonError::Session(
                "the completed Keep receipt disagrees with current attempt metadata".to_string(),
            ));
        }
        Ok(true)
    }

    async fn prepare_keep_apply(
        &self,
        session_id: &str,
        set: &crate::git::AttemptSet,
        checked: &StoredCheckedTree,
    ) -> Result<StoredKeepApply, DaemonError> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let workspace = sandbox.root();
        let apply_root = crate::attempts::keep_apply_root(workspace, session_id, &set.id);
        Self::reset_derived_directory(&apply_root).await?;

        let preimage_tree = self
            .snapshot_primary_keep_tree(session_id, set, "prepare")
            .await?;
        let message = format!(
            "axocoatl Keep preimage {}",
            crate::attempts::set_key(&set.id)
        );
        let preimage_commit = Self::require_git_output(
            self.session_git(
                session_id,
                &[
                    "commit-tree",
                    &preimage_tree,
                    "-p",
                    &set.base_sha,
                    "-m",
                    &message,
                ],
            )
            .await?,
            "protecting the Keep preimage",
        )?;
        let preimage_ref = crate::attempts::keep_preimage_ref(&set.id);
        Self::require_git_output(
            self.session_git(session_id, &["update-ref", &preimage_ref, &preimage_commit])
                .await?,
            "publishing the Keep preimage",
        )?;

        let candidate_ref = crate::attempts::checked_candidate_ref(&set.id, checked.index);
        let candidate_commit = Self::require_git_output(
            self.session_git(session_id, &["rev-parse", "--verify", &candidate_ref])
                .await?,
            "validating the checked candidate commit",
        )?;
        let candidate_tree_expression = format!("{candidate_ref}^{{tree}}");
        let candidate_tree = Self::require_git_output(
            self.session_git(session_id, &["rev-parse", &candidate_tree_expression])
                .await?,
            "validating the checked candidate tree",
        )?;
        if candidate_commit != checked.commit_oid || candidate_tree != checked.tree_oid {
            return Err(DaemonError::AttemptConflict(
                "the protected checked candidate changed; run Checks again".to_string(),
            ));
        }

        // Both commits have the attempt snapshot as their parent. merge-tree
        // therefore applies only base→candidate to the complete primary
        // preimage, while preserving unrelated work already in the workspace.
        let merge = self
            .session_git(
                session_id,
                &["merge-tree", "--write-tree", &preimage_ref, &candidate_ref],
            )
            .await?;
        if !merge.ok() {
            return Err(DaemonError::AttemptConflict(format!(
                "the workspace changed since these attempts started, and attempt {} conflicts with it: {}{}",
                checked.index + 1,
                merge.stdout.trim(),
                merge.stderr.trim()
            )));
        }
        let postimage_tree = merge.stdout.trim().to_string();
        if !matches!(postimage_tree.len(), 40 | 64)
            || !postimage_tree.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(DaemonError::Session(format!(
                "Git returned an invalid Keep postimage tree {postimage_tree:?}"
            )));
        }
        let postimage_ref = crate::attempts::keep_postimage_ref(&set.id);
        Self::require_git_output(
            self.session_git(session_id, &["update-ref", &postimage_ref, &postimage_tree])
                .await?,
            "publishing the Keep postimage",
        )?;

        let raw_path = apply_root.join("paths.raw");
        let raw_output = format!("--output={}", raw_path.to_string_lossy());
        Self::require_git_output(
            self.session_git(
                session_id,
                &[
                    "diff",
                    "--raw",
                    "-z",
                    "--no-renames",
                    &raw_output,
                    &preimage_tree,
                    &postimage_tree,
                ],
            )
            .await?,
            "enumerating the Keep transaction paths",
        )?;
        let raw = tokio::fs::read(&raw_path).await.map_err(|error| {
            DaemonError::Session(format!("reading Keep transaction paths: {error}"))
        })?;
        let _ = tokio::fs::remove_file(&raw_path).await;
        let (paths, changes_gitlink) = Self::parse_raw_tree_diff(&raw)?;
        if changes_gitlink || checked.changes_gitlink {
            return Err(DaemonError::AttemptConflict(
                "Axocoatl cannot safely Keep submodule/gitlink changes yet".to_string(),
            ));
        }
        if paths.is_empty() {
            return Err(DaemonError::AttemptConflict(format!(
                "attempt {} produced no change to Keep",
                checked.index + 1
            )));
        }

        let pre_entries = self
            .tree_entries_for_paths(session_id, &preimage_tree, &paths)
            .await?;
        let stage_root = apply_root.join("stage");
        let post_entries = self
            .materialize_keep_tree(session_id, set, &postimage_tree, &stage_root, &paths)
            .await?;
        let backup_root = apply_root.join("preimage");
        Self::reset_derived_directory(&backup_root).await?;
        let postimage_root = apply_root.join("postimage");
        Self::reset_derived_directory(&postimage_root).await?;

        let affected: HashSet<String> = paths.iter().cloned().collect();
        let mut plans = Vec::with_capacity(paths.len());
        for path in paths {
            let primary_path = workspace.join(&path);
            let preimage = Self::fingerprint_keep_workspace_path(
                workspace,
                std::path::Path::new(&path),
                &affected,
            )
            .await?;
            let postimage = Self::fingerprint_keep_workspace_path(
                &stage_root,
                std::path::Path::new(&path),
                &affected,
            )
            .await?;
            let pre_entry = pre_entries.get(&path);
            let post_entry = post_entries.get(&path);
            if pre_entry
                .is_some_and(|entry| !Self::keep_tree_leaf_matches_fingerprint(entry, &preimage))
            {
                return Err(DaemonError::AttemptConflict(format!(
                    "the primary Keep preimage kind or mode for {path:?} does not match its captured Git leaf"
                )));
            }
            if post_entry
                .is_some_and(|entry| !Self::keep_tree_leaf_matches_fingerprint(entry, &postimage))
                || (post_entry.is_none()
                    && postimage
                        .as_ref()
                        .is_some_and(|image| image.kind != "directory"))
            {
                return Err(DaemonError::Session(format!(
                    "the staged Keep postimage kind or mode for {path:?} does not match its protected Git leaf"
                )));
            }
            if pre_entry.is_none() && post_entry.is_some() && preimage.is_some() {
                let structural_directory = preimage
                    .as_ref()
                    .is_some_and(|image| image.kind == "directory")
                    && pre_entries
                        .keys()
                        .any(|candidate| candidate.starts_with(&format!("{path}/")));
                if !structural_directory {
                    return Err(DaemonError::AttemptConflict(format!(
                        "attempt {} would overwrite untracked path {path:?}",
                        checked.index + 1
                    )));
                }
            }
            if pre_entry.is_some() {
                Self::copy_keep_leaf(&primary_path, &backup_root.join(&path)).await?;
                let backup = Self::fingerprint_file(&backup_root.join(&path)).await?;
                if backup != preimage {
                    return Err(DaemonError::Session(format!(
                        "Keep preimage backup for {path:?} changed bytes"
                    )));
                }
            }
            if post_entry.is_some() {
                Self::copy_keep_leaf(&stage_root.join(&path), &postimage_root.join(&path)).await?;
            }
            plans.push(StoredKeepPath {
                path,
                preimage,
                postimage,
            });
        }

        // Prove the raw postimage store can reproduce every consumable stage
        // leaf before publishing the journal. From this point onward recovery
        // never checks the tree out again, so filter programs/container state
        // are not transaction dependencies.
        Self::rebuild_keep_stage_from_postimage(&apply_root, &plans, &post_entries).await?;

        Ok(StoredKeepApply {
            index: checked.index,
            patch_sha256: checked.patch_sha256.clone(),
            candidate_tree,
            preimage_tree,
            postimage_tree,
            paths: plans,
        })
    }

    async fn validate_keep_apply_refs(
        &self,
        session_id: &str,
        set: &crate::git::AttemptSet,
        apply: &StoredKeepApply,
    ) -> Result<(), DaemonError> {
        for (reference, expected) in [
            (
                crate::attempts::keep_preimage_ref(&set.id),
                apply.preimage_tree.as_str(),
            ),
            (
                crate::attempts::keep_postimage_ref(&set.id),
                apply.postimage_tree.as_str(),
            ),
            (
                crate::attempts::checked_candidate_ref(&set.id, apply.index),
                apply.candidate_tree.as_str(),
            ),
        ] {
            let expression = format!("{reference}^{{tree}}");
            let actual = Self::require_git_output(
                self.session_git(session_id, &["rev-parse", &expression])
                    .await?,
                "validating protected Keep objects",
            )?;
            if actual != expected {
                return Err(DaemonError::Session(format!(
                    "protected Keep object {reference:?} changed identity"
                )));
            }
        }
        Ok(())
    }

    async fn reconcile_keep_apply(
        &self,
        session_id: &str,
        set: &crate::git::AttemptSet,
        apply: &StoredKeepApply,
    ) -> Result<(), DaemonError> {
        self.validate_keep_apply_refs(session_id, set, apply)
            .await?;
        let mut seen = HashSet::new();
        let paths: Vec<String> = apply
            .paths
            .iter()
            .map(|plan| {
                Self::validate_keep_path(&plan.path)?;
                if !seen.insert(plan.path.clone()) {
                    return Err(DaemonError::Session(format!(
                        "Keep journal repeats path {:?}",
                        plan.path
                    )));
                }
                Ok(plan.path.clone())
            })
            .collect::<Result<_, DaemonError>>()?;
        let expected_raw_path = crate::attempts::keep_apply_root(
            &self
                .get_session(session_id)
                .await
                .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?
                .working_dir,
            session_id,
            &set.id,
        )
        .join("verify-paths.raw");
        let raw_output = format!("--output={}", expected_raw_path.to_string_lossy());
        Self::require_git_output(
            self.session_git(
                session_id,
                &[
                    "diff",
                    "--raw",
                    "-z",
                    "--no-renames",
                    &raw_output,
                    &apply.preimage_tree,
                    &apply.postimage_tree,
                ],
            )
            .await?,
            "validating Keep journal paths",
        )?;
        let expected_raw = tokio::fs::read(&expected_raw_path).await.map_err(|error| {
            DaemonError::Session(format!("reading validated Keep paths: {error}"))
        })?;
        let _ = tokio::fs::remove_file(&expected_raw_path).await;
        let (expected_paths, changes_gitlink) = Self::parse_raw_tree_diff(&expected_raw)?;
        if changes_gitlink || expected_paths != paths {
            return Err(DaemonError::Session(
                "Keep journal paths do not match its protected trees".to_string(),
            ));
        }

        let pre_entries = self
            .tree_entries_for_paths(session_id, &apply.preimage_tree, &paths)
            .await?;
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let sandbox = self.ensure_sandbox(&session).await?;
        let workspace = sandbox.root();
        let post_entries = self
            .tree_entries_for_paths(session_id, &apply.postimage_tree, &paths)
            .await?;
        let apply_root = crate::attempts::keep_apply_root(workspace, session_id, &set.id);
        let stage_root =
            Self::rebuild_keep_stage_from_postimage(&apply_root, &apply.paths, &post_entries)
                .await?;

        let mut pending = HashSet::new();
        for plan in &apply.paths {
            let fingerprint = Self::fingerprint_keep_workspace_path(
                workspace,
                std::path::Path::new(&plan.path),
                &seen,
            )
            .await?;
            // Current clean/smudge rules may themselves be one of the paths
            // already installed before a crash. Classifying with a fresh Git
            // blob would then reinterpret still-preimage bytes under postimage
            // attributes. Raw kind/mode/bytes are the durable WAL identity.
            let matches_post = Self::keep_fingerprint_matches(
                &fingerprint,
                &plan.postimage,
                !post_entries.contains_key(&plan.path),
            );
            if matches_post {
                continue;
            }
            let matches_pre = Self::keep_fingerprint_matches(
                &fingerprint,
                &plan.preimage,
                !pre_entries.contains_key(&plan.path),
            );
            if !matches_pre {
                return Err(DaemonError::AttemptConflict(format!(
                    "workspace path {:?} is neither the Keep preimage nor postimage; restore or move that edit before retrying Keep",
                    plan.path
                )));
            }
            pending.insert(plan.path.clone());
        }

        let mut deletions: Vec<&StoredKeepPath> = apply
            .paths
            .iter()
            .filter(|plan| pending.contains(&plan.path) && !post_entries.contains_key(&plan.path))
            .collect();
        deletions.sort_by_key(|plan| std::cmp::Reverse(plan.path.matches('/').count()));
        for plan in deletions {
            let relative = std::path::Path::new(&plan.path);
            let target = workspace.join(relative);
            let current = Self::fingerprint_keep_workspace_path(workspace, relative, &seen).await?;
            if current != plan.preimage {
                return Err(DaemonError::AttemptConflict(format!(
                    "workspace path {:?} changed during Keep",
                    plan.path
                )));
            }
            tokio::fs::remove_file(&target).await.map_err(|error| {
                DaemonError::Session(format!("deleting Keep path {:?}: {error}", plan.path))
            })?;
            Self::remove_empty_keep_parents(workspace, relative).await;
        }

        let mut additions: Vec<&StoredKeepPath> = apply
            .paths
            .iter()
            .filter(|plan| pending.contains(&plan.path) && post_entries.contains_key(&plan.path))
            .collect();
        additions.sort_by_key(|plan| plan.path.matches('/').count());
        for plan in additions {
            let relative = std::path::Path::new(&plan.path);
            let target = workspace.join(relative);
            let stage = stage_root.join(relative);
            let staged =
                Self::fingerprint_keep_workspace_path(&stage_root, relative, &seen).await?;
            if staged != plan.postimage {
                return Err(DaemonError::Session(format!(
                    "staged Keep postimage for {:?} changed bytes",
                    plan.path
                )));
            }
            Self::ensure_keep_parent_dirs(workspace, relative).await?;
            match tokio::fs::symlink_metadata(&target).await {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    let current =
                        Self::fingerprint_keep_workspace_path(workspace, relative, &seen).await?;
                    if current != plan.preimage {
                        return Err(DaemonError::AttemptConflict(format!(
                            "workspace directory {:?} changed during Keep",
                            plan.path
                        )));
                    }
                    tokio::fs::remove_dir(&target).await.map_err(|error| {
                        DaemonError::AttemptConflict(format!(
                            "Keep cannot replace non-empty directory {:?}: {error}",
                            plan.path
                        ))
                    })?;
                }
                Ok(_) => {
                    let current =
                        Self::fingerprint_keep_workspace_path(workspace, relative, &seen).await?;
                    if current != plan.preimage {
                        return Err(DaemonError::AttemptConflict(format!(
                            "workspace path {:?} changed during Keep",
                            plan.path
                        )));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if plan
                        .preimage
                        .as_ref()
                        .is_some_and(|image| image.kind != "directory")
                    {
                        return Err(DaemonError::AttemptConflict(format!(
                            "workspace path {:?} disappeared during Keep",
                            plan.path
                        )));
                    }
                }
                Err(error) => {
                    return Err(DaemonError::Session(format!(
                        "checking Keep destination {:?}: {error}",
                        plan.path
                    )))
                }
            }
            tokio::fs::rename(&stage, &target).await.map_err(|error| {
                DaemonError::Session(format!("installing Keep path {:?}: {error}", plan.path))
            })?;
        }

        // Validate every affected raw worktree leaf as one final phase. Do not
        // reinterpret bytes through the current clean/smudge configuration:
        // recovery deliberately cannot depend on filter programs that existed
        // only in the pre-crash container.
        for plan in &apply.paths {
            let fingerprint = Self::fingerprint_keep_workspace_path(
                workspace,
                std::path::Path::new(&plan.path),
                &seen,
            )
            .await?;
            if !Self::keep_fingerprint_matches(
                &fingerprint,
                &plan.postimage,
                !post_entries.contains_key(&plan.path),
            ) {
                return Err(DaemonError::Session(format!(
                    "Keep did not reach the protected postimage at {:?}",
                    plan.path
                )));
            }
        }
        Ok(())
    }

    /// Keep one attempt by applying only its delta onto the primary working
    /// tree. This does not commit or merge: the chosen changes return to the
    /// session for deliberate git review and commit.
    pub async fn adopt_variant(
        &self,
        session_id: &str,
        set_id: &str,
        index: usize,
    ) -> Result<crate::git::GitStatus, DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
        let current = self.peek_current_attempt_set(session_id).await?;
        let receipt_path =
            crate::attempts::keep_receipt_path(&session.working_dir, session_id, set_id)
                .to_string_lossy()
                .to_string();
        let receipt: Option<StoredKeepReceipt> =
            Self::read_host_json_file(std::path::Path::new(&receipt_path)).await?;
        if let Some(receipt) = receipt {
            if receipt.set_id != set_id || receipt.index != index {
                return Err(DaemonError::AttemptConflict(format!(
                    "attempt set '{set_id}' was already kept using a different attempt"
                )));
            }
            match current.as_ref() {
                None => return Ok(receipt.status),
                Some(current) if current.id != set_id => return Ok(receipt.status),
                Some(current) if Self::completed_keep_receipt_allows_cleanup(current, index)? => {
                    // Cleanup removes the set directory before current.json. A
                    // crash in that narrow window leaves the receipt and current
                    // roster as the only recovery inputs; both are sufficient
                    // to finish exact, idempotent cleanup without reopening the
                    // now-deleted apply journal.
                    Self::require_attempt_resolution_backend(&self.config.sandbox.backend)?;
                    self.ensure_attempt_recovery_sandbox(&session).await?;
                    self.remove_attempt_worktrees(session_id, current)
                        .await
                        .map_err(|error| {
                            DaemonError::Session(format!(
                                "attempt {} was kept, but cleanup is still incomplete: {error}",
                                index + 1
                            ))
                        })?;
                    return Ok(receipt.status);
                }
                Some(_) => {}
            }
        }
        let current = current.ok_or_else(|| {
            DaemonError::Session("this session has no current attempt set".to_string())
        })?;
        if current.id != set_id {
            return Err(DaemonError::AttemptConflict(format!(
                "attempt set '{set_id}' is stale; the current set is '{}'",
                current.id
            )));
        }
        Self::require_attempt_resolution_backend(&self.config.sandbox.backend)?;
        let sandbox = self.ensure_attempt_recovery_sandbox(&session).await?;
        let mut set = self.require_attempt_set(session_id, set_id).await?;
        if set.state == crate::git::AttemptSetState::Discarding {
            return Err(DaemonError::AttemptConflict(
                "this attempt set is already being discarded; retry Discard to finish cleanup"
                    .to_string(),
            ));
        }
        if !set.lanes.iter().any(|lane| lane.index == index) {
            return Err(DaemonError::Session(format!(
                "attempt {} does not exist",
                index + 1
            )));
        }
        if let Some(selected) = set.kept_index {
            if selected != index {
                return Err(DaemonError::AttemptConflict(format!(
                    "Keep is already in progress for attempt {}; retry that same attempt to finish",
                    selected + 1
                )));
            }
        } else if matches!(
            set.state,
            crate::git::AttemptSetState::Applying
                | crate::git::AttemptSetState::Applied
                | crate::git::AttemptSetState::TranscriptRecorded
        ) {
            return Err(DaemonError::Session(
                "attempt metadata is missing the selected Keep lane".to_string(),
            ));
        }

        let keep_phase = matches!(
            set.state,
            crate::git::AttemptSetState::Applying
                | crate::git::AttemptSetState::Applied
                | crate::git::AttemptSetState::TranscriptRecorded
        );
        let mut stored_apply = None;
        if !keep_phase {
            if !matches!(
                set.state,
                crate::git::AttemptSetState::Verified | crate::git::AttemptSetState::Judged
            ) {
                return Err(DaemonError::AttemptConflict(
                    "run Checks to completion before Keep".to_string(),
                ));
            }
            let (_, states) = self.require_terminal_attempt(session_id, set_id).await?;
            if !matches!(
                states
                    .iter()
                    .find(|state| state.index == index)
                    .map(|state| state.state),
                Some(crate::git::AttemptLaneState::Completed)
            ) {
                return Err(DaemonError::AttemptConflict(format!(
                    "attempt {} did not complete and cannot be kept",
                    index + 1
                )));
            }
            let attempt_root = crate::attempts::attempt_root(sandbox.root(), session_id, set_id)
                .to_string_lossy()
                .to_string();
            let verdicts: Vec<crate::git::LaneVerdict> = self
                .read_variant_meta(&sandbox, &attempt_root, "verdicts.json")
                .await?
                .ok_or_else(|| {
                    DaemonError::AttemptConflict("run Checks before keeping an attempt".to_string())
                })?;
            let verdict = verdicts
                .iter()
                .find(|verdict| verdict.index == index)
                .ok_or_else(|| {
                    DaemonError::AttemptConflict(format!(
                        "attempt {} has not been checked",
                        index + 1
                    ))
                })?;
            if !verdict.passed || verdict.changed_files == 0 {
                return Err(DaemonError::AttemptConflict(format!(
                    "attempt {} did not pass Checks with a non-empty change",
                    index + 1
                )));
            }
            let expected = verdict.patch_sha256.as_deref().ok_or_else(|| {
                DaemonError::AttemptConflict(
                    "Checks were recorded without a patch identity; run Checks again before Keep"
                        .to_string(),
                )
            })?;
            let checked_trees: Vec<StoredCheckedTree> = self
                .read_variant_meta(&sandbox, &attempt_root, "checked-trees.json")
                .await?
                .ok_or_else(|| {
                    DaemonError::AttemptConflict(
                        "Checks were recorded without protected candidate trees; run Checks again"
                            .to_string(),
                    )
                })?;
            let checked = checked_trees
                .iter()
                .find(|checked| checked.index == index)
                .ok_or_else(|| {
                    DaemonError::AttemptConflict(format!(
                        "attempt {} is missing its checked candidate tree; run Checks again",
                        index + 1
                    ))
                })?;
            if checked.patch_sha256 != expected || checked.changes_gitlink {
                return Err(DaemonError::AttemptConflict(format!(
                    "attempt {} checked identity is inconsistent or unsupported; run Checks again before Keep",
                    index + 1
                )));
            }
            // Checks already froze and imported the candidate. Stop any stale
            // process ownership before computing the primary pre/post trees.
            self.stop_attempt_runtime(session_id, &set).await?;
            let persisted = self.prepare_keep_apply(session_id, &set, checked).await?;
            self.write_variant_meta(session_id, set_id, "keep-apply.json", &persisted)
                .await?;
            set.state = crate::git::AttemptSetState::Applying;
            set.kept_index = Some(index);
            self.persist_attempt_set(&sandbox, &set).await?;
            stored_apply = Some(persisted);
        } else if matches!(
            set.state,
            crate::git::AttemptSetState::Applying
                | crate::git::AttemptSetState::Applied
                | crate::git::AttemptSetState::TranscriptRecorded
        ) {
            let attempt_root = crate::attempts::attempt_root(sandbox.root(), session_id, set_id)
                .to_string_lossy()
                .to_string();
            let persisted: StoredKeepApply = self
                .read_variant_meta(&sandbox, &attempt_root, "keep-apply.json")
                .await?
                .ok_or_else(|| {
                    DaemonError::Session(
                        "Keep metadata is missing its durable apply journal; refusing to guess workspace state"
                            .to_string(),
                    )
                })?;
            if persisted.index != index || persisted.patch_sha256.len() != 64 {
                return Err(DaemonError::Session(
                    "Keep apply metadata is inconsistent; refusing to mutate the workspace"
                        .to_string(),
                ));
            }
            stored_apply = Some(persisted);
        }

        // From here onward only protected primary-repository objects and the
        // durable per-path journal are authoritative. A retry never reopens or
        // trusts the mutable lane checkout.
        self.stop_attempt_runtime(session_id, &set).await?;

        if set.state == crate::git::AttemptSetState::Applying {
            let apply = stored_apply
                .as_ref()
                .ok_or_else(|| DaemonError::Session("Keep journal was not loaded".to_string()))?;
            self.reconcile_keep_apply(session_id, &set, apply).await?;
            set.state = crate::git::AttemptSetState::Applied;
            self.persist_attempt_set(&sandbox, &set).await?;
        }

        if set.state == crate::git::AttemptSetState::Applied {
            let apply = stored_apply
                .as_ref()
                .ok_or_else(|| DaemonError::Session("Keep journal was not loaded".to_string()))?;
            self.reconcile_keep_apply(session_id, &set, apply).await?;
        }

        if set.state != crate::git::AttemptSetState::TranscriptRecorded {
            let assistant = self
                .read_lane_output(&sandbox, &set, index)
                .await?
                .filter(|content| !content.trim().is_empty())
                .unwrap_or_else(|| {
                    format!(
                        "Kept attempt {}. Its changes are in the workspace for review.",
                        index + 1
                    )
                });
            self.append_kept_attempt_to_session(&session, &set, index, assistant, &sandbox)
                .await
                .map_err(|error| {
                    DaemonError::Session(format!(
                        "attempt {} was applied, but its chat turn could not be recorded: {error}",
                        index + 1
                    ))
                })?;
            set.state = crate::git::AttemptSetState::TranscriptRecorded;
            self.persist_attempt_set(&sandbox, &set).await?;
        }
        // Read the committed success response before deleting recovery state.
        // Status is presentation, not transaction authority: required Git
        // filters may have existed only inside the pre-crash container. Falling
        // back to the exact journal prevents a completed Keep from becoming
        // permanently unresolvable for that reason.
        let status = match self.git_status(session_id).await {
            Ok(status) => status,
            Err(error) => {
                tracing::warn!(
                    session_id,
                    set_id,
                    error = %error,
                    "using conservative Keep receipt because Git status is unavailable"
                );
                Self::conservative_keep_status(stored_apply.as_ref().ok_or_else(|| {
                    DaemonError::Session("Keep journal was not loaded".to_string())
                })?)
            }
        };
        Self::write_json_file(
            &sandbox,
            &receipt_path,
            &StoredKeepReceipt {
                set_id: set_id.to_string(),
                index,
                status: status.clone(),
            },
        )
        .await?;
        if let Err(error) = self.remove_attempt_worktrees(session_id, &set).await {
            return Err(DaemonError::Session(format!(
                "attempt {} was applied, but cleanup was incomplete: {error}",
                index + 1
            )));
        }
        Ok(status)
    }

    /// Cancel/join a running set if necessary, then remove only that set.
    pub async fn discard_attempt(&self, session_id: &str, set_id: &str) -> Result<(), DaemonError> {
        self.require_attempt_set(session_id, set_id).await?;
        let (_operation, _cancellation_requested) = self
            .lock_attempt_operation_for_cleanup(session_id, Some(set_id))
            .await?;
        let result = async {
            let set = self.require_attempt_set(session_id, set_id).await?;
            self.discard_attempt_locked(session_id, set).await
        }
        .await;
        self.clear_attempt_cancellation(session_id, set_id).await;
        result
    }

    async fn discard_attempt_locked(
        &self,
        session_id: &str,
        mut set: crate::git::AttemptSet,
    ) -> Result<(), DaemonError> {
        if matches!(
            set.state,
            crate::git::AttemptSetState::Applying
                | crate::git::AttemptSetState::Applied
                | crate::git::AttemptSetState::TranscriptRecorded
        ) {
            return Err(DaemonError::AttemptConflict(
                "Keep has already started for this set; retry Keep to finish its resumable apply/record/cleanup sequence"
                    .to_string(),
            ));
        }
        if set.state != crate::git::AttemptSetState::Discarding {
            let session = self
                .get_session(session_id)
                .await
                .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;
            let sandbox = self.ensure_sandbox(&session).await?;
            set.state = crate::git::AttemptSetState::Discarding;
            self.persist_attempt_set(&sandbox, &set).await?;
        }
        self.stop_attempt_runtime(session_id, &set).await?;
        self.remove_attempt_worktrees(session_id, &set).await?;
        let _ = self.session_store.lock().await.touch(session_id);
        Ok(())
    }

    /// Execute an instruction inside a session, streaming the agent's output
    /// (text, reasoning, and tool calls) to `sink` as it is produced. Used by
    /// the `/ws` `session` command for a live cockpit.
    pub async fn execute_session_streaming(
        &self,
        session_id: &str,
        input: &str,
        model_override: Option<String>,
        target_agent: Option<String>,
        sink: axocoatl_actor::StreamSink,
    ) -> Result<axocoatl_core::AgentOutput, DaemonError> {
        let operation = self.attempt_operation(session_id).await;
        let _operation = operation.lock().await;
        self.require_no_unresolved_attempt(session_id).await?;
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| DaemonError::Session(format!("session '{session_id}' not found")))?;

        let actor = match &session.mode {
            SessionMode::SingleAgent { agent_id } => self.session_actor(&session, agent_id).await?,
            SessionMode::Lattice { workflow_id } => {
                // Multi-agent: run the workflow's agents session-scoped,
                // sandboxed, in dependency order — streamed to the bus.
                return self
                    .execute_session_lattice(
                        &session,
                        workflow_id.as_deref(),
                        input,
                        model_override,
                        target_agent,
                    )
                    .await;
            }
            SessionMode::Custom { agents } => {
                // User-picked subset, still in topo order. Same execution
                // path as Lattice but with explicit agent list.
                if agents.is_empty() {
                    return Err(DaemonError::Session(
                        "Custom mode has no agents selected".into(),
                    ));
                }
                return self
                    .execute_session_agents(
                        &session,
                        agents.clone(),
                        input,
                        model_override,
                        target_agent,
                    )
                    .await;
            }
        };

        let output = axocoatl_actor::execute_agent_streaming(
            &actor,
            axocoatl_core::AgentInput::text(input).with_model_override(model_override),
            sink,
        )
        .await
        .map_err(DaemonError::AgentSpawn)?;

        let _ = self.session_store.lock().await.touch(session_id);
        Ok(output)
    }

    /// Run a multi-agent (lattice-mode) session: the workflow's agents,
    /// session-scoped and sharing the one session sandbox, executed in
    /// dependency order. Each agent's output streams to the bus keyed by the
    /// session id, so the cockpit + lattice panel see the org work live.
    async fn execute_session_lattice(
        &self,
        session: &Session,
        workflow_id: Option<&str>,
        input: &str,
        model_override: Option<String>,
        target_agent: Option<String>,
    ) -> Result<axocoatl_core::AgentOutput, DaemonError> {
        let workflow = match workflow_id {
            Some(wid) => self
                .config
                .workflows
                .iter()
                .find(|w| w.id == wid)
                .ok_or_else(|| DaemonError::Session(format!("workflow '{wid}' not found")))?,
            None => self
                .config
                .workflows
                .first()
                .ok_or_else(|| DaemonError::Session("no workflows configured".to_string()))?,
        };
        if workflow.agents.is_empty() {
            return Err(DaemonError::Session("workflow has no agents".to_string()));
        }

        let agents = workflow.agents.clone();
        self.execute_session_agents(session, agents, input, model_override, target_agent)
            .await
    }

    /// Run a specific list of agents inside a session, topologically ordered
    /// by their `depends_on`. Shared by `Lattice` and `Custom` modes.
    async fn execute_session_agents(
        &self,
        session: &Session,
        agents: Vec<String>,
        input: &str,
        model_override: Option<String>,
        target_agent: Option<String>,
    ) -> Result<axocoatl_core::AgentOutput, DaemonError> {
        let mut order = Self::topo_order(&agents, &self.config);
        // Per-turn target_agent: only that one runs (still respects topo).
        if let Some(target) = target_agent.as_deref() {
            if !order.iter().any(|a| a == target) {
                return Err(DaemonError::Session(format!(
                    "target agent '{target}' is not in this session"
                )));
            }
            order.retain(|a| a == target);
        }
        let bus = self.stream_bus.clone();
        let mut prior: Vec<(String, String)> = Vec::new();
        let mut last: Option<axocoatl_core::AgentOutput> = None;

        for agent_id in &order {
            let actor = self.session_actor(session, agent_id).await?;

            // Each agent sees the original instruction plus what upstream
            // agents have already produced.
            let agent_input = if prior.is_empty() {
                input.to_string()
            } else {
                let work = prior
                    .iter()
                    .map(|(a, o)| format!("### {a}\n{o}"))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                format!("{input}\n\n## Work already completed by other agents\n{work}")
            };

            let _ = bus.send(crate::stream::StreamFrame::Event {
                event_type: "AgentActivated".to_string(),
                agent: Some(agent_id.clone()),
                task: None,
                name: None,
                output: None,
                tokens: None,
                workflow: Some(session.id.clone()),
            });

            let out = self
                .run_session_agent_streamed(
                    &actor,
                    &session.id,
                    agent_id,
                    &agent_input,
                    model_override.clone(),
                )
                .await?;

            let _ = bus.send(crate::stream::StreamFrame::Event {
                event_type: "TaskCompleted".to_string(),
                agent: Some(agent_id.clone()),
                task: None,
                name: None,
                output: Some(out.content.chars().take(200).collect()),
                tokens: Some(out.token_usage.total() as u64),
                workflow: Some(session.id.clone()),
            });

            prior.push((agent_id.clone(), out.content.clone()));
            last = Some(out);
        }

        let _ = self.session_store.lock().await.touch(&session.id);
        last.ok_or_else(|| DaemonError::Session("no agents ran".to_string()))
    }

    /// Execute one agent, forwarding its stream chunks to the bus as frames
    /// keyed by `run_id` (the session id) and labelled with `agent_label`.
    async fn run_session_agent_streamed(
        &self,
        actor: &ractor::ActorRef<axocoatl_actor::AgentMessage>,
        run_id: &str,
        agent_label: &str,
        input: &str,
        model_override: Option<String>,
    ) -> Result<axocoatl_core::AgentOutput, DaemonError> {
        // The same recorder the attempts use. Here it answers a different
        // question — which files this turn touched — so the reviewer can filter
        // git by "what the agent just did" without that becoming a second
        // concept alongside staged and unstaged.
        let trace: Arc<StdMutex<Vec<crate::trajectory::Action>>> =
            Arc::new(StdMutex::new(Vec::new()));
        let out = Self::stream_agent_run(
            self.stream_bus.clone(),
            actor.clone(),
            run_id.to_string(),
            agent_label.to_string(),
            input.to_string(),
            StreamAgentRunOptions {
                model_override,
                trace: Some(trace.clone()),
                supplied_history: None,
            },
        )
        .await;

        // Only writes count. A turn that read twenty files and changed one has
        // changed one, and listing the reads would bury it.
        if let Ok(steps) = trace.lock() {
            let mut touched: Vec<String> = steps
                .iter()
                .filter(|a| {
                    matches!(
                        a.kind,
                        crate::trajectory::ActionKind::Edit | crate::trajectory::ActionKind::Write
                    ) && !a.failed
                        && !a.target.is_empty()
                })
                .map(|a| a.target.clone())
                .collect();
            touched.sort();
            touched.dedup();
            if let Ok(mut store) = self.session_last_turn.lock() {
                store.insert(run_id.to_string(), touched);
            }
        }
        out
    }

    /// Files the session's agent wrote in its most recent turn.
    pub fn session_last_turn_files(&self, session_id: &str) -> Vec<String> {
        self.session_last_turn
            .lock()
            .ok()
            .and_then(|s| s.get(session_id).cloned())
            .unwrap_or_default()
    }

    /// Drive one agent run, forwarding its stream chunks to the bus as frames
    /// keyed by `run_id` and labelled `agent_label`. Standalone (no `&self`)
    /// so it can run inside a spawned task — e.g. a variant lane keyed
    /// `{session}#{i}`.
    /// `trace`, when supplied, accumulates this run's normalised trajectory.
    ///
    /// Recorded here rather than by subscribing to the bus because the bus is a
    /// broadcast for viewers: frames there are dropped when nobody is listening
    /// and when a slow receiver lags. A trajectory that exists only if someone
    /// had the page open is not evidence, and the whole point of Tier 2 is to
    /// answer "how did they differ" *after* the run, when the tab that watched
    /// it is long gone.
    async fn stream_agent_run(
        bus: tokio::sync::broadcast::Sender<crate::stream::StreamFrame>,
        actor: ractor::ActorRef<axocoatl_actor::AgentMessage>,
        run_id: String,
        agent_label: String,
        input: String,
        options: StreamAgentRunOptions,
    ) -> Result<axocoatl_core::AgentOutput, DaemonError> {
        let StreamAgentRunOptions {
            model_override,
            trace,
            supplied_history,
        } = options;
        let (sink_tx, mut sink_rx) =
            tokio::sync::mpsc::unbounded_channel::<axocoatl_actor::AgentStreamChunk>();
        let fwd = {
            let bus = bus.clone();
            let rid = run_id.clone();
            let aid = agent_label.clone();
            let trace = trace.clone();
            tokio::spawn(async move {
                use crate::stream::StreamFrame as F;
                use axocoatl_actor::AgentStreamChunk as C;
                // Maps a tool call's id to its position in the trajectory, so the
                // result frame can mark the step that failed. A model can have
                // several calls open at once, so this cannot be "the last step".
                let mut at: HashMap<String, usize> = HashMap::new();
                while let Some(chunk) = sink_rx.recv().await {
                    if let Some(t) = &trace {
                        match &chunk {
                            C::ToolCallStarted {
                                id,
                                name,
                                arguments,
                            } => {
                                if let Ok(mut steps) = t.lock() {
                                    let seq = steps.len();
                                    steps.push(crate::trajectory::Action::from_call(
                                        seq,
                                        name,
                                        Some(arguments),
                                    ));
                                    at.insert(id.clone(), seq);
                                }
                            }
                            C::ToolCallResult { id, is_error, .. } if *is_error => {
                                if let (Ok(mut steps), Some(&i)) = (t.lock(), at.get(id)) {
                                    if let Some(step) = steps.get_mut(i) {
                                        step.failed = true;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    let frame = match chunk {
                        C::Text(d) => F::Token {
                            workflow: rid.clone(),
                            agent: aid.clone(),
                            delta: d,
                        },
                        C::Reasoning(d) => F::Reasoning {
                            workflow: rid.clone(),
                            agent: aid.clone(),
                            delta: d,
                        },
                        C::ToolCallStarted {
                            id,
                            name,
                            arguments,
                        } => F::ToolCall {
                            workflow: rid.clone(),
                            agent: aid.clone(),
                            call_id: id,
                            name,
                            phase: "start".to_string(),
                            arguments: Some(arguments),
                            result: None,
                            is_error: false,
                        },
                        C::ToolCallResult {
                            id,
                            name,
                            result,
                            is_error,
                        } => F::ToolCall {
                            workflow: rid.clone(),
                            agent: aid.clone(),
                            call_id: id,
                            name,
                            phase: "result".to_string(),
                            arguments: None,
                            result: Some(result),
                            is_error,
                        },
                    };
                    let _ = bus.send(frame);
                }
            })
        };
        let mut agent_input =
            axocoatl_core::AgentInput::text(input).with_model_override(model_override);
        if let Some(history) = supplied_history {
            // Attempts own no durable transcript. They receive one identical
            // request-local snapshot of the permanent session conversation,
            // preserving follow-up context and the full streamed tool loop
            // without writing candidate turns back into the canonical actor.
            agent_input = agent_input.with_supplied_history(history);
        }
        let out = axocoatl_actor::execute_agent_streaming(&actor, agent_input, sink_tx)
            .await
            .map_err(DaemonError::AgentSpawn)?;
        let _ = fwd.await;
        Ok(out)
    }

    /// Order a workflow's agents so every agent comes after its dependencies
    /// (Kahn's algorithm). Falls back to config order if there is a cycle.
    fn topo_order(agents: &[String], config: &AxocoatlConfig) -> Vec<String> {
        use std::collections::VecDeque;
        let member: HashSet<&str> = agents.iter().map(|s| s.as_str()).collect();
        let mut deps: HashMap<String, Vec<String>> = HashMap::new();
        let mut indeg: HashMap<String, usize> = HashMap::new();
        for a in agents {
            let d: Vec<String> = config
                .agents
                .iter()
                .find(|c| &c.id == a)
                .map(|c| {
                    c.depends_on
                        .iter()
                        .filter(|x| member.contains(x.as_str()))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            indeg.insert(a.clone(), d.len());
            deps.insert(a.clone(), d);
        }
        let mut queue: VecDeque<String> = agents
            .iter()
            .filter(|a| indeg.get(*a).copied().unwrap_or(0) == 0)
            .cloned()
            .collect();
        let mut order = Vec::new();
        while let Some(n) = queue.pop_front() {
            order.push(n.clone());
            for a in agents {
                if deps.get(a).map(|d| d.contains(&n)).unwrap_or(false) {
                    let e = indeg.get_mut(a).unwrap();
                    *e -= 1;
                    if *e == 0 {
                        queue.push_back(a.clone());
                    }
                }
            }
        }
        if order.len() == agents.len() {
            order
        } else {
            agents.to_vec()
        }
    }

    /// Get — spawning on first use — the session-scoped actor for `agent_id`.
    async fn session_actor(
        &self,
        session: &Session,
        agent_id: &str,
    ) -> Result<ractor::ActorRef<axocoatl_actor::AgentMessage>, DaemonError> {
        let scoped = format!("{}:{}", session.id, agent_id);
        let sid = AgentId::new(&scoped);
        if let Some(actor) = self.agent_registry.get(&sid).await {
            if self.agent_registry.is_alive(&sid).await {
                return Ok(actor);
            }
            self.agent_registry.remove(&sid).await;
        }
        let agent_yaml = self
            .config
            .agents
            .iter()
            .find(|a| a.id == agent_id)
            .ok_or_else(|| {
                DaemonError::Session(format!("agent '{agent_id}' is not in the config"))
            })?
            .clone();
        let sandbox = self.ensure_sandbox(session).await?;
        let context_dir = sandbox.root().to_path_buf();
        let executor = self.build_session_executor(session, sandbox, true).await;
        self.spawn_session_agent(
            session,
            &agent_yaml,
            &scoped,
            Arc::new(executor),
            &context_dir,
            true,
        )
        .await
    }

    /// Build the per-session tool executor: file/shell/terminal tools rooted
    /// at `sandbox`, the session's allowlisted skills (callable as tools), and
    /// web search when configured. Shared by the primary session actor and
    /// per-variant actors (which pass a worktree-rooted attached sandbox).
    async fn build_session_executor(
        &self,
        session: &Session,
        sandbox: Arc<dyn Sandbox>,
        include_integrations: bool,
    ) -> ToolExecutor {
        let mut executor = ToolExecutor::new();
        axocoatl_tools::register_session_tools(&mut executor, sandbox);
        if !include_integrations {
            // Parallel candidates may be thrown away. Until external tools have
            // set-scoped, reversible permission semantics, attempts are limited
            // to their isolated repository container rather than duplicating
            // writes to MCP servers, Skills, or search providers.
            return executor;
        }
        // Skills on the session's allowlist become callable tools — calling
        // one fires it into the lattice.
        for skill_id in &session.enabled_skills {
            if let Some(skill) = self.config.skills.iter().find(|g| &g.id == skill_id) {
                let tool =
                    crate::skill_tool::SkillTool::new(skill.clone(), self.event_lattice.clone());
                executor.register_builtin(tool.tool_name(), Arc::new(tool));
            }
        }
        // Web search — offered when a provider is configured.
        if let Some(ws) = &self.config.web_search {
            let tool = axocoatl_tools::WebSearchTool::from_config(
                &ws.provider,
                ws.api_key.expose_secret(),
            );
            executor.register_builtin("web_search", Arc::new(tool));
        }
        // Global MCP tools (discovered at bootstrap) are available to session
        // agents too, dispatched over the daemon's persistent connections.
        {
            let reg = self.mcp_registry.read().await;
            register_discovered_mcp_tools(&mut executor, &reg);
        }
        executor.with_mcp_registry(self.mcp_registry.clone())
    }

    /// Spawn a session-scoped agent actor named `{session}:{agent}`, bound to
    /// the per-session tool executor and given a working-directory preamble.
    async fn spawn_session_agent(
        &self,
        session: &Session,
        agent_yaml: &axocoatl_config::AgentConfigYaml,
        scoped_id: &str,
        tool_executor: Arc<ToolExecutor>,
        // The in-sandbox working dir shown to the model (`sandbox.root()`): the
        // host repo for Podman, the in-VM clone/worktree for E2B. Project
        // instructions are read separately from the host path (`session.working_dir`).
        context_dir: &std::path::Path,
        // Regular session actors participate in configured shared core blocks.
        // Attempts must not: parallel candidates mutating the same durable block
        // would influence one another despite isolated worktrees/transcripts.
        use_shared_core: bool,
    ) -> Result<ractor::ActorRef<axocoatl_actor::AgentMessage>, DaemonError> {
        let mut agent_config = agent_yaml.to_core();

        // Resolve the provider (per-agent Ollama, else the shared registry).
        let provider: Arc<dyn axocoatl_llm::LlmProvider> =
            self.resolve_provider(&agent_config.provider, Some(&agent_yaml.model))?;

        // The scoped id drives the actor name and the checkpoint key, so a
        // session's conversation is isolated from the global agent's.
        agent_config.id = AgentId::new(scoped_id);

        let core_store =
            Self::build_core(scoped_id, &self.data_dir, &agent_config.memory.core).await;
        let shared_core = if use_shared_core {
            Self::resolve_shared(&agent_config.memory.core, &self.shared_registry)
        } else {
            HashMap::new()
        };
        let behavior = DefaultAgentBehavior::new(provider, self.counter.clone())
            .with_checkpoint_store(self.checkpoint_store.clone())
            .with_tool_executor(tool_executor)
            .with_sampling(agent_config.sampling.clone())
            .with_core_memory(core_store, shared_core)
            .with_daily_log(Arc::new(axocoatl_memory::DailyLogMemory::new(
                scoped_id.to_string(),
                format!("{}/memory/daily_log", self.data_dir),
            )))
            .with_session_context(context_dir.display())
            // Shared/versioned team knowledge layer — walks up the HOST repo
            // (session.working_dir) collecting every AXOCOATL.md. Stays host-rooted
            // even for a remote session (the repo lives on the host; the in-VM
            // clone is of that same committed tree).
            .with_project_instructions(&session.working_dir);

        let (actor_ref, handle) = AgentActor::spawn(
            Some(scoped_id.to_string()),
            AgentActor,
            (agent_config, Box::new(behavior) as Box<dyn AgentBehavior>),
        )
        .await
        .map_err(|e| DaemonError::AgentSpawn(format!("{scoped_id}: {e}")))?;

        self.agent_registry
            .register(AgentId::new(scoped_id), actor_ref.clone())
            .await;
        if use_shared_core {
            self.agent_handles.lock().unwrap().push(handle);
        } else {
            // Attempt actors are owned and joined through `ActiveAttemptRun`.
            // Retaining every completed actor JoinHandle until daemon shutdown
            // would make repeated Explore/Keep cycles grow this global vector.
            drop(handle);
        }
        tracing::info!(session = %session.id, agent = %scoped_id, "Session agent spawned");
        Ok(actor_ref)
    }

    /// Gracefully shut down all agents and process-local attempt tasks.
    pub async fn shutdown(self) {
        // Attempt tasks are not ordinary supervised agents: their JoinHandles
        // own metadata writes that must finish before the runtime disappears.
        // Preserve their worktrees/current manifests for recovery, but stop and
        // join the process-local execution just as Discard would before deletion.
        let active_sets: Vec<(String, String)> = self
            .active_attempts
            .lock()
            .await
            .iter()
            .map(|(session, run)| (session.clone(), run.set_id.clone()))
            .collect();
        for (session_id, set_id) in active_sets {
            match self.require_attempt_set(&session_id, &set_id).await {
                Ok(set) => {
                    if let Err(error) = self.stop_attempt_runtime(&session_id, &set).await {
                        tracing::warn!(
                            session = %session_id,
                            attempt_set = %set_id,
                            error = %error,
                            "failed to join an attempt set during shutdown"
                        );
                    }
                }
                Err(error) => tracing::warn!(
                    session = %session_id,
                    attempt_set = %set_id,
                    error = %error,
                    "could not load an active attempt set during shutdown"
                ),
            }
        }

        let ids = self.agent_registry.list_ids().await;
        for id in &ids {
            if let Some(actor) = self.agent_registry.get(id).await {
                actor.stop(None);
            }
        }
        let handles = self.agent_handles.into_inner().unwrap_or_default();
        for handle in handles {
            let _ = handle.await;
        }
        tracing::info!("Axocoatl daemon shut down");
    }

    /// Number of running agents.
    pub async fn agent_count(&self) -> usize {
        self.agent_registry.count().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AxocoatlConfig {
        axocoatl_config::parse_config(
            r#"
agents:
  - id: test-agent
    name: "Test Agent"
    provider: mock
    model: test-model
    system_prompt: "You are a test agent."
    token_budget:
      per_execution: 10000
"#,
            &std::path::PathBuf::from("test.yaml"),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn bootstrap_fails_with_missing_provider() {
        let config = test_config();
        let result = AxocoatlDaemon::bootstrap(config).await;
        // Should fail because "mock" provider isn't registered
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("mock"),
            "Error should mention mock provider: {err}"
        );
    }

    #[test]
    fn cleanup_interrupts_running_lanes_and_checks_but_never_keep() {
        use crate::git::AttemptSetState;

        assert!(AxocoatlDaemon::attempt_state_is_interruptible(
            AttemptSetState::Running
        ));
        assert!(AxocoatlDaemon::attempt_state_is_interruptible(
            AttemptSetState::Checking
        ));
        assert!(AxocoatlDaemon::attempt_is_interrupt_target(
            Some("set-current"),
            "set-current",
            AttemptSetState::Running
        ));
        assert!(
            !AxocoatlDaemon::attempt_is_interrupt_target(
                Some("set-stale"),
                "set-current",
                AttemptSetState::Running
            ),
            "a stale Discard must never interrupt the current set"
        );
        for state in [
            AttemptSetState::Preparing,
            AttemptSetState::Ready,
            AttemptSetState::Verified,
            AttemptSetState::Judged,
            AttemptSetState::Failed,
            AttemptSetState::Discarding,
            AttemptSetState::Applying,
            AttemptSetState::Applied,
            AttemptSetState::TranscriptRecorded,
        ] {
            assert!(
                !AxocoatlDaemon::attempt_state_is_interruptible(state),
                "state {state:?} must not be interrupted out of band"
            );
        }
    }

    #[test]
    fn judge_contract_forbids_ties_and_names_exact_candidates() {
        let contract = judge_ranking_contract(&[0, 3, 7]);
        assert!(contract.contains("Candidate indexes are exactly [0, 3, 7]"));
        assert!(contract.contains("permutation of 1 through 3"));
        assert!(contract.contains("ties are forbidden"));
        assert!(contract.contains("winner must be the candidate whose rank is 1"));
        assert!(contract.contains("lower candidate index"));
    }

    #[tokio::test]
    async fn interrupted_cleanup_wait_acquires_a_released_workspace_lease() {
        let operation = Arc::new(tokio::sync::Mutex::new(()));
        let holder = operation.clone().lock_owned().await;
        let waiter = Box::pin(operation.lock_owned());
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(holder);
        });

        let guard = AxocoatlDaemon::wait_for_attempt_operation_after_interrupt(
            waiter,
            Duration::from_millis(250),
        )
        .await;
        assert!(guard.is_ok(), "cleanup must acquire the released lease");
        release.await.unwrap();
    }

    #[tokio::test]
    async fn interrupted_cleanup_wait_is_bounded_when_a_lease_will_not_release() {
        let operation = Arc::new(tokio::sync::Mutex::new(()));
        let _holder = operation.clone().lock_owned().await;
        let waiter = Box::pin(operation.lock_owned());
        let started = std::time::Instant::now();

        let result = AxocoatlDaemon::wait_for_attempt_operation_after_interrupt(
            waiter,
            Duration::from_millis(20),
        )
        .await;
        assert!(matches!(result, Err(DaemonError::AttemptConflict(_))));
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "cleanup must return a retryable conflict instead of hanging"
        );
    }

    #[test]
    fn baseline_cost_requires_every_lanes_real_token_volume() {
        let known = crate::git::LaneUsage {
            index: 0,
            model: Some("remote-model".to_string()),
            input_tokens: 100,
            output_tokens: 20,
            token_usage_known: true,
            cost_usd: 0.001,
            cost_known: true,
            duration_ms: 10,
        };
        let unknown = crate::git::LaneUsage {
            index: 1,
            model: Some("remote-model".to_string()),
            input_tokens: 0,
            output_tokens: 0,
            token_usage_known: false,
            cost_usd: 0.0,
            cost_known: false,
            duration_ms: 10,
        };

        assert!(AxocoatlDaemon::complete_lane_token_volume(
            std::slice::from_ref(&known),
            1
        ));
        assert!(!AxocoatlDaemon::complete_lane_token_volume(
            std::slice::from_ref(&known),
            2
        ));
        assert!(!AxocoatlDaemon::complete_lane_token_volume(
            &[known, unknown],
            2
        ));
    }

    #[tokio::test]
    async fn host_attempt_reads_restore_outputs_without_a_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(
            dir.path().join("output-1.json"),
            r#"{"index":1,"content":"second"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            dir.path().join("output-0.json"),
            r#"{"index":0,"content":"first"}"#,
        )
        .await
        .unwrap();

        let outputs = AxocoatlDaemon::read_lane_outputs_host(dir.path(), &[1, 0])
            .await
            .unwrap();
        assert_eq!(
            outputs,
            vec![
                crate::git::AttemptLaneOutput {
                    index: 0,
                    content: "first".to_string(),
                },
                crate::git::AttemptLaneOutput {
                    index: 1,
                    content: "second".to_string(),
                },
            ]
        );
    }

    #[test]
    fn raw_checked_tree_diff_preserves_literal_paths_and_rejects_gitlinks() {
        let raw = concat!(
            ":100644 100644 1111111 2222222 M\0:(literal)x\0",
            ":000000 100755 0000000 3333333 A\0dir/file name\n\0"
        );
        let (paths, gitlink) = AxocoatlDaemon::parse_raw_tree_diff(raw.as_bytes()).unwrap();
        assert_eq!(
            paths,
            vec![":(literal)x".to_string(), "dir/file name\n".to_string()]
        );
        assert!(!gitlink);

        let gitlink = b":160000 160000 1111111 2222222 M\0submodule\0";
        assert!(AxocoatlDaemon::parse_raw_tree_diff(gitlink).unwrap().1);
        let invalid_path = b":100644 100644 1111111 2222222 M\0bad-\xff\0";
        assert!(matches!(
            AxocoatlDaemon::parse_raw_tree_diff(invalid_path),
            Err(DaemonError::AttemptConflict(_))
        ));
    }

    #[test]
    fn keep_tree_lookup_treats_git_pathspec_magic_as_a_literal_filename() {
        use std::process::Command;

        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .output()
                .unwrap()
        };
        assert!(git(&["init", "-q"]).status.success());
        std::fs::write(repo.path().join(":(literal)x"), b"literal").unwrap();
        assert!(git(&["add", "-A"]).status.success());
        let tree = git(&["write-tree"]);
        assert!(tree.status.success());
        let tree = String::from_utf8(tree.stdout).unwrap();
        let output = git(&[
            "--literal-pathspecs",
            "ls-tree",
            "-r",
            "-z",
            tree.trim(),
            "--",
            ":(literal)x",
        ]);
        assert!(output.status.success());
        assert!(output.stdout.ends_with(b"\t:(literal)x\0"));
    }

    #[test]
    fn structural_directory_is_a_resumable_keep_intermediate() {
        let directory = Some(StoredFileFingerprint {
            kind: "directory".to_string(),
            sha256: String::new(),
            executable: false,
        });
        assert!(AxocoatlDaemon::keep_fingerprint_matches(
            &None, &directory, true
        ));
        assert!(!AxocoatlDaemon::keep_fingerprint_matches(
            &None, &directory, false
        ));
        assert!(!AxocoatlDaemon::keep_fingerprint_matches(
            &Some(StoredFileFingerprint {
                kind: "file".to_string(),
                sha256: "changed".to_string(),
                executable: false,
            }),
            &directory,
            true
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn keep_fingerprints_preserve_raw_bytes_and_never_follow_parent_symlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        tokio::fs::write(outside.path().join("child"), b"outside-secret")
            .await
            .unwrap();
        symlink(outside.path(), workspace.path().join("link")).unwrap();
        let affected = HashSet::from(["link".to_string(), "link/child".to_string()]);
        assert_eq!(
            AxocoatlDaemon::fingerprint_keep_workspace_path(
                workspace.path(),
                std::path::Path::new("link/child"),
                &affected,
            )
            .await
            .unwrap(),
            None,
            "an affected parent symlink makes its descendant logically absent"
        );

        let raw_path = workspace.path().join("raw");
        tokio::fs::write(&raw_path, [0xff, b'\n']).await.unwrap();
        let mut permissions = tokio::fs::metadata(&raw_path).await.unwrap().permissions();
        permissions.set_mode(0o755);
        tokio::fs::set_permissions(&raw_path, permissions)
            .await
            .unwrap();
        let fingerprint = AxocoatlDaemon::fingerprint_file(&raw_path)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fingerprint.kind, "file");
        assert_eq!(
            fingerprint.sha256,
            AxocoatlDaemon::bytes_sha256(&[0xff, b'\n'])
        );
        assert!(fingerprint.executable);
    }

    #[cfg(unix)]
    #[test]
    fn keep_checkout_forces_symlink_semantics_and_validates_tree_modes() {
        use std::os::unix::fs::symlink;
        use std::process::Command;

        let repo = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .output()
                .unwrap()
        };
        assert!(run(&["init", "-q"]).status.success());
        std::fs::write(repo.path().join("target"), b"target").unwrap();
        symlink("target", repo.path().join("link")).unwrap();
        assert!(run(&["add", "target", "link"]).status.success());
        let tree = String::from_utf8(run(&["write-tree"]).stdout).unwrap();
        assert!(run(&["config", "core.symlinks", "false"]).status.success());

        let stage = tempfile::tempdir().unwrap();
        let index = repo.path().join("keep.index");
        let git_dir = repo.path().join(".git");
        let read_tree = Command::new("git")
            .env("GIT_INDEX_FILE", &index)
            .arg("-C")
            .arg(repo.path())
            .args(["read-tree", tree.trim()])
            .output()
            .unwrap();
        assert!(read_tree.status.success());
        let checkout = Command::new("git")
            .env("GIT_DIR", &git_dir)
            .env("GIT_INDEX_FILE", &index)
            .env("GIT_WORK_TREE", stage.path())
            .args([
                "-c",
                "core.filemode=true",
                "-c",
                "core.symlinks=true",
                "checkout-index",
                "--force",
                "--all",
            ])
            .output()
            .unwrap();
        assert!(checkout.status.success());
        assert!(std::fs::symlink_metadata(stage.path().join("link"))
            .unwrap()
            .file_type()
            .is_symlink());

        let symlink_fingerprint = Some(StoredFileFingerprint {
            kind: "symlink".to_string(),
            sha256: "target".to_string(),
            executable: false,
        });
        let regular_fingerprint = Some(StoredFileFingerprint {
            kind: "file".to_string(),
            sha256: "target".to_string(),
            executable: false,
        });
        assert!(AxocoatlDaemon::keep_tree_leaf_matches_fingerprint(
            "120000 blob deadbeef",
            &symlink_fingerprint
        ));
        assert!(!AxocoatlDaemon::keep_tree_leaf_matches_fingerprint(
            "120000 blob deadbeef",
            &regular_fingerprint
        ));
        assert!(AxocoatlDaemon::keep_tree_leaf_matches_fingerprint(
            "100644 blob deadbeef",
            &regular_fingerprint
        ));
        assert!(!AxocoatlDaemon::keep_tree_leaf_matches_fingerprint(
            "100755 blob deadbeef",
            &regular_fingerprint
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn keep_stage_resumes_from_immutable_raw_postimages() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let transaction = tempfile::tempdir().unwrap();
        let apply_root = transaction.path();
        let postimage_root = apply_root.join("postimage");
        tokio::fs::create_dir_all(postimage_root.join("dir"))
            .await
            .unwrap();
        tokio::fs::write(postimage_root.join("raw"), [0xff, 0x00, b'\n'])
            .await
            .unwrap();
        let mut permissions = tokio::fs::metadata(postimage_root.join("raw"))
            .await
            .unwrap()
            .permissions();
        permissions.set_mode(0o755);
        tokio::fs::set_permissions(postimage_root.join("raw"), permissions)
            .await
            .unwrap();
        tokio::fs::write(postimage_root.join("dir/child"), b"post-child")
            .await
            .unwrap();
        symlink("raw", postimage_root.join("link")).unwrap();

        let paths = ["deleted", "dir", "dir/child", "link", "raw"];
        let affected: HashSet<String> = paths.iter().map(|path| (*path).to_string()).collect();
        let mut plans = Vec::new();
        for path in paths {
            plans.push(StoredKeepPath {
                path: path.to_string(),
                preimage: None,
                postimage: AxocoatlDaemon::fingerprint_keep_workspace_path(
                    &postimage_root,
                    std::path::Path::new(path),
                    &affected,
                )
                .await
                .unwrap(),
            });
        }
        let post_entries = HashMap::from([
            ("dir/child".to_string(), "100644:child".to_string()),
            ("link".to_string(), "120000:link".to_string()),
            ("raw".to_string(), "100755:raw".to_string()),
        ]);

        let stage =
            AxocoatlDaemon::rebuild_keep_stage_from_postimage(apply_root, &plans, &post_entries)
                .await
                .unwrap();
        assert_eq!(
            tokio::fs::read(stage.join("raw")).await.unwrap(),
            [0xff, 0x00, b'\n']
        );
        assert!(
            tokio::fs::metadata(stage.join("raw"))
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o111
                != 0
        );
        assert_eq!(
            tokio::fs::read_link(stage.join("link")).await.unwrap(),
            std::path::Path::new("raw")
        );

        // Model cancellation after atomic installs consumed two staged leaves.
        let workspace = apply_root.join("workspace");
        tokio::fs::create_dir_all(workspace.join("dir"))
            .await
            .unwrap();
        tokio::fs::rename(stage.join("raw"), workspace.join("raw"))
            .await
            .unwrap();
        tokio::fs::rename(stage.join("dir/child"), workspace.join("dir/child"))
            .await
            .unwrap();

        // A restart reconstructs the exact consumable stage from raw backups;
        // no Git checkout, filter, or old container is involved.
        let rebuilt =
            AxocoatlDaemon::rebuild_keep_stage_from_postimage(apply_root, &plans, &post_entries)
                .await
                .unwrap();
        assert_eq!(
            tokio::fs::read(rebuilt.join("raw")).await.unwrap(),
            [0xff, 0x00, b'\n']
        );
        assert_eq!(
            tokio::fs::read(rebuilt.join("dir/child")).await.unwrap(),
            b"post-child"
        );

        // Corruption of any durable leaf is detected before the existing
        // stage is reset, which in reconciliation is before workspace writes.
        tokio::fs::write(rebuilt.join("raw"), b"stage-sentinel")
            .await
            .unwrap();
        tokio::fs::write(postimage_root.join("dir/child"), b"corrupt")
            .await
            .unwrap();
        assert!(AxocoatlDaemon::rebuild_keep_stage_from_postimage(
            apply_root,
            &plans,
            &post_entries,
        )
        .await
        .is_err());
        assert_eq!(
            tokio::fs::read(rebuilt.join("raw")).await.unwrap(),
            b"stage-sentinel"
        );
    }

    #[test]
    fn keep_retry_uses_raw_preimage_even_if_attributes_reinterpret_its_git_blob() {
        let preimage = Some(StoredFileFingerprint {
            kind: "file".to_string(),
            sha256: AxocoatlDaemon::bytes_sha256(b"old\r\n"),
            executable: false,
        });
        let postimage = Some(StoredFileFingerprint {
            kind: "file".to_string(),
            sha256: AxocoatlDaemon::bytes_sha256(b"new\n"),
            executable: false,
        });
        let current = preimage.clone();
        assert!(!AxocoatlDaemon::keep_fingerprint_matches(
            &current, &postimage, false
        ));
        assert!(AxocoatlDaemon::keep_fingerprint_matches(
            &current, &preimage, false
        ));
    }

    #[test]
    fn completed_keep_has_a_conservative_status_without_git_filters() {
        let file = |sha256: &str| {
            Some(StoredFileFingerprint {
                kind: "file".to_string(),
                sha256: sha256.to_string(),
                executable: false,
            })
        };
        let apply = StoredKeepApply {
            index: 0,
            patch_sha256: "a".repeat(64),
            candidate_tree: "b".repeat(40),
            preimage_tree: "c".repeat(40),
            postimage_tree: "d".repeat(40),
            paths: vec![
                StoredKeepPath {
                    path: "added".to_string(),
                    preimage: None,
                    postimage: file("post"),
                },
                StoredKeepPath {
                    path: "deleted".to_string(),
                    preimage: file("pre"),
                    postimage: None,
                },
                StoredKeepPath {
                    path: "modified".to_string(),
                    preimage: file("before"),
                    postimage: file("after"),
                },
            ],
        };

        let status = AxocoatlDaemon::conservative_keep_status(&apply);
        assert!(!status.clean);
        assert_eq!(status.branch, "HEAD");
        assert_eq!(
            status
                .files
                .iter()
                .map(|file| (file.path.as_str(), file.state.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("added", "added"),
                ("deleted", "deleted"),
                ("modified", "modified")
            ]
        );
        assert!(status.files.iter().all(|file| {
            file.unstaged && !file.staged && file.added.is_none() && file.removed.is_none()
        }));
    }

    #[test]
    fn completed_keep_receipt_can_finish_cleanup_after_set_root_loss() {
        let mut set = crate::git::AttemptSet {
            id: "set".to_string(),
            session_id: "session".to_string(),
            task: "task".to_string(),
            instruction: "task".to_string(),
            base_sha: "1".repeat(40),
            base_tree: "2".repeat(40),
            state: crate::git::AttemptSetState::TranscriptRecorded,
            kept_index: Some(1),
            created_at: 1,
            lanes: Vec::new(),
        };
        assert!(AxocoatlDaemon::completed_keep_receipt_allows_cleanup(&set, 1).unwrap());

        // A receipt cannot authorize cleanup of a set whose transcript phase
        // was never durably reached.
        set.state = crate::git::AttemptSetState::Applied;
        assert!(!AxocoatlDaemon::completed_keep_receipt_allows_cleanup(&set, 1).unwrap());
        set.state = crate::git::AttemptSetState::TranscriptRecorded;
        assert!(AxocoatlDaemon::completed_keep_receipt_allows_cleanup(&set, 0).is_err());
    }

    #[test]
    fn unresolved_local_attempt_requires_local_backend_after_restart() {
        assert!(AxocoatlDaemon::require_attempt_resolution_backend("podman").is_ok());
        let error = AxocoatlDaemon::require_attempt_resolution_backend("e2b")
            .unwrap_err()
            .to_string();
        assert!(error.contains("switch"));
        assert!(error.contains("Podman"));
        assert!(error.contains("Keep"));
        assert!(error.contains("Discard"));
    }

    #[tokio::test]
    async fn durable_local_attempt_pointer_remains_visible_after_backend_change() {
        let workspace = tempfile::tempdir().unwrap();
        let session_id = "session-after-restart";
        let set_id = "0190aabb-ccdd-7eef-8899-aabbccddeeff";
        let set = crate::git::AttemptSet {
            id: set_id.to_string(),
            session_id: session_id.to_string(),
            task: "try ways".to_string(),
            instruction: "try ways".to_string(),
            base_sha: "1".repeat(40),
            base_tree: "2".repeat(40),
            state: crate::git::AttemptSetState::Verified,
            kept_index: None,
            created_at: 1,
            lanes: vec![crate::git::Variant {
                index: 0,
                branch: crate::attempts::branch_name(set_id, 0),
                worktree: crate::attempts::worktree_path(workspace.path(), session_id, set_id, 0)
                    .to_string_lossy()
                    .to_string(),
                model: None,
                agent: None,
                provider: None,
            }],
        };
        let current = crate::attempts::session_attempts_root(workspace.path(), session_id)
            .join("current.json");
        tokio::fs::create_dir_all(current.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&current, serde_json::to_vec(&set).unwrap())
            .await
            .unwrap();

        let recovered = AxocoatlDaemon::read_current_attempt_set_host(workspace.path(), session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.id, set_id);
        assert!(AxocoatlDaemon::require_attempt_resolution_backend("e2b").is_err());
    }
}
