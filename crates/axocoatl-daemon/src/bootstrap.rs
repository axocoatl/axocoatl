//! Daemon bootstrap: config → providers → agents → coordination.
//! This is the integration point that wires all subsystems together.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use tokio::sync::mpsc;

use axocoatl_actor::{AgentActor, AgentBehavior, AgentRegistry, DefaultAgentBehavior};
use axocoatl_config::AxocoatlConfig;
use axocoatl_coordination::{EventId, EventLattice, EventType, LatticeEvent};
use axocoatl_core::AgentId;
use axocoatl_isolation::session_sandbox::SessionSandbox;
use axocoatl_llm::ProviderRegistry;
use axocoatl_mcp::approval::{McpApprovalGate, SharedApprovalGate};
use axocoatl_mcp::permissions::McpPermissionStore;
use axocoatl_mcp::{McpToolRegistry, McpTransportType};
use axocoatl_memory::chat::ChatStore;
use axocoatl_memory::files::FileStore;
use axocoatl_memory::{CheckpointPolicy, CheckpointStore, LongTermMemory};
use axocoatl_session::{Session, SessionMode, SessionStore};
use axocoatl_token::{ApproximateCounter, TokenCounter};
use axocoatl_tools::ToolExecutor;
use ractor::Actor;

use crate::activation::{self, ActivationRequest};
use crate::error::DaemonError;
use crate::scheduler::ScheduleTable;
use crate::workflow::{WorkflowExecution, WorkflowOutput};

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
    /// Live state of every proactive agent — populated by
    /// `start_proactive_runners`, exposed via `/api/proactive`.
    pub proactive_table: crate::proactive::ProactiveTable,
    /// Persistent store of directory sessions.
    pub session_store: Arc<tokio::sync::Mutex<SessionStore>>,
    /// Persistent store of lightweight chats (the Chat tab — no directory,
    /// no sandbox). Loaded from {data_dir}/chats/*.json at boot. Atomic
    /// temp+rename JSON writes per chat — see [`ChatStore::persist`].
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
    /// Live HITL interrupts. When an Interrupt node fires, it parks here
    /// keyed by `{automation_id}:{run_id}:{node_id}` and the executor
    /// blocks on `notify.notified()`. The dashboard surfaces these as
    /// pending; `POST /api/automations/{id}/runs/{run_id}/resume` wakes them.
    pub pending_interrupts: Arc<
        tokio::sync::RwLock<std::collections::HashMap<String, crate::interrupt::PendingInterrupt>>,
    >,
    /// Per-automation run history + checkpoints — the time-travel store.
    /// Writes happen from inside the executor after every node completion.
    pub run_store: Arc<crate::automation_runs::AutomationRunStore>,
    /// Live OCI sandbox containers, keyed by session id.
    session_sandboxes: Arc<tokio::sync::Mutex<HashMap<String, Arc<SessionSandbox>>>>,
    /// Ring buffer of the most recent lattice events (capped at 200).
    pub event_log: Arc<StdMutex<VecDeque<LatticeEvent>>>,
    /// The observability stream bus — flattened events + live agent tokens.
    /// Every dashboard WebSocket subscribes to this.
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
    pub tool_executor: Arc<ToolExecutor>,
    long_term_memory: Arc<tokio::sync::RwLock<LongTermMemory>>,
    activation_tx: mpsc::UnboundedSender<ActivationRequest>,
    activation_handle: Option<tokio::task::JoinHandle<()>>,
    agent_handles: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for AxocoatlDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AxocoatlDaemon")
            .field("agents", &self.config.agents.len())
            .finish_non_exhaustive()
    }
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

        // 3. Set up shared long-term memory (Tier 3)
        let ltm_path = format!("{data_dir}/memory/long_term.bin");
        let mut ltm = LongTermMemory::new(&ltm_path);
        if let Err(e) = ltm.load().await {
            tracing::warn!(error = %e, "Failed to load long-term memory, starting fresh");
        } else if !ltm.is_empty() {
            tracing::info!(entries = ltm.len(), "Loaded long-term memory");
        }
        let long_term_memory = Arc::new(tokio::sync::RwLock::new(ltm));

        // 5. Set up tool executor with built-in tools
        let mut tool_executor = ToolExecutor::new();
        tool_executor.register_builtin("echo", Arc::new(axocoatl_tools::EchoTool));
        tool_executor.register_builtin("json_keys", Arc::new(axocoatl_tools::JsonKeysTool));
        tool_executor.register_builtin("text_split", Arc::new(axocoatl_tools::TextSplitTool));
        let tool_executor = Arc::new(tool_executor);

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

        // 7. Spawn agents (deferred from earlier so the hook registry exists)
        let mut agent_handles = Vec::new();
        for agent_yaml in &config.agents {
            let handle = Self::spawn_agent(
                agent_yaml,
                &config,
                &provider_registry,
                &counter,
                &checkpoint_store,
                &tool_executor,
                &long_term_memory,
                &agent_registry,
                &hook_registry,
            )
            .await?;
            agent_handles.push(handle);
        }

        // 8. Set up event lattice for stigmergic coordination
        let event_lattice = Arc::new(EventLattice::new(256));

        for agent_yaml in &config.agents {
            let agent_id = AgentId::new(&agent_yaml.id);
            // Entry agents are activated directly by execute_workflow().
            // Downstream agents activate on accumulated TaskCompleted signals (0.5 each).
            // Threshold = N * 0.5 where N = number of dependencies; use 1.0 for
            // unreachable-via-cascade agents (defensive default).
            let (threshold, decay_rate) = if agent_yaml.depends_on.is_empty() {
                (1.0_f32, 0.0_f32)
            } else {
                let n = agent_yaml.depends_on.len() as f32;
                (n * 0.5, 0.01)
            };
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

        // 10. Spawn the activation loop
        let (activation_tx, activation_rx) = mpsc::unbounded_channel();
        let activation_handle = tokio::spawn(activation::run_activation_loop(
            activation_rx,
            agent_registry.clone(),
            event_lattice.clone(),
            activation_tx.clone(),
            config.agents.clone(),
            stream_bus.clone(),
        ));

        // 11. Spawn the event subscriber — keeps the last 200 lattice events in
        // a ring buffer AND bridges every event onto the stream bus so the
        // dashboard WebSocket sees the full coordination feed.
        let event_log: Arc<StdMutex<VecDeque<LatticeEvent>>> =
            Arc::new(StdMutex::new(VecDeque::with_capacity(200)));
        let log_for_task = event_log.clone();
        let mut event_rx = event_lattice.subscribe();
        let lattice_for_task = event_lattice.clone();
        let bus_for_bridge = stream_bus.clone();
        tokio::spawn(async move {
            while let Ok(notif) = event_rx.recv().await {
                // Bridge to the stream bus for the dashboard.
                let _ = bus_for_bridge.send(crate::stream::event_frame(&notif));
                // Keep the ring buffer for the event timeline.
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

        // Unified Automation store. Lives at {data_dir}/automations.json.
        // First-boot seed: project the legacy YAML sections through
        // `Automation::from_legacy` into the store. Subsequent boots use
        // the file as-is — the dashboard editor is the authority.
        let automation_store = {
            let path = std::path::PathBuf::from(format!("{data_dir}/automations.json"));
            let mut store = crate::automation_store::AutomationStore::open(&path)
                .map_err(|e| DaemonError::Session(e.to_string()))?;
            match store.seed_from_legacy_if_empty(&config) {
                Ok(true) => tracing::info!(
                    automations = store.len(),
                    "seeded automation store from legacy YAML sections"
                ),
                Ok(false) => tracing::debug!(
                    automations = store.len(),
                    "automation store already populated; skipping legacy seed"
                ),
                Err(e) => tracing::warn!(error = %e, "automation store seed failed"),
            }
            Arc::new(tokio::sync::RwLock::new(store))
        };

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
            automation_store,
            pending_interrupts: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            run_store,
            session_sandboxes: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            event_log,
            stream_bus,
            active_runs,
            tool_executor,
            long_term_memory,
            activation_tx,
            activation_handle: Some(activation_handle),
            agent_handles: std::sync::Mutex::new(agent_handles),
        })
    }

    /// Spawn a single agent: build its provider + behavior, start the actor,
    /// and register it. Shared by bootstrap and `restart_agent`.
    #[allow(clippy::too_many_arguments)]
    async fn spawn_agent(
        agent_yaml: &axocoatl_config::AgentConfigYaml,
        config: &AxocoatlConfig,
        provider_registry: &ProviderRegistry,
        counter: &Arc<dyn TokenCounter>,
        checkpoint_store: &Arc<CheckpointStore>,
        tool_executor: &Arc<ToolExecutor>,
        long_term_memory: &Arc<tokio::sync::RwLock<LongTermMemory>>,
        agent_registry: &AgentRegistry,
        hook_registry: &Arc<axocoatl_tools::HookRegistry>,
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

        let mut behavior = DefaultAgentBehavior::new(provider, counter.clone())
            .with_checkpoint_store(checkpoint_store.clone())
            .with_tool_executor(tool_executor.clone())
            .with_long_term_memory(long_term_memory.clone())
            .with_hook_registry(hook_registry.clone());

        // Tier 4 semantic memory — one store per agent, for cross-session
        // recall. A failure here is non-fatal: the agent runs without it.
        let data_dir = std::env::var("AXOCOATL_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
        match axocoatl_memory::SemanticMemory::new(
            &agent_id.to_string(),
            format!("{data_dir}/memory/semantic"),
        ) {
            Ok(sem) => behavior = behavior.with_semantic_memory(Arc::new(sem)),
            Err(e) => {
                tracing::warn!(agent = %agent_id, error = %e, "semantic memory unavailable")
            }
        }

        let (actor_ref, handle) = AgentActor::spawn(
            Some(agent_id.to_string()),
            AgentActor,
            (agent_config, Box::new(behavior) as Box<dyn AgentBehavior>),
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
            &self.long_term_memory,
            &self.agent_registry,
            &self.hook_registry,
        )
        .await?;
        self.agent_handles.lock().unwrap().push(handle);

        // Re-register in the event lattice with the same threshold rules.
        let (threshold, decay_rate) = if agent_yaml.depends_on.is_empty() {
            (1.0_f32, 0.0_f32)
        } else {
            (agent_yaml.depends_on.len() as f32 * 0.5, 0.01)
        };
        self.event_lattice.register_agent(id, threshold, decay_rate);

        tracing::info!(agent = %agent_id, "Agent restarted");
        Ok(())
    }

    fn setup_providers(
        config: &AxocoatlConfig,
        registry: &mut ProviderRegistry,
    ) -> Result<(), DaemonError> {
        // OpenAI
        if let Some(openai) = &config.providers.openai {
            if !openai.api_key.is_empty() {
                let provider = axocoatl_llm_openai::OpenAiProvider::new(
                    openai.api_key.expose_secret(),
                    "gpt-4o", // Default model — agents specify their own
                );
                registry.register(Arc::new(provider));
                tracing::info!("Registered OpenAI provider");

                // Set up fallback chain if configured
                if let Some(fallback) = &openai.fallback {
                    registry.set_fallback_chain("openai", vec![fallback.clone()]);
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

                if let Some(fallback) = &openrouter.fallback {
                    registry.set_fallback_chain("openrouter", vec![fallback.clone()]);
                }
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
        let id = AgentId::new(agent_id);
        let actor =
            self.agent_registry.get(&id).await.ok_or_else(|| {
                DaemonError::AgentSpawn(format!("Agent '{}' not found", agent_id))
            })?;

        let output = axocoatl_actor::execute_agent(&actor, axocoatl_core::AgentInput::text(input))
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
        if let Some(sandbox) = self.session_sandboxes.lock().await.remove(id) {
            sandbox.stop().await;
        }
        self.session_store
            .lock()
            .await
            .close(id)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    /// Delete a session entirely — stop and remove its sandbox, then drop the
    /// JSON from disk. Memory tiers under `{data_dir}/memory/{session_id}` are
    /// left in place; a user that creates a new session pointing at the same
    /// directory gets a fresh memory slate (different session id).
    pub async fn delete_session(&self, id: &str) -> Result<(), DaemonError> {
        if let Some(sandbox) = self.session_sandboxes.lock().await.remove(id) {
            sandbox.stop().await;
        }
        self.session_store
            .lock()
            .await
            .remove(id)
            .map_err(|e| DaemonError::Session(e.to_string()))
    }

    /// Rename a session (display name only).
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

    /// Ensure the session's OCI sandbox container is running, returning it.
    async fn ensure_sandbox(&self, session: &Session) -> Result<Arc<SessionSandbox>, DaemonError> {
        let mut boxes = self.session_sandboxes.lock().await;
        if let Some(sb) = boxes.get(&session.id) {
            return Ok(sb.clone());
        }
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
        let sandbox = Arc::new(sandbox);
        boxes.insert(session.id.clone(), sandbox.clone());
        Ok(sandbox)
    }

    /// Execute an instruction inside a session.
    pub async fn execute_session(
        &self,
        session_id: &str,
        input: &str,
    ) -> Result<axocoatl_core::AgentOutput, DaemonError> {
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

        let output = axocoatl_actor::execute_agent(&actor, axocoatl_core::AgentInput::text(input))
            .await
            .map_err(DaemonError::AgentSpawn)?;

        let _ = self.session_store.lock().await.touch(session_id);
        Ok(output)
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
        let bus = self.stream_bus.clone();
        let (sink_tx, mut sink_rx) =
            tokio::sync::mpsc::unbounded_channel::<axocoatl_actor::AgentStreamChunk>();
        let fwd = {
            let bus = bus.clone();
            let rid = run_id.to_string();
            let aid = agent_label.to_string();
            tokio::spawn(async move {
                use crate::stream::StreamFrame as F;
                use axocoatl_actor::AgentStreamChunk as C;
                while let Some(chunk) = sink_rx.recv().await {
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
        let out = axocoatl_actor::execute_agent_streaming(
            actor,
            axocoatl_core::AgentInput::text(input).with_model_override(model_override),
            sink_tx,
        )
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
            return Ok(actor);
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
        let mut executor = ToolExecutor::new();
        axocoatl_tools::register_session_tools(&mut executor, sandbox);

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

        self.spawn_session_agent(session, &agent_yaml, &scoped, Arc::new(executor))
            .await
    }

    /// Spawn a session-scoped agent actor named `{session}:{agent}`, bound to
    /// the per-session tool executor and given a working-directory preamble.
    async fn spawn_session_agent(
        &self,
        session: &Session,
        agent_yaml: &axocoatl_config::AgentConfigYaml,
        scoped_id: &str,
        tool_executor: Arc<ToolExecutor>,
    ) -> Result<ractor::ActorRef<axocoatl_actor::AgentMessage>, DaemonError> {
        let mut agent_config = agent_yaml.to_core();

        // Resolve the provider (per-agent Ollama, else the shared registry).
        let provider: Arc<dyn axocoatl_llm::LlmProvider> = if agent_config.provider == "ollama" {
            let ollama = self.config.providers.ollama.as_ref().ok_or_else(|| {
                DaemonError::Provider("Ollama provider not configured".to_string())
            })?;
            let model = if agent_yaml.model.is_empty() {
                ollama.model.as_deref().unwrap_or("llama3.2")
            } else {
                &agent_yaml.model
            };
            Arc::new(axocoatl_llm_ollama::OllamaProvider::with_base_url(
                &ollama.base_url,
                model,
            ))
        } else {
            self.provider_registry
                .get(&agent_config.provider)
                .cloned()
                .ok_or_else(|| {
                    DaemonError::Provider(format!(
                        "Provider '{}' not configured",
                        agent_config.provider
                    ))
                })?
        };

        // The scoped id drives the actor name and the checkpoint key, so a
        // session's conversation is isolated from the global agent's.
        agent_config.id = AgentId::new(scoped_id);

        let behavior = DefaultAgentBehavior::new(provider, self.counter.clone())
            .with_checkpoint_store(self.checkpoint_store.clone())
            .with_tool_executor(tool_executor)
            .with_long_term_memory(self.long_term_memory.clone())
            .with_session_context(session.working_dir.display())
            // Shared/versioned team knowledge layer — walks up from working_dir
            // collecting every AXOCOATL.md it finds.
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
        self.agent_handles.lock().unwrap().push(handle);
        tracing::info!(session = %session.id, agent = %scoped_id, "Session agent spawned");
        Ok(actor_ref)
    }

    /// Execute a multi-agent workflow.
    ///
    /// Directly activates the entry agent(s) with the user's input.
    /// As each agent completes, a TaskCompleted event is published to the lattice,
    /// which accumulates pheromone signals on downstream agents. When a downstream
    /// agent's threshold is crossed (based on its `depends_on` count), it activates
    /// and receives the upstream outputs as context.
    pub async fn execute_workflow(
        &self,
        workflow_id: &str,
        input: &str,
    ) -> Result<WorkflowOutput, DaemonError> {
        // 1. Look up workflow config
        let workflow = self
            .config
            .workflows
            .iter()
            .find(|w| w.id == workflow_id)
            .ok_or_else(|| DaemonError::WorkflowNotFound(workflow_id.to_string()))?;

        // 2. Build the set of expected agents and their dependency maps
        let mut expected_agents = HashSet::new();
        let depends_on_map: DashMap<AgentId, Vec<AgentId>> = DashMap::new();

        for agent_id_str in &workflow.agents {
            let agent_id = AgentId::new(agent_id_str);
            expected_agents.insert(agent_id.clone());

            // Look up this agent's depends_on from config
            if let Some(agent_yaml) = self.config.agents.iter().find(|a| a.id == *agent_id_str) {
                let deps: Vec<AgentId> = agent_yaml.depends_on.iter().map(AgentId::new).collect();
                depends_on_map.insert(agent_id, deps);
            }
        }

        if expected_agents.is_empty() {
            return Err(DaemonError::WorkflowExecution(
                "Workflow has no agents".to_string(),
            ));
        }

        // 3. Create the execution tracker
        let workflow_exec = Arc::new(WorkflowExecution::new(
            workflow_id.to_string(),
            expected_agents,
            input.to_string(),
            depends_on_map,
        ));

        // 4. Determine entry agents: use explicit entry_point from config,
        //    or fall back to all workflow agents with no dependencies.
        let entry_agents: Vec<AgentId> = if let Some(entry) = &workflow.entry_point {
            vec![AgentId::new(entry)]
        } else {
            workflow
                .agents
                .iter()
                .filter(|id| {
                    self.config
                        .agents
                        .iter()
                        .find(|a| &a.id == *id)
                        .is_some_and(|a| a.depends_on.is_empty())
                })
                .map(AgentId::new)
                .collect()
        };

        if entry_agents.is_empty() {
            return Err(DaemonError::WorkflowExecution(
                "Workflow has no entry agents (no entry_point and no agents with empty depends_on)"
                    .to_string(),
            ));
        }

        // 5. Publish a UserInput event (informational — for external observers, doesn't
        //    drive activation). Then directly activate the entry agent(s) so the
        //    stigmergic cascade begins with a clean state.
        let kickoff_event = LatticeEvent {
            id: EventId::random(),
            event_type: EventType::UserInput,
            payload: serde_json::json!({
                "input": input,
                "workflow_id": workflow_id,
            }),
            produced_by: "user".to_string(),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };

        tracing::info!(
            workflow = %workflow_id,
            entry_agents = ?entry_agents.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            "Workflow started — activating entry agents"
        );

        // 6. Directly send entry agents into the activation loop.
        //    Downstream activations will happen via lattice.publish() calls
        //    inside the activation loop after each agent completes.
        for agent_id in entry_agents {
            if workflow_exec.expected_agents.contains(&agent_id) {
                let _ = self.activation_tx.send(ActivationRequest {
                    agent_id,
                    triggering_event: kickoff_event.clone(),
                    workflow_exec: workflow_exec.clone(),
                });
            }
        }

        // 7. Wait for completion with timeout
        let timeout_secs = 300u64;
        match tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            workflow_exec.done.notified(),
        )
        .await
        {
            Ok(()) => {
                tracing::info!(workflow = %workflow_id, "Workflow completed");
                Ok(workflow_exec.into_output())
            }
            Err(_) => {
                tracing::error!(workflow = %workflow_id, "Workflow timed out");
                Err(DaemonError::WorkflowTimeout(timeout_secs))
            }
        }
    }

    /// Gracefully shut down all agents and the activation loop.
    pub async fn shutdown(self) {
        // Stop the activation loop
        if let Some(handle) = self.activation_handle {
            handle.abort();
            let _ = handle.await;
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

    /// Number of configured workflows.
    pub fn workflow_count(&self) -> usize {
        self.config.workflows.len()
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
}
