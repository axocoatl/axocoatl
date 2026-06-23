//! Coordinator behavior — an orchestrator agent that decomposes a goal into
//! subtasks, assigns each to a worker agent, runs them in parallel, and
//! synthesizes the results.
//!
//! - Decomposition prefers the symbolic HTN planner (resolving any LLM frontiers
//!   task-by-task) and falls back to whole-goal LLM decomposition only when no
//!   planner is configured.
//! - Workers are chosen by a capability/budget auction and spawned with the full
//!   agent stack (checkpointing, long-term + semantic memory, hooks, tools).
//! - The run is checkpointed — the plan plus each worker's outcome — so after a
//!   crash the next run for the same goal resumes, skipping work already done.
//! - If every worker fails the coordinator returns an error rather than
//!   synthesizing from nothing.

use std::collections::HashMap;
use std::sync::Arc;

use axocoatl_coordination::{compute_bid, run_auction, AgentBid, HtnPlanner, HtnTask, HtnTaskType};
use axocoatl_core::{AgentConfig, AgentId, AgentInput, AgentOutput, TokenUsageStats};
use axocoatl_llm::{ChatRequest, LlmProvider};
use axocoatl_memory::{AgentCheckpoint, CheckpointStore, DailyLogMemory, SemanticMemory};
use axocoatl_token::{TokenCounter, TokenTracker};
use axocoatl_tools::{HookRegistry, ToolExecutor};
use serde::{Deserialize, Serialize};

use crate::actor_impl::{execute_agent, AgentActor, AgentMessage};
use crate::behavior::AgentBehavior;
use crate::default_behavior::DefaultAgentBehavior;
use crate::error::AgentError;
use crate::frontier_resolver::LlmFrontierResolver;

/// Budget assumed for a worker that declares no explicit `token_budget` —
/// treated as ample so an unbounded worker isn't penalized in the auction.
pub const DEFAULT_WORKER_BUDGET: usize = 100_000;

/// Extract the JSON array from an LLM decomposition response.
///
/// Reasoning models (Qwen3, DeepSeek-R1, …) emit a `<think>…</think>` block
/// and/or prose around their answer, and many models wrap it in a ```json
/// fence. A bare `serde_json::from_str` on the whole response then fails even
/// though a valid array is present. This drops a leading think block and any
/// code fence, then falls back to the outermost `[ … ]` slice.
fn extract_json_array(content: &str) -> Option<String> {
    let mut s = content.trim();
    if let Some(end) = s.find("</think>") {
        s = s[end + "</think>".len()..].trim();
    }
    if let Some(start) = s.find("```") {
        let after = &s[start + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        s = match after.find("```") {
            Some(end) => after[..end].trim(),
            None => after.trim(),
        };
    }
    if serde_json::from_str::<serde_json::Value>(s).is_ok() {
        return Some(s.to_string());
    }
    let start = s.find('[')?;
    let end = s.rfind(']')?;
    (end > start).then(|| s[start..=end].to_string())
}

/// Status of a worker agent managed by the coordinator.
#[derive(Debug, Clone)]
pub struct WorkerStatus {
    pub agent_id: AgentId,
    pub task: Option<String>,
    pub state: WorkerState,
    pub token_usage: TokenUsageStats,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkerState {
    Idle,
    Running,
    Completed,
    Failed { error: String },
}

/// Configuration for a worker spawned by the coordinator.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub id: AgentId,
    pub name: String,
    pub system_prompt: String,
    pub tools: Vec<String>,
    /// The model this worker runs on (e.g. `llama3.2`). Without it a worker would
    /// inherit `AgentConfig::default()`'s `gpt-4o`, which 404s on a local-only
    /// (Ollama) provider — and local-first is the coordinator's whole point.
    pub model: String,
    /// The worker's token budget, used as the budget signal in the auction.
    pub token_budget: usize,
    /// Recall tuning for this worker (passive injection + recall-tool defaults).
    pub recall: axocoatl_core::RecallConfig,
}

/// A unit of work the coordinator assigns to a worker: a name, a description,
/// and the tool names it requires (used by the auction to match workers).
#[derive(Debug, Clone)]
pub struct Subtask {
    pub name: String,
    pub description: String,
    pub required_tools: Vec<String>,
}

/// Result of a worker's task execution.
#[derive(Debug, Clone)]
pub struct WorkerResult {
    pub worker_id: AgentId,
    pub task_name: String,
    pub output: Result<AgentOutput, String>,
}

/// Persisted orchestration state for resumable runs. Serialized into the
/// coordinator's checkpoint (`AgentCheckpoint.behavior_state`) so that, after a
/// crash/restart, the next run for the same goal skips work already done.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrchestrationState {
    goal: String,
    items: Vec<OrchestrationItem>,
    /// Set once synthesis has succeeded — a completed run is never resumed.
    completed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrchestrationItem {
    name: String,
    description: String,
    required_tools: Vec<String>,
    /// `None` until this subtask's worker has finished.
    outcome: Option<OrchestrationOutcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum OrchestrationOutcome {
    Succeeded { content: String },
    Failed { error: String },
}

/// A subtask as reported to an observer: what it is, which worker won the
/// capability+budget auction, and the runner-up bids. Plain data so the actor
/// crate stays decoupled from the daemon's wire types.
#[derive(Debug, Clone, Default)]
pub struct ReportedSubtask {
    pub name: String,
    pub description: String,
    pub winner: String,
    pub score: f32,
    pub adhoc: bool,
    pub bids: Vec<ReportedBid>,
}

/// One worker's bid on a subtask.
#[derive(Debug, Clone, Default)]
pub struct ReportedBid {
    pub worker: String,
    pub score: f32,
}

/// Observer of a coordinator's run, so a UI can render Layer-2 progress. The
/// daemon implements this and forwards to the dashboard stream. Every method
/// takes the run id (`workflow`) — one coordinator can run many workflows.
pub trait CoordinatorReporter: Send + Sync {
    /// The decomposition + auction outcome, emitted once before the workers run.
    fn plan(&self, workflow: &str, coordinator: &str, goal: &str, subtasks: &[ReportedSubtask]);
    /// A worker began its subtask.
    fn worker_started(&self, workflow: &str, worker: &str);
    /// A worker finished — its output and token count.
    fn worker_done(&self, workflow: &str, worker: &str, output: &str, tokens: u64);
}

/// Coordinator behavior — manages a pool of worker agents.
///
/// The coordinator:
/// 1. Receives a high-level task
/// 2. Decomposes it into subtasks (via HTN planner or LLM)
/// 3. Spawns worker agents for each subtask
/// 4. Collects results and synthesizes a final response
pub struct CoordinatorBehavior {
    provider: Arc<dyn LlmProvider>,
    counter: Arc<dyn TokenCounter>,
    tracker: Option<TokenTracker>,
    tool_executor: Option<Arc<ToolExecutor>>,
    system_prompt: Option<String>,
    agent_id: String,
    /// The coordinator's own model, inherited by ad-hoc workers (those spawned
    /// when no pooled worker bids) so they run on the same provider/model rather
    /// than the `gpt-4o` default, which fails on a local Ollama setup.
    model: String,

    /// Configurations for worker agents this coordinator can spawn.
    worker_configs: Vec<WorkerConfig>,
    /// Active workers and their actor refs.
    active_workers: HashMap<AgentId, ractor::ActorRef<AgentMessage>>,
    /// JoinHandles for worker actors.
    worker_handles: Vec<tokio::task::JoinHandle<()>>,
    /// Collected results from workers.
    worker_results: Vec<WorkerResult>,
    /// Optional HTN planner. When set, decompose_task tries symbolic
    /// decomposition (no LLM call) before falling back to the LLM.
    htn_planner: Option<HtnPlanner>,
    /// Monotonic run counter — scopes worker actor names per run so repeated
    /// executions of the same coordinator never collide in ractor's registry.
    run_seq: u64,
    /// Full-stack dependencies handed to every spawned worker so a worker is a
    /// first-class agent (checkpointed, with core + semantic memory and the
    /// global hook registry), not a bare provider+tools shell.
    checkpoint_store: Option<Arc<CheckpointStore>>,
    /// Shared core-memory blocks handed to each worker (opt-in team memory).
    shared_blocks: std::collections::HashMap<String, axocoatl_memory::SharedBlock>,
    hook_registry: Option<Arc<HookRegistry>>,
    /// Data directory for per-worker semantic memory. When set (by the daemon),
    /// each worker gets a Tier-4 semantic store under it; when unset, workers
    /// run without semantic memory (and create no on-disk store).
    data_dir: Option<String>,
    /// Version counter for the coordinator's own orchestration checkpoints.
    checkpoint_version: u64,
    /// Orchestration state restored from a checkpoint in `on_start`; consumed by
    /// the next run if its goal matches (resume), else discarded (fresh run).
    resumed_state: Option<OrchestrationState>,
    /// Optional observer of run progress (decompose, auction, workers). The
    /// daemon sets this to forward Layer-2 progress to the dashboard stream.
    reporter: Option<Arc<dyn CoordinatorReporter>>,
}

impl CoordinatorBehavior {
    pub fn new(provider: Arc<dyn LlmProvider>, counter: Arc<dyn TokenCounter>) -> Self {
        Self {
            provider,
            counter,
            tracker: None,
            tool_executor: None,
            system_prompt: None,
            agent_id: String::new(),
            model: String::new(),
            worker_configs: Vec::new(),
            active_workers: HashMap::new(),
            worker_handles: Vec::new(),
            worker_results: Vec::new(),
            htn_planner: None,
            run_seq: 0,
            checkpoint_store: None,
            shared_blocks: std::collections::HashMap::new(),
            hook_registry: None,
            data_dir: None,
            checkpoint_version: 0,
            resumed_state: None,
            reporter: None,
        }
    }

    pub fn with_tool_executor(mut self, executor: Arc<ToolExecutor>) -> Self {
        self.tool_executor = Some(executor);
        self
    }

    /// Attach a run-progress observer (the daemon forwards it to the dashboard).
    pub fn with_reporter(mut self, reporter: Arc<dyn CoordinatorReporter>) -> Self {
        self.reporter = Some(reporter);
        self
    }

    pub fn with_checkpoint_store(mut self, store: Arc<CheckpointStore>) -> Self {
        self.checkpoint_store = Some(store);
        self
    }

    pub fn with_shared_blocks(
        mut self,
        shared: std::collections::HashMap<String, axocoatl_memory::SharedBlock>,
    ) -> Self {
        self.shared_blocks = shared;
        self
    }

    pub fn with_hook_registry(mut self, registry: Arc<HookRegistry>) -> Self {
        self.hook_registry = Some(registry);
        self
    }

    /// Set the data directory used for per-worker semantic memory stores.
    pub fn with_data_dir(mut self, data_dir: String) -> Self {
        self.data_dir = Some(data_dir);
        self
    }

    /// Set the coordinator's own model. Ad-hoc workers inherit it so they run on
    /// the coordinator's provider/model instead of the `gpt-4o` default.
    pub fn with_model(mut self, model: String) -> Self {
        self.model = model;
        self
    }

    /// Add a worker configuration. Workers with these configs can be spawned on demand.
    pub fn add_worker_config(mut self, config: WorkerConfig) -> Self {
        self.worker_configs.push(config);
        self
    }

    /// Attach an HTN planner. When set, `decompose_task` tries symbolic
    /// decomposition (no LLM call) before falling back to the LLM.
    pub fn with_htn_methods(mut self, planner: HtnPlanner) -> Self {
        self.htn_planner = Some(planner);
        self
    }

    /// Spawn a worker agent and return its ID.
    async fn spawn_worker(&mut self, config: &WorkerConfig) -> Result<AgentId, AgentError> {
        let agent_config = AgentConfig {
            id: config.id.clone(),
            name: config.name.clone(),
            // The worker's own model — without this it would fall back to
            // `AgentConfig::default()`'s `gpt-4o`, which 404s on a local provider.
            model: config.model.clone(),
            system_prompt: Some(config.system_prompt.clone()),
            tools: config.tools.clone(),
            // Carry the worker's recall tuning so its `on_start` configures the
            // passive path and the recall-tool defaults from it.
            memory: axocoatl_core::MemoryConfig {
                recall: config.recall.clone(),
                ..Default::default()
            },
            ..AgentConfig::default()
        };

        // Build the worker with the full agent stack so it is a first-class
        // agent, not a bare provider+tools shell: checkpointing, long-term and
        // semantic memory, the global hook registry, and tool execution.
        let mut behavior = DefaultAgentBehavior::new(self.provider.clone(), self.counter.clone());
        if let Some(executor) = &self.tool_executor {
            behavior = behavior.with_tool_executor(executor.clone());
        }
        if let Some(store) = &self.checkpoint_store {
            behavior = behavior.with_checkpoint_store(store.clone());
        }
        if let Some(hooks) = &self.hook_registry {
            behavior = behavior.with_hook_registry(hooks.clone());
        }
        // Per-worker Tier-2/3/4 stores under the coordinator's data dir (same
        // scheme as a standalone agent). Built only when a data dir is configured
        // (the daemon sets it); omitted in lightweight/embedded use. Non-fatal.
        if let Some(data_dir) = &self.data_dir {
            // Daily log — a worker archives raw conversation segments here before
            // context compaction summarizes them, so no history is lost.
            behavior = behavior.with_daily_log(Arc::new(DailyLogMemory::new(
                config.id.to_string(),
                format!("{data_dir}/memory/daily_log"),
            )));
            match SemanticMemory::new(
                &config.id.to_string(),
                format!("{data_dir}/memory/semantic"),
            ) {
                Ok(sem) => behavior = behavior.with_semantic_memory(Arc::new(sem)),
                Err(e) => {
                    tracing::warn!(worker = %config.id, error = %e, "semantic memory unavailable")
                }
            }
            // Core memory (Tier 3) — the default block set per worker, plus the
            // coordinator's shared blocks (opt-in team memory).
            let specs: Vec<axocoatl_memory::MemoryBlock> = axocoatl_core::default_core_blocks()
                .iter()
                .map(axocoatl_memory::MemoryBlock::from)
                .collect();
            let store = axocoatl_memory::build_store(
                &config.id.to_string(),
                axocoatl_memory::core_store_path(data_dir, &config.id.to_string()),
                &specs,
            )
            .await;
            behavior = behavior.with_core_memory(
                Arc::new(tokio::sync::RwLock::new(store)),
                self.shared_blocks.clone(),
            );
        }

        // Run-scoped actor name so repeated runs of this coordinator never
        // collide in ractor's global registry; the logical id (config.id) still
        // keys active_workers and drives delegation.
        // (spawn, not spawn_linked — ractor 0.15 doesn't expose spawn_linked on
        // the Actor trait; worker crashes surface as errors from execute_agent.)
        let actor_name = format!("{}#{}", config.id, self.run_seq);
        let (actor_ref, handle) = ractor::Actor::spawn(
            Some(actor_name),
            AgentActor,
            (agent_config, Box::new(behavior) as Box<dyn AgentBehavior>),
        )
        .await
        .map_err(|e| AgentError::Internal(format!("Failed to spawn worker: {e}")))?;

        // Store handle so we can await termination
        self.worker_handles.push(handle);
        self.active_workers.insert(config.id.clone(), actor_ref);
        tracing::info!(
            coordinator = %self.agent_id,
            worker = %config.id,
            "Spawned worker agent"
        );

        Ok(config.id.clone())
    }

    /// Stop all active workers and await full teardown so their actor names are
    /// released from ractor's registry before the next run, then join the
    /// spawned actor tasks so nothing is left running.
    async fn stop_all_workers(&mut self) {
        for (id, actor) in self.active_workers.drain() {
            let _ = actor
                .stop_and_wait(None, Some(std::time::Duration::from_secs(10)))
                .await;
            tracing::debug!(worker = %id, "Stopped worker");
        }
        for handle in self.worker_handles.drain(..) {
            let _ = handle.await;
        }
    }

    /// Persist the current orchestration state to the coordinator's checkpoint
    /// so a crash/restart can resume the run. No-op when no checkpoint store is
    /// configured (lightweight/embedded use).
    async fn checkpoint_orchestration(&mut self, state: &OrchestrationState) {
        let Some(store) = self.checkpoint_store.clone() else {
            return;
        };
        self.checkpoint_version += 1;
        let checkpoint_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let behavior_state = match serde_json::to_string(state) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(coordinator = %self.agent_id, error = %e, "failed to serialize orchestration state");
                return;
            }
        };
        let ckpt = AgentCheckpoint {
            version: self.checkpoint_version,
            agent_id: self.agent_id.clone(),
            checkpoint_time,
            session_messages: Vec::new(),
            cumulative_token_usage: TokenUsageStats::default(),
            behavior_state,
        };
        if let Err(e) = store.save(&ckpt).await {
            tracing::warn!(coordinator = %self.agent_id, error = %e, "failed to checkpoint orchestration");
        }
    }

    /// Decompose a goal into subtasks. Prefers the symbolic HTN planner: it
    /// plans, resolves any LLM frontiers (decomposing only those tasks with the
    /// model, not the whole goal), and errors if the plan can't be made fully
    /// primitive. Only when no planner is configured does it decompose the whole
    /// goal with the LLM. Either way, an empty decomposition is an error.
    async fn decompose_task(&self, task: &str) -> Result<Vec<Subtask>, AgentError> {
        if let Some(planner) = &self.htn_planner {
            let root = HtnTask {
                name: task.to_string(),
                parameters: HashMap::new(),
                task_type: HtnTaskType::Compound,
            };
            // resolve_frontiers takes &mut self; clone so the shared planner is
            // left untouched across runs.
            let mut planner = planner.clone();
            let resolver = LlmFrontierResolver::new(self.provider.clone());
            let plan = planner
                .resolve_frontiers(root, &resolver, 4)
                .await
                .map_err(AgentError::Internal)?;
            if !plan.llm_frontiers.is_empty() {
                return Err(AgentError::Internal(format!(
                    "HTN planning left {} task(s) unresolved after frontier resolution",
                    plan.llm_frontiers.len()
                )));
            }
            let subtasks: Vec<Subtask> = plan
                .primitives
                .into_iter()
                .map(|t| Subtask {
                    description: t
                        .parameters
                        .get("description")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| t.name.clone()),
                    required_tools: t.required_tools(),
                    name: t.name,
                })
                .collect();
            if subtasks.is_empty() {
                return Err(AgentError::Internal(
                    "HTN planning produced no subtasks".to_string(),
                ));
            }
            tracing::info!(
                coordinator = %self.agent_id,
                subtasks = subtasks.len(),
                "Decomposed via HTN"
            );
            return Ok(subtasks);
        }

        // No planner configured — decompose the whole goal with the LLM.
        let decompose_prompt = format!(
            "You are a task decomposition engine. Break the following task into 2-5 \
             independent subtasks.\n\
             Return ONLY a JSON array of objects with 'name', 'description', and 'tools' \
             fields ('tools' is an array of tool names the subtask needs, [] if none).\n\
             Do not include any other text.\n\n\
             Task: {task}"
        );
        let request = ChatRequest::with_system(
            "You decompose tasks into subtasks. Return only valid JSON.",
            decompose_prompt,
        );
        let response = self
            .provider
            .chat(request)
            .await
            .map_err(|e| AgentError::Provider(e.to_string()))?;

        // Reasoning models wrap the array in <think> blocks, prose, or a ```json
        // fence; pull the JSON array out before parsing so decomposition is
        // robust to that, not just to a clean bare array.
        let json = extract_json_array(&response.content).ok_or_else(|| {
            AgentError::Internal(format!(
                "task decomposition returned no JSON array (first 200 chars: {})",
                response.content.chars().take(200).collect::<String>()
            ))
        })?;
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).map_err(|e| {
            AgentError::Internal(format!("task decomposition returned invalid JSON: {e}"))
        })?;
        let subtasks: Vec<Subtask> = parsed
            .into_iter()
            .map(|s| {
                let required_tools = s
                    .get("tools")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                Subtask {
                    name: s["name"].as_str().unwrap_or("task").to_string(),
                    description: s["description"].as_str().unwrap_or(task).to_string(),
                    required_tools,
                }
            })
            .collect();
        if subtasks.is_empty() {
            return Err(AgentError::Internal(
                "task decomposition produced no subtasks".to_string(),
            ));
        }
        Ok(subtasks)
    }
}

#[async_trait::async_trait]
impl AgentBehavior for CoordinatorBehavior {
    async fn on_start(&mut self, config: &AgentConfig) -> Result<(), AgentError> {
        self.system_prompt = config.system_prompt.clone();
        self.agent_id = config.id.to_string();

        // Restore an incomplete orchestration so the next run can resume it
        // (same model as a normal agent restoring its session on restart).
        if let Some(store) = &self.checkpoint_store {
            if let Ok(Some(ckpt)) = store.load_latest(&config.id).await {
                self.checkpoint_version = ckpt.version;
                if let Some(json) = ckpt.behavior_state {
                    match serde_json::from_str::<OrchestrationState>(&json) {
                        Ok(state) if !state.completed => {
                            let done = state.items.iter().filter(|i| i.outcome.is_some()).count();
                            tracing::info!(
                                coordinator = %self.agent_id,
                                done,
                                total = state.items.len(),
                                "Restored incomplete orchestration; will resume on next run"
                            );
                            self.resumed_state = Some(state);
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(
                            coordinator = %self.agent_id,
                            error = %e,
                            "ignoring unparseable orchestration checkpoint"
                        ),
                    }
                }
            }
        }

        if let Some(budget) = &config.token_budget {
            self.tracker = Some(TokenTracker::new(budget.clone(), self.counter.clone()));
        }

        tracing::info!(
            coordinator = %self.agent_id,
            workers = self.worker_configs.len(),
            "Coordinator started"
        );

        Ok(())
    }

    async fn execute(&mut self, input: AgentInput) -> Result<AgentOutput, AgentError> {
        // Run one coordination pass, then ALWAYS tear the workers down — on
        // success and on every error path — so no worker actor or task leaks.
        let result = self.run_once(input).await;
        self.stop_all_workers().await;
        result
    }

    async fn on_stop(&mut self) -> Result<(), AgentError> {
        self.stop_all_workers().await;
        tracing::info!(coordinator = %self.agent_id, "Coordinator stopped");
        Ok(())
    }
}

impl CoordinatorBehavior {
    /// One coordination pass: decompose, assign each subtask to a worker by
    /// auction, run the workers in parallel, and synthesize their results.
    /// Worker teardown is the caller's responsibility — [`execute`] always tears
    /// down afterward, on success and on every error path.
    async fn run_once(&mut self, input: AgentInput) -> Result<AgentOutput, AgentError> {
        // A fresh run: bump the run sequence (scopes worker actor names so
        // repeated runs never collide) and clear the previous run's results.
        self.run_seq += 1;
        self.worker_results.clear();

        // The run id (from the activation context) scopes the progress events so
        // the dashboard associates them with this workflow run; the original goal
        // goes to synthesis.
        let workflow_id = input
            .context
            .as_ref()
            .and_then(|c| c.get("workflow_id"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| self.agent_id.clone());
        let goal = input.content;

        // 1. Build the work list: resume an incomplete checkpointed run for the
        //    same goal (skipping work already done), else decompose fresh.
        let mut items: Vec<OrchestrationItem> = match self.resumed_state.take() {
            Some(state) if !state.completed && state.goal == goal => {
                let done = state.items.iter().filter(|i| i.outcome.is_some()).count();
                tracing::info!(
                    coordinator = %self.agent_id,
                    done,
                    total = state.items.len(),
                    "Resuming orchestration from checkpoint"
                );
                state.items
            }
            _ => {
                let subtasks = self.decompose_task(&goal).await?;
                tracing::info!(
                    coordinator = %self.agent_id,
                    subtasks = subtasks.len(),
                    "Decomposed task"
                );
                subtasks
                    .into_iter()
                    .map(|s| OrchestrationItem {
                        name: s.name,
                        description: s.description,
                        required_tools: s.required_tools,
                        outcome: None,
                    })
                    .collect()
            }
        };

        // Persist the plan so a crash after decomposition doesn't re-decompose.
        let mut state = OrchestrationState {
            goal: goal.clone(),
            items: items.clone(),
            completed: false,
        };
        self.checkpoint_orchestration(&state).await;

        // 2. Assign each PENDING subtask to a worker by auction (best fit by tool
        //    match and budget); already-completed items are skipped entirely.
        let pending: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.outcome.is_none())
            .map(|(i, _)| i)
            .collect();
        let mut available = self.worker_configs.clone();
        let coord_id = self.agent_id.clone();
        let coord_model = self.model.clone();
        let mut assignments: Vec<(usize, AgentId)> = Vec::new();
        // The auction outcome per subtask, reported to observers (the dashboard
        // run view) once the whole plan is assigned.
        let mut plan: Vec<ReportedSubtask> = Vec::new();

        for &idx in &pending {
            let item = &items[idx];
            let required_tools = &item.required_tools;
            // An ad-hoc worker granted exactly the subtask's required tools —
            // used when the pool is empty or no pooled worker can cover the
            // tools, so a subtask is never forced onto an unfit worker.
            let make_adhoc = || WorkerConfig {
                id: AgentId::new(format!("{coord_id}-worker-{idx}")),
                name: format!("Worker {idx}"),
                system_prompt: format!("You are a worker agent. Your task: {}", item.description),
                tools: required_tools.clone(),
                // Inherit the coordinator's model so an ad-hoc worker runs on the
                // same (local) provider rather than the `gpt-4o` default.
                model: coord_model.clone(),
                token_budget: DEFAULT_WORKER_BUDGET,
                recall: axocoatl_core::RecallConfig::default(),
            };
            let mut reported_bids: Vec<ReportedBid> = Vec::new();
            let mut adhoc = false;
            let worker_config = if available.is_empty() {
                adhoc = true;
                make_adhoc()
            } else {
                let bids: Vec<AgentBid> = available
                    .iter()
                    .map(|wc| {
                        let ac = AgentConfig {
                            id: wc.id.clone(),
                            tools: wc.tools.clone(),
                            ..AgentConfig::default()
                        };
                        compute_bid(&ac, required_tools, 0, wc.token_budget)
                    })
                    .collect();
                reported_bids = bids
                    .iter()
                    .map(|b| ReportedBid {
                        worker: b.agent_id.to_string(),
                        score: b.score,
                    })
                    .collect();
                match run_auction(bids).and_then(|id| available.iter().position(|w| w.id == id)) {
                    Some(pos) => available.remove(pos),
                    None => {
                        tracing::warn!(
                            coordinator = %coord_id,
                            tools = ?required_tools,
                            "No worker bid for subtask; spawning an ad-hoc worker with the required tools"
                        );
                        adhoc = true;
                        make_adhoc()
                    }
                }
            };
            let worker_id = self.spawn_worker(&worker_config).await?;
            let wid_str = worker_id.to_string();
            let score = reported_bids
                .iter()
                .find(|b| b.worker == wid_str)
                .map(|b| b.score)
                .unwrap_or(0.0);
            plan.push(ReportedSubtask {
                name: item.name.clone(),
                description: item.description.clone(),
                winner: wid_str,
                score,
                adhoc,
                bids: reported_bids,
            });
            assignments.push((idx, worker_id));
        }

        // Report the decomposition + auction outcome before the workers run, so
        // the dashboard can render the Layer-2 plan (goal → subtasks → winners).
        if let Some(reporter) = &self.reporter {
            reporter.plan(&workflow_id, &coord_id, &goal, &plan);
        }

        // 3. Delegate the pending subtasks to workers IN PARALLEL.
        let mut join_set = tokio::task::JoinSet::new();
        for (idx, worker_id) in assignments {
            if let Some(reporter) = &self.reporter {
                reporter.worker_started(&workflow_id, &worker_id.to_string());
            }
            let actor = self.active_workers.get(&worker_id).cloned();
            let desc = items[idx].description.clone();
            let name = items[idx].name.clone();
            let wid = worker_id.clone();
            join_set.spawn(async move {
                let result = if let Some(actor_ref) = actor {
                    execute_agent(&actor_ref, AgentInput::text(desc))
                        .await
                        .map_err(|e| format!("Worker {wid} failed: {e}"))
                } else {
                    Err(format!("Worker {wid} not found"))
                };
                (idx, name, wid, result)
            });
        }

        // Record each outcome as it completes and checkpoint after every one, so
        // a crash never loses finished work.
        let mut total_usage = TokenUsageStats::default();
        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok((idx, name, worker_id, Ok(output))) => {
                    total_usage.merge(&output.token_usage);
                    if let Some(reporter) = &self.reporter {
                        reporter.worker_done(
                            &workflow_id,
                            &worker_id.to_string(),
                            &output.content,
                            output.token_usage.total() as u64,
                        );
                    }
                    items[idx].outcome = Some(OrchestrationOutcome::Succeeded {
                        content: output.content.clone(),
                    });
                    self.worker_results.push(WorkerResult {
                        worker_id,
                        task_name: name,
                        output: Ok(output),
                    });
                }
                Ok((idx, name, worker_id, Err(e))) => {
                    tracing::warn!(worker = %worker_id, task = %name, error = %e, "Worker task failed");
                    if let Some(reporter) = &self.reporter {
                        reporter.worker_done(&workflow_id, &worker_id.to_string(), &e, 0);
                    }
                    items[idx].outcome = Some(OrchestrationOutcome::Failed { error: e.clone() });
                    self.worker_results.push(WorkerResult {
                        worker_id,
                        task_name: name,
                        output: Err(e),
                    });
                }
                Err(e) => {
                    // A panicked task carries no item index; leave the item
                    // pending so a resume re-runs it.
                    tracing::error!(error = %e, "Worker task panicked");
                    continue;
                }
            }
            state.items = items.clone();
            self.checkpoint_orchestration(&state).await;
        }

        // 4. Aggregate outcomes across ALL items (including any restored from a
        //    previous run). An item still pending here means its worker panicked.
        let succeeded: Vec<(String, String)> = items
            .iter()
            .filter_map(|it| match &it.outcome {
                Some(OrchestrationOutcome::Succeeded { content }) => {
                    Some((it.name.clone(), content.clone()))
                }
                _ => None,
            })
            .collect();
        let failed: Vec<(String, String)> = items
            .iter()
            .filter_map(|it| match &it.outcome {
                Some(OrchestrationOutcome::Failed { error }) => {
                    Some((it.name.clone(), error.clone()))
                }
                None => Some((it.name.clone(), "worker did not complete".to_string())),
                _ => None,
            })
            .collect();

        // If nothing succeeded there is nothing to synthesize — surface failure.
        if succeeded.is_empty() {
            return Err(AgentError::Internal(format!(
                "all {} worker task(s) failed; nothing to synthesize",
                failed.len()
            )));
        }

        // 5. Synthesize: give the model the original goal and a structured view
        //    of what succeeded and what failed so it answers the goal and
        //    accounts for any gaps.
        let mut synthesis_prompt = format!("Original goal:\n{goal}\n\nWorker results:\n");
        for (name, content) in &succeeded {
            synthesis_prompt.push_str(&format!("\n## {name} (succeeded)\n{content}\n"));
        }
        if !failed.is_empty() {
            synthesis_prompt.push_str("\nThese subtasks failed — account for the gaps:\n");
            for (name, err) in &failed {
                synthesis_prompt.push_str(&format!("- {name}: {err}\n"));
            }
        }
        synthesis_prompt
            .push_str("\nSynthesize these into a single coherent response to the original goal.");

        let request = ChatRequest::with_system(
            self.system_prompt
                .as_deref()
                .unwrap_or("You are a helpful coordinator."),
            synthesis_prompt,
        );
        let response = self
            .provider
            .chat(request)
            .await
            .map_err(|e| AgentError::Provider(e.to_string()))?;
        total_usage.merge(&response.usage);

        // Mark the run complete so a later request for the same goal starts
        // fresh rather than resuming this finished run.
        state.items = items;
        state.completed = true;
        self.checkpoint_orchestration(&state).await;

        Ok(AgentOutput {
            content: response.content,
            tool_calls: Vec::new(),
            token_usage: total_usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axocoatl_core::{AgentRole, ChatMessage};
    use axocoatl_llm::{
        ChatResponse, FinishReason, ProviderCapabilities, ProviderError, StreamEvent,
    };
    use axocoatl_token::TokenCounter;
    use std::pin::Pin;
    use tokio_stream::Stream;

    #[test]
    fn extract_json_array_reads_reasoning_wrapped_output() {
        // Qwen3/DeepSeek-style: a think block, then a fenced array.
        let raw = "<think>Break the task into independent subtasks.</think>\n\n\
                   ```json\n[{\"name\":\"a\",\"description\":\"x\",\"tools\":[]}]\n```";
        let json = extract_json_array(raw).expect("array extracted");
        let v: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn extract_json_array_reads_prose_wrapped_output() {
        let raw =
            "Sure! Here is the decomposition:\n[{\"name\":\"a\"}]\nLet me know if that helps.";
        let json = extract_json_array(raw).expect("array extracted");
        assert!(json.starts_with('[') && json.ends_with(']'));
    }

    #[test]
    fn extract_json_array_passes_through_bare_array() {
        assert_eq!(
            extract_json_array(" [1, 2, 3] ").as_deref(),
            Some("[1, 2, 3]")
        );
    }

    #[test]
    fn extract_json_array_rejects_no_array() {
        assert!(extract_json_array("I cannot do that.").is_none());
    }

    /// Every chat returns a fixed two-subtask decomposition. The coordinator's
    /// decompose call parses it into two subtasks; worker + synthesis calls just
    /// echo it back — enough to exercise the full decompose→delegate→synthesize
    /// path without a real model.
    struct MockLlm;

    #[async_trait]
    impl LlmProvider for MockLlm {
        fn provider_id(&self) -> &str {
            "mock"
        }
        fn model_id(&self) -> &str {
            "mock-model"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
            Ok(ChatResponse {
                content: r#"[{"name":"sub_a","description":"do A"},{"name":"sub_b","description":"do B"}]"#
                    .to_string(),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: TokenUsageStats::new(5, 5),
                model: "mock-model".to_string(),
                provider: "mock".to_string(),
            })
        }
        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let events = vec![
                Ok(StreamEvent::TextDelta {
                    delta: "ok".to_string(),
                }),
                Ok(StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                }),
            ];
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    /// Provider whose every call fails — used to force worker failures.
    struct FailingLlm;

    #[async_trait]
    impl LlmProvider for FailingLlm {
        fn provider_id(&self) -> &str {
            "failing"
        }
        fn model_id(&self) -> &str {
            "fail"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            Err(ProviderError::ApiError {
                provider: "failing".to_string(),
                status: 500,
                message: "mock LLM failure".to_string(),
            })
        }
        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            Err(ProviderError::ApiError {
                provider: "failing".to_string(),
                status: 500,
                message: "mock LLM failure".to_string(),
            })
        }
    }

    struct SimpleCounter;
    impl TokenCounter for SimpleCounter {
        fn count_text(&self, text: &str) -> usize {
            text.len() / 4 + 1
        }
        fn count_messages(&self, msgs: &[ChatMessage]) -> usize {
            msgs.iter()
                .map(|m| m.text_content().map_or(1, |t| self.count_text(t)))
                .sum()
        }
        fn count_tool_definition(&self, j: &serde_json::Value) -> usize {
            self.count_text(&j.to_string())
        }
    }

    fn coord_config() -> AgentConfig {
        AgentConfig {
            id: AgentId::new("lead"),
            name: "Lead".to_string(),
            role: AgentRole::Coordinator,
            ..AgentConfig::default()
        }
    }

    #[tokio::test]
    async fn coordinator_decomposes_delegates_synthesizes() {
        let provider: Arc<dyn LlmProvider> = Arc::new(MockLlm);
        let counter: Arc<dyn TokenCounter> = Arc::new(SimpleCounter);
        let mut coord = CoordinatorBehavior::new(provider, counter)
            .add_worker_config(WorkerConfig {
                id: AgentId::new("w1"),
                name: "W1".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                token_budget: DEFAULT_WORKER_BUDGET,
                recall: axocoatl_core::RecallConfig::default(),
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("w2"),
                name: "W2".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                token_budget: DEFAULT_WORKER_BUDGET,
                recall: axocoatl_core::RecallConfig::default(),
            });

        coord.on_start(&coord_config()).await.unwrap();
        let out = coord
            .execute(AgentInput::text("build something"))
            .await
            .unwrap();

        // The coordinator decomposed the goal, delegated to both workers in
        // parallel, and synthesized a non-empty result.
        assert!(!out.content.is_empty());
        assert_eq!(coord.worker_results.len(), 2);
    }

    #[tokio::test]
    async fn coordinator_uses_htn_when_methods_loaded() {
        // The HTN method decomposes the goal into THREE subtasks; the LLM mock
        // would return only two. Three workers proves HTN was used for
        // decomposition (no LLM decompose call).
        let methods = r#"
- task_pattern: "build something"
  preconditions: []
  subtasks:
    - name: "htn_a"
      parameters: {}
      task_type: Primitive
    - name: "htn_b"
      parameters: {}
      task_type: Primitive
    - name: "htn_c"
      parameters: {}
      task_type: Primitive
"#;
        let planner = HtnPlanner::from_methods_yaml(methods).unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(MockLlm);
        let counter: Arc<dyn TokenCounter> = Arc::new(SimpleCounter);
        let mut coord = CoordinatorBehavior::new(provider, counter)
            .with_htn_methods(planner)
            .add_worker_config(WorkerConfig {
                id: AgentId::new("h1"),
                name: "H1".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                token_budget: DEFAULT_WORKER_BUDGET,
                recall: axocoatl_core::RecallConfig::default(),
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("h2"),
                name: "H2".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                token_budget: DEFAULT_WORKER_BUDGET,
                recall: axocoatl_core::RecallConfig::default(),
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("h3"),
                name: "H3".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                token_budget: DEFAULT_WORKER_BUDGET,
                recall: axocoatl_core::RecallConfig::default(),
            });

        coord.on_start(&coord_config()).await.unwrap();
        let out = coord
            .execute(AgentInput::text("build something"))
            .await
            .unwrap();

        assert!(!out.content.is_empty());
        assert_eq!(coord.worker_results.len(), 3);
    }

    #[tokio::test]
    async fn coordinator_with_no_workers_uses_adhoc() {
        // No worker pool: the auction has nothing to bid on, so each subtask
        // gets an ad-hoc worker. Proves the empty-pool fallback / backward compat.
        let provider: Arc<dyn LlmProvider> = Arc::new(MockLlm);
        let counter: Arc<dyn TokenCounter> = Arc::new(SimpleCounter);
        let mut coord = CoordinatorBehavior::new(provider, counter);

        coord.on_start(&coord_config()).await.unwrap();
        let out = coord.execute(AgentInput::text("do work")).await.unwrap();

        assert!(!out.content.is_empty());
        // MockLlm decomposed into two subtasks → two ad-hoc workers.
        assert_eq!(coord.worker_results.len(), 2);
    }

    #[tokio::test]
    async fn coordinator_resolves_htn_frontier_via_llm() {
        // The method for "root" yields one primitive (p1) and one compound task
        // (needs_llm) with no method — a frontier. resolve_frontiers asks the LLM
        // (MockLlm → two subtasks) to decompose just that task, so the final plan
        // is fully primitive: p1 + the two resolved subtasks = 3.
        let methods = r#"
- task_pattern: "root"
  preconditions: []
  subtasks:
    - name: "p1"
      parameters: {}
      task_type: Primitive
    - name: "needs_llm"
      parameters: {}
      task_type: Compound
"#;
        let planner = HtnPlanner::from_methods_yaml(methods).unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(MockLlm);
        let counter: Arc<dyn TokenCounter> = Arc::new(SimpleCounter);
        let mut coord = CoordinatorBehavior::new(provider, counter).with_htn_methods(planner);
        for id in ["r1", "r2", "r3"] {
            coord = coord.add_worker_config(WorkerConfig {
                id: AgentId::new(id),
                name: id.to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                token_budget: DEFAULT_WORKER_BUDGET,
                recall: axocoatl_core::RecallConfig::default(),
            });
        }

        coord.on_start(&coord_config()).await.unwrap();
        let out = coord.execute(AgentInput::text("root")).await.unwrap();

        assert!(!out.content.is_empty());
        // p1 + the two LLM-resolved frontier subtasks.
        assert_eq!(coord.worker_results.len(), 3);
    }

    #[tokio::test]
    async fn auction_routes_subtask_to_tool_matching_worker() {
        // The single subtask requires the "special" tool; only the specialist
        // worker has it, so the auction must route the subtask there.
        let methods = r#"
- task_pattern: "route"
  preconditions: []
  subtasks:
    - name: "needs_special"
      parameters:
        tools: ["special"]
      task_type: Primitive
"#;
        let planner = HtnPlanner::from_methods_yaml(methods).unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(MockLlm);
        let counter: Arc<dyn TokenCounter> = Arc::new(SimpleCounter);
        let mut coord = CoordinatorBehavior::new(provider, counter)
            .with_htn_methods(planner)
            .add_worker_config(WorkerConfig {
                id: AgentId::new("generalist"),
                name: "Generalist".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                token_budget: DEFAULT_WORKER_BUDGET,
                recall: axocoatl_core::RecallConfig::default(),
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("specialist"),
                name: "Specialist".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec!["special".to_string()],
                model: "test-model".to_string(),
                token_budget: DEFAULT_WORKER_BUDGET,
                recall: axocoatl_core::RecallConfig::default(),
            });

        coord.on_start(&coord_config()).await.unwrap();
        coord.execute(AgentInput::text("route")).await.unwrap();

        assert_eq!(coord.worker_results.len(), 1);
        assert_eq!(
            coord.worker_results[0].worker_id,
            AgentId::new("specialist")
        );
    }

    #[tokio::test]
    async fn coordinator_runs_repeatedly_without_collision() {
        let provider: Arc<dyn LlmProvider> = Arc::new(MockLlm);
        let counter: Arc<dyn TokenCounter> = Arc::new(SimpleCounter);
        let mut coord = CoordinatorBehavior::new(provider, counter)
            .add_worker_config(WorkerConfig {
                id: AgentId::new("rep_a"),
                name: "A".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                token_budget: DEFAULT_WORKER_BUDGET,
                recall: axocoatl_core::RecallConfig::default(),
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("rep_b"),
                name: "B".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                token_budget: DEFAULT_WORKER_BUDGET,
                recall: axocoatl_core::RecallConfig::default(),
            });
        coord.on_start(&coord_config()).await.unwrap();

        // Two runs on the SAME coordinator instance must both succeed — the
        // run-scoped actor names and stop_and_wait teardown prevent a registry
        // collision on the second run.
        let first = coord.execute(AgentInput::text("first")).await;
        assert!(first.is_ok(), "first run failed: {first:?}");
        let second = coord.execute(AgentInput::text("second")).await;
        assert!(second.is_ok(), "second run failed: {second:?}");
        // worker_results reflects only the latest run (cleared each run).
        assert_eq!(coord.worker_results.len(), 2);
    }

    #[tokio::test]
    async fn coordinator_errors_when_all_workers_fail() {
        // HTN decomposes with no LLM call, but the workers run on a failing
        // provider, so every subtask fails — the coordinator surfaces an error
        // instead of synthesizing from nothing.
        let methods = r#"
- task_pattern: "build something"
  preconditions: []
  subtasks:
    - name: "a"
      parameters: {}
      task_type: Primitive
    - name: "b"
      parameters: {}
      task_type: Primitive
"#;
        let planner = HtnPlanner::from_methods_yaml(methods).unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(FailingLlm);
        let counter: Arc<dyn TokenCounter> = Arc::new(SimpleCounter);
        let mut coord = CoordinatorBehavior::new(provider, counter)
            .with_htn_methods(planner)
            .add_worker_config(WorkerConfig {
                id: AgentId::new("f1"),
                name: "F1".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                token_budget: DEFAULT_WORKER_BUDGET,
                recall: axocoatl_core::RecallConfig::default(),
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("f2"),
                name: "F2".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                token_budget: DEFAULT_WORKER_BUDGET,
                recall: axocoatl_core::RecallConfig::default(),
            });

        coord.on_start(&coord_config()).await.unwrap();
        let result = coord.execute(AgentInput::text("build something")).await;
        assert!(result.is_err(), "expected an error when all workers fail");
    }

    #[tokio::test]
    async fn coordinator_resumes_incomplete_orchestration() {
        use axocoatl_memory::CheckpointPolicy;

        let tmp = std::env::temp_dir().join(format!("axo-coord-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = Arc::new(CheckpointStore::new(&tmp, CheckpointPolicy::Manual));

        // Pre-seed a checkpoint for "lead": one item already done, one pending.
        let state = OrchestrationState {
            goal: "resume goal".to_string(),
            items: vec![
                OrchestrationItem {
                    name: "done_item".to_string(),
                    description: "already done".to_string(),
                    required_tools: vec![],
                    outcome: Some(OrchestrationOutcome::Succeeded {
                        content: "prior result".to_string(),
                    }),
                },
                OrchestrationItem {
                    name: "pending_item".to_string(),
                    description: "still to do".to_string(),
                    required_tools: vec![],
                    outcome: None,
                },
            ],
            completed: false,
        };
        let ckpt = AgentCheckpoint {
            version: 1,
            agent_id: "lead".to_string(),
            checkpoint_time: 0,
            session_messages: Vec::new(),
            cumulative_token_usage: TokenUsageStats::default(),
            behavior_state: Some(serde_json::to_string(&state).unwrap()),
        };
        store.save(&ckpt).await.unwrap();

        let provider: Arc<dyn LlmProvider> = Arc::new(MockLlm);
        let counter: Arc<dyn TokenCounter> = Arc::new(SimpleCounter);
        let mut coord = CoordinatorBehavior::new(provider, counter)
            .with_checkpoint_store(store.clone())
            .add_worker_config(WorkerConfig {
                id: AgentId::new("rw1"),
                name: "RW1".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                token_budget: DEFAULT_WORKER_BUDGET,
                recall: axocoatl_core::RecallConfig::default(),
            });

        // on_start restores the incomplete run; execute resumes it.
        coord.on_start(&coord_config()).await.unwrap();
        coord
            .execute(AgentInput::text("resume goal"))
            .await
            .unwrap();

        // Only the pending item ran this turn — the already-done item was skipped.
        assert_eq!(coord.worker_results.len(), 1);
        assert_eq!(coord.worker_results[0].task_name, "pending_item");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
