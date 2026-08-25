//! Coordinator behavior — an orchestrator agent that decomposes a goal into
//! subtasks, assigns each to a worker agent, runs them in parallel, and
//! synthesizes the results.
//!
//! - Decomposition prefers the symbolic HTN planner (resolving any LLM frontiers
//!   task-by-task) and falls back to whole-goal LLM decomposition only when no
//!   planner is configured.
//! - Workers are chosen by a capability/budget auction and declared workers are
//!   spawned with their configured checkpoint, daily/core/semantic memory,
//!   hooks, and exact tool capabilities.
//! - Nonterminal actor-internal checkpoints retain the plan and completed worker
//!   outcomes for crash recovery. Normal terminal cancellation/failure clears
//!   resumable behavior state so the next turn decomposes fresh.
//! - If every worker fails the coordinator returns an error rather than
//!   synthesizing from nothing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axocoatl_coordination::{compute_bid, run_auction, AgentBid, HtnPlanner, HtnTask, HtnTaskType};
use axocoatl_core::{
    secure_fs::SecureDir, AgentAttachment, AgentConfig, AgentId, AgentInput, AgentOutput,
    ChatMessage, ConversationMode, MemoryConfig, MessageRole, OverflowPolicy, SamplingConfig,
    TokenBudget, TokenUsageStats, ToolCallRecord,
};
use axocoatl_llm::{ChatRequest, LlmProvider};
use axocoatl_memory::{
    AgentCheckpoint, CheckpointStore, DailyLogMemory, SemanticMemory, SessionMemory,
};
use axocoatl_token::{BudgetError, TokenCounter, TokenTracker};
use axocoatl_tools::{HookRegistry, ToolExecutor};
use serde::{Deserialize, Serialize};

use crate::actor_impl::{
    execute_agent_controlled_measured, execute_agent_measured,
    execute_agent_streaming_controlled_measured, execute_agent_streaming_measured, AgentActor,
    AgentExecutionFailure, AgentMessage, MeasuredAgentRunOutcome,
};
use crate::behavior::{AgentBehavior, AgentStreamChunk, ExecutionUsageState, StreamSink};
use crate::default_behavior::{
    attach_to_last_user_message, load_project_instructions, DefaultAgentBehavior,
};
use crate::error::AgentError;
use crate::frontier_resolver::LlmFrontierResolver;
use crate::provider_budget::{self, ControlledChat};
use crate::run_control::{AgentRunControl, AgentRunOutcome};

/// Auction scalar for a worker with no enforced token budget. Execution is also
/// unbounded in that case, so the bid must not invent a finite enforcement cap.
pub const DEFAULT_WORKER_BUDGET: usize = usize::MAX;

fn executor_tools_for_worker(
    configured: &[String],
    inherited: &HashSet<String>,
) -> HashSet<String> {
    if configured.is_empty() {
        inherited.clone()
    } else {
        configured
            .iter()
            .filter(|name| inherited.contains(*name))
            .cloned()
            .collect()
    }
}

fn callable_tools_for_declared_worker(
    configured: &[String],
    inherited: &HashSet<String>,
    has_persistent_memory: bool,
) -> Vec<String> {
    let mut tools = executor_tools_for_worker(configured, inherited);
    if has_persistent_memory {
        tools.extend([
            crate::recall::RECALL_SEARCH.to_string(),
            crate::recall::RECALL_TIMEFRAME.to_string(),
            crate::core_memory_tools::CORE_MEMORY_APPEND.to_string(),
            crate::core_memory_tools::CORE_MEMORY_REPLACE.to_string(),
            crate::core_memory_tools::CORE_MEMORY_SET.to_string(),
        ]);
    }
    let mut tools: Vec<_> = tools.into_iter().collect();
    tools.sort();
    tools
}

fn missing_required_tools(required: &[String], callable: &[String]) -> Vec<String> {
    let callable: HashSet<_> = callable.iter().map(String::as_str).collect();
    let mut missing: Vec<_> = required
        .iter()
        .filter(|name| !callable.contains(name.as_str()))
        .cloned()
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

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
#[derive(Clone)]
pub struct WorkerConfig {
    pub id: AgentId,
    pub name: String,
    pub system_prompt: String,
    pub tools: Vec<String>,
    /// The model this worker runs on (e.g. `llama3.2`). Without it a worker would
    /// inherit `AgentConfig::default()`'s `gpt-4o`, which 404s on a local-only
    /// (Ollama) provider — and local-first is the coordinator's whole point.
    pub model: String,
    /// Exact provider capability for a declared heterogeneous worker. `None`
    /// means an ad-hoc worker inherits the coordinator provider.
    pub provider: Option<Arc<dyn LlmProvider>>,
    /// The worker's enforced token budget. Its execution cap is also the budget
    /// signal used in the assignment auction.
    pub token_budget: Option<TokenBudget>,
    pub sampling: SamplingConfig,
    pub memory: MemoryConfig,
    /// In-sandbox working directory shown to the worker.
    pub session_context: Option<String>,
    /// Host workspace root used to load versioned project instructions.
    pub project_instructions_root: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for WorkerConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("system_prompt", &self.system_prompt)
            .field("tools", &self.tools)
            .field("model", &self.model)
            .field(
                "provider",
                &self
                    .provider
                    .as_ref()
                    .map(|provider| provider.provider_id()),
            )
            .field("token_budget", &self.token_budget)
            .field("sampling", &self.sampling)
            .field("memory", &self.memory)
            .field("session_context", &self.session_context)
            .field("project_instructions_root", &self.project_instructions_root)
            .finish()
    }
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
    /// Actor-lifetime usage that preceded this run. Keeping the base separate
    /// from `token_usage` prevents a resumed partial run from being counted a
    /// second time when it eventually terminalizes.
    #[serde(default)]
    lifetime_usage_before_run: TokenUsageStats,
    #[serde(default)]
    lifetime_usage_before_run_known: bool,
    /// Saturating total usage already incurred by coordinator calls and workers
    /// in this run. Restored runs resume both evidence and budget accounting.
    #[serde(default)]
    token_usage: TokenUsageStats,
    /// Whether `token_usage` covers every dispatched provider/worker call.
    /// Older checkpoints predate completeness tracking and are treated as
    /// known because their legacy execution paths stored only completed calls.
    #[serde(default = "default_true")]
    token_usage_known: bool,
    /// Coordinator-side provider usage only (decomposition/frontiers/synthesis).
    /// This is the amount recharged into the shared coordinator tracker on
    /// resume; worker usage remains governed by each worker's own budget.
    #[serde(default)]
    coordinator_provider_usage: TokenUsageStats,
    /// Set once synthesis has succeeded — a completed run is never resumed.
    completed: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrchestrationItem {
    name: String,
    description: String,
    required_tools: Vec<String>,
    /// `None` until this subtask's worker has finished.
    outcome: Option<OrchestrationOutcome>,
}

enum CoordinatorRunOutcome {
    Completed(AgentOutput),
    Cancelled(AgentOutput),
}

struct CoordinatorRequestContext {
    history: Vec<ChatMessage>,
    system: Option<String>,
    model: Option<String>,
    attachments: Vec<AgentAttachment>,
    conversation_mode: ConversationMode,
}

/// Coordinator-side provider history is intentionally text-only. Worker-native
/// tool transactions can carry route-specific replay metadata and must remain
/// durable evidence in the Session ledger without ever being replayed through a
/// potentially different coordinator provider. Valid complete tool groups are
/// omitted atomically; malformed groups fail closed before any provider/worker
/// dispatch or actor-session mutation.
fn sanitize_coordinator_history(history: &[ChatMessage]) -> Result<Vec<ChatMessage>, AgentError> {
    let mut safe = Vec::new();
    let mut index = 0_usize;
    while index < history.len() {
        let message = &history[index];
        match message.role {
            MessageRole::System => {
                // The controlled coordinator system prompt/override is the only
                // system authority. Imported historical System messages are not
                // conversation text and are deliberately not replayed.
                if !message.tool_calls.is_empty() || message.tool_call_id.is_some() {
                    return Err(AgentError::Internal(
                        "coordinator history contains malformed system tool fields".to_string(),
                    ));
                }
                index += 1;
            }
            MessageRole::User => {
                if !message.tool_calls.is_empty() || message.tool_call_id.is_some() {
                    return Err(AgentError::Internal(
                        "coordinator history contains malformed user tool fields".to_string(),
                    ));
                }
                safe.push(message.clone());
                index += 1;
            }
            MessageRole::Assistant if message.tool_calls.is_empty() => {
                if message.tool_call_id.is_some() {
                    return Err(AgentError::Internal(
                        "coordinator history contains malformed assistant tool result id"
                            .to_string(),
                    ));
                }
                safe.push(message.clone());
                index += 1;
            }
            MessageRole::Assistant => {
                if message.tool_call_id.is_some() {
                    return Err(AgentError::Internal(
                        "coordinator history contains malformed assistant tool result id"
                            .to_string(),
                    ));
                }
                let calls = &message.tool_calls;
                let mut matched = vec![false; calls.len()];
                for result_offset in 0..calls.len() {
                    let result_index = index.saturating_add(1).saturating_add(result_offset);
                    let Some(result) = history.get(result_index) else {
                        return Err(AgentError::Internal(
                            "coordinator history ends inside a tool transaction".to_string(),
                        ));
                    };
                    if result.role != MessageRole::Tool || !result.tool_calls.is_empty() {
                        return Err(AgentError::Internal(
                            "coordinator history interrupts a tool transaction".to_string(),
                        ));
                    }
                    let result_id = result.tool_call_id.as_deref().filter(|id| !id.is_empty());
                    let result_name = result.name.as_deref().filter(|name| !name.is_empty());
                    let match_index = calls.iter().enumerate().position(|(call_index, call)| {
                        if matched[call_index] {
                            return false;
                        }
                        let id_matches = result_id.is_none_or(|id| call.id == id);
                        let name_matches = result_name.is_none_or(|name| call.name == name);
                        (result_id.is_some() || result_name.is_some()) && id_matches && name_matches
                    });
                    let Some(match_index) = match_index else {
                        return Err(AgentError::Internal(
                            "coordinator history has an unmatched or duplicate tool result"
                                .to_string(),
                        ));
                    };
                    matched[match_index] = true;
                }
                if matched.iter().any(|matched| !matched) {
                    return Err(AgentError::Internal(
                        "coordinator history has a missing tool result".to_string(),
                    ));
                }
                index = index.saturating_add(1).saturating_add(calls.len());
            }
            MessageRole::Tool => {
                return Err(AgentError::Internal(
                    "coordinator history contains an orphan tool result".to_string(),
                ));
            }
        }
    }
    Ok(safe)
}

/// Render the coordinator-owned, already-sanitized conversation as read-only
/// context inside a worker's assigned task. A declared Worker's Tier-1 history
/// remains private: replaying parent messages as native Worker history would
/// duplicate them across runs and could mix provider protocols. Keeping the
/// context in this turn's User input works consistently for ActorSession,
/// SuppliedHistory, and Stateless workers, and the normal selected-provider
/// preflight accounts for the complete rendered input.
fn worker_task_content(history: &[ChatMessage], description: &str) -> String {
    let mut context = String::new();
    for message in history {
        let role = match message.role {
            MessageRole::User => "User",
            MessageRole::Assistant if message.tool_calls.is_empty() => "Assistant",
            MessageRole::System | MessageRole::Assistant | MessageRole::Tool => {
                debug_assert!(false, "worker context must be sanitized before rendering");
                continue;
            }
        };
        let text = match &message.content {
            axocoatl_core::MessageContent::Text(text) => text.clone(),
            axocoatl_core::MessageContent::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    axocoatl_core::ContentPart::Text(text) => Some(text.as_str()),
                    axocoatl_core::ContentPart::Image { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        };
        if text.trim().is_empty() {
            continue;
        }
        context.push_str("### ");
        context.push_str(role);
        context.push('\n');
        context.push_str(&text);
        context.push_str("\n\n");
    }

    format!(
        "## Coordinator conversation context (read-only)\n\n{context}\
         ## Assigned subtask\n\n{description}"
    )
}

fn ordered_worker_tool_calls(outputs: &[Option<AgentOutput>]) -> Vec<ToolCallRecord> {
    outputs
        .iter()
        .filter_map(Option::as_ref)
        .flat_map(|output| output.tool_calls.iter().cloned())
        .collect()
}

fn cancelled_coordinator_output(
    items: &[OrchestrationItem],
    outputs: &[Option<AgentOutput>],
    token_usage: TokenUsageStats,
) -> AgentOutput {
    let mut sections = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let live_content = outputs
            .get(index)
            .and_then(Option::as_ref)
            .map(|output| output.content.as_str())
            .filter(|content| !content.is_empty());
        let persisted_content = match &item.outcome {
            Some(OrchestrationOutcome::Succeeded { content, .. }) if !content.is_empty() => {
                Some(content.as_str())
            }
            _ => None,
        };
        if let Some(content) = live_content.or(persisted_content) {
            sections.push(format!("## {}\n{content}", item.name));
        }
    }
    AgentOutput {
        content: sections.join("\n\n"),
        tool_calls: ordered_worker_tool_calls(outputs),
        token_usage,
    }
}

fn charge_shared_tracker(
    tracker: Option<&TokenTracker>,
    usage: &TokenUsageStats,
) -> Result<(), AgentError> {
    let Some(tracker) = tracker else {
        return Ok(());
    };
    let output = usage
        .output_tokens
        .saturating_add(usage.reasoning_tokens.unwrap_or(0));
    let result = tracker.record_usage(usage.input_tokens, output);
    match (tracker.budget().overflow_policy.clone(), result) {
        (_, Ok(())) => Ok(()),
        (OverflowPolicy::Warn, Err(error)) => {
            tracing::warn!(%error, "Coordinator aggregate usage exceeded token budget (warn policy)");
            Ok(())
        }
        (OverflowPolicy::Abort, Err(BudgetError::ExecutionBudgetExceeded { used, budget })) => {
            Err(AgentError::TokenBudgetExceeded { used, budget })
        }
        (
            OverflowPolicy::Abort,
            Err(BudgetError::WouldExceedBudget {
                current,
                requested,
                budget,
            }),
        ) => Err(AgentError::TokenBudgetExceeded {
            used: current.saturating_add(requested),
            budget,
        }),
    }
}

async fn forward_worker_tool_stream(
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<AgentStreamChunk>,
    parent: StreamSink,
    run_seq: u64,
    worker_index: usize,
    worker_id: String,
    next_group: Arc<std::sync::atomic::AtomicU64>,
) {
    use std::collections::VecDeque;
    use std::sync::atomic::Ordering;

    let mut groups = HashMap::<u64, u64>::new();
    let mut call_ids = HashMap::<(String, String), VecDeque<String>>::new();
    let mut orphan_result = 0_u64;
    while let Some(chunk) = receiver.recv().await {
        let forwarded = match chunk {
            AgentStreamChunk::ToolCallStarted {
                source_agent: _,
                id,
                name,
                arguments,
                provider_arguments,
                provider_metadata,
                assistant_content,
                provider_response_group,
                provider_call_index,
                provider_call_count,
            } => {
                let parent_group = *groups.entry(provider_response_group).or_insert_with(|| {
                    next_group
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                            current.checked_add(1)
                        })
                        .unwrap_or(u64::MAX)
                });
                let evidence_id = format!(
                    "coord-r{run_seq}-w{worker_index}-g{parent_group}-i{provider_call_index}"
                );
                call_ids
                    .entry((id, name.clone()))
                    .or_default()
                    .push_back(evidence_id.clone());
                AgentStreamChunk::ToolCallStarted {
                    source_agent: Some(worker_id.clone()),
                    id: evidence_id,
                    name,
                    arguments,
                    provider_arguments,
                    provider_metadata,
                    assistant_content,
                    provider_response_group: parent_group,
                    provider_call_index,
                    provider_call_count,
                }
            }
            AgentStreamChunk::ToolCallResult {
                source_agent: _,
                id,
                name,
                result,
                is_error,
            } => {
                let evidence_id = call_ids
                    .get_mut(&(id, name.clone()))
                    .and_then(VecDeque::pop_front)
                    .unwrap_or_else(|| {
                        let id = format!(
                            "coord-r{run_seq}-w{worker_index}-orphan-result-{orphan_result}"
                        );
                        orphan_result = orphan_result.saturating_add(1);
                        id
                    });
                AgentStreamChunk::ToolCallResult {
                    source_agent: Some(worker_id.clone()),
                    id: evidence_id,
                    name,
                    result,
                    is_error,
                }
            }
            AgentStreamChunk::Text(_) | AgentStreamChunk::Reasoning(_) => continue,
        };
        if parent.send(forwarded).is_err() {
            break;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum OrchestrationOutcome {
    Succeeded {
        content: String,
        #[serde(default)]
        tool_calls: Vec<ToolCallRecord>,
        #[serde(default)]
        token_usage: TokenUsageStats,
    },
    Failed {
        error: String,
    },
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
    /// A worker returned a handled failure. Default is a compatibility no-op;
    /// product reporters should render a terminal failed state.
    fn worker_failed(&self, _workflow: &str, _worker: &str, _error: &str) {}
    /// A worker reached a cooperative cancellation boundary with honest partial
    /// output/usage. This is distinct from completion and failure.
    fn worker_cancelled(
        &self,
        _workflow: &str,
        _worker: &str,
        _partial_output: &str,
        _tokens: u64,
    ) {
    }
    /// A worker task panicked. Kept distinct so observers never show a stale
    /// running node or a false successful completion.
    fn worker_panicked(&self, _workflow: &str, _worker: &str, _error: &str) {}
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
    run_provider_usage: TokenUsageStats,
    /// Usage from prior terminal runs. The active run is tracked separately so
    /// orchestration checkpoints can resume without double counting.
    lifetime_token_usage: TokenUsageStats,
    lifetime_token_usage_known: bool,
    active_run_usage: ExecutionUsageState,
    last_execution_usage: std::sync::Mutex<axocoatl_core::MeasuredTokenUsage>,
    tool_executor: Option<Arc<ToolExecutor>>,
    system_prompt: Option<String>,
    agent_id: String,
    /// The coordinator's own model, inherited by ad-hoc workers (those spawned
    /// when no pooled worker bids) so they run on the same provider/model rather
    /// than the `gpt-4o` default, which fails on a local Ollama setup.
    model: String,
    sampling: SamplingConfig,
    token_budget: Option<TokenBudget>,
    session: SessionMemory,
    session_context: Option<String>,
    session_working_dir: Option<String>,
    project_instructions: Option<String>,
    project_instructions_root: Option<std::path::PathBuf>,

    /// Configurations for worker agents this coordinator can spawn.
    worker_configs: Vec<WorkerConfig>,
    /// Stable product-facing identity for each internally scoped worker id.
    /// Checkpoints/actors use the scoped id; reporters and stream attribution
    /// use this logical id so runtime plumbing never leaks into the UI.
    worker_logical_ids: HashMap<AgentId, String>,
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
    /// Already-opened control-plane root for per-worker memory. Workers inherit
    /// this capability rather than reopening its ambient pathname.
    data_root: Option<SecureDir>,
    /// Version counter for the coordinator's own orchestration checkpoints.
    checkpoint_version: u64,
    /// Orchestration state restored from a checkpoint in `on_start`; consumed by
    /// the next run if its goal matches (resume), else discarded (fresh run).
    resumed_state: Option<OrchestrationState>,
    /// Optional observer of run progress (decompose, auction, workers). The
    /// daemon sets this to forward Layer-2 progress to the dashboard stream.
    reporter: Option<Arc<dyn CoordinatorReporter>>,
    /// Parent execution sink. Worker text remains isolated, while tool
    /// start/result evidence is occurrence-safely forwarded into this stream.
    stream_sink: Option<StreamSink>,
}

impl CoordinatorBehavior {
    pub fn new(provider: Arc<dyn LlmProvider>, counter: Arc<dyn TokenCounter>) -> Self {
        Self {
            provider,
            counter,
            tracker: None,
            run_provider_usage: TokenUsageStats::default(),
            lifetime_token_usage: TokenUsageStats::default(),
            lifetime_token_usage_known: true,
            active_run_usage: ExecutionUsageState::default(),
            last_execution_usage: std::sync::Mutex::new(axocoatl_core::MeasuredTokenUsage::known(
                TokenUsageStats::default(),
            )),
            tool_executor: None,
            system_prompt: None,
            agent_id: String::new(),
            model: String::new(),
            sampling: SamplingConfig::default(),
            token_budget: None,
            session: SessionMemory::new(),
            session_context: None,
            session_working_dir: None,
            project_instructions: None,
            project_instructions_root: None,
            worker_configs: Vec::new(),
            worker_logical_ids: HashMap::new(),
            active_workers: HashMap::new(),
            worker_handles: Vec::new(),
            worker_results: Vec::new(),
            htn_planner: None,
            run_seq: 0,
            checkpoint_store: None,
            shared_blocks: std::collections::HashMap::new(),
            hook_registry: None,
            data_root: None,
            checkpoint_version: 0,
            resumed_state: None,
            reporter: None,
            stream_sink: None,
        }
    }

    fn active_run_usage_snapshot(&self) -> TokenUsageStats {
        self.active_run_usage.usage_snapshot()
    }

    fn set_active_run_usage(&self, usage: TokenUsageStats) {
        self.active_run_usage.set(usage, true);
    }

    fn restore_active_run_usage(&self, usage: TokenUsageStats, known: bool) {
        self.active_run_usage.set(usage, known);
    }

    fn merge_active_run_usage(&self, usage: &TokenUsageStats) {
        self.active_run_usage.merge(usage);
    }

    fn mark_active_run_usage_unknown(&self) {
        self.active_run_usage.mark_unknown();
    }

    fn adopt_worker_measurement(
        &self,
        output: &mut AgentOutput,
        measured: &axocoatl_core::MeasuredTokenUsage,
        total_usage: &mut TokenUsageStats,
    ) {
        output.token_usage = measured.usage.clone();
        total_usage.merge(&measured.usage);
        self.merge_active_run_usage(&measured.usage);
        if !measured.complete {
            self.mark_active_run_usage_unknown();
        }
    }

    fn active_run_usage_known(&self) -> bool {
        self.active_run_usage.snapshot().is_some()
    }

    fn cumulative_token_usage_snapshot(&self) -> TokenUsageStats {
        let mut usage = self.lifetime_token_usage.clone();
        usage.merge(&self.active_run_usage_snapshot());
        usage
    }

    fn cumulative_token_usage_known(&self) -> bool {
        self.lifetime_token_usage_known && self.active_run_usage_known()
    }

    fn cumulative_token_usage_measurement(&self) -> axocoatl_core::MeasuredTokenUsage {
        let usage = self.cumulative_token_usage_snapshot();
        if self.cumulative_token_usage_known() {
            axocoatl_core::MeasuredTokenUsage::known(usage)
        } else {
            axocoatl_core::MeasuredTokenUsage::lower_bound(usage)
        }
    }

    fn finalize_active_run_usage(&mut self) {
        let active = self.active_run_usage_snapshot();
        *self
            .last_execution_usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            self.active_run_usage.measurement();
        self.lifetime_token_usage.merge(&active);
        self.lifetime_token_usage_known &= self.active_run_usage_known();
        self.set_active_run_usage(TokenUsageStats::default());
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

    fn shared_blocks_for_worker(
        &self,
        memory: &MemoryConfig,
    ) -> std::collections::HashMap<String, axocoatl_memory::SharedBlock> {
        memory
            .core
            .blocks
            .iter()
            .filter(|block| block.shared)
            .filter_map(|block| {
                self.shared_blocks
                    .get(&block.label)
                    .cloned()
                    .map(|handle| (block.label.clone(), handle))
            })
            .collect()
    }

    pub fn with_hook_registry(mut self, registry: Arc<HookRegistry>) -> Self {
        self.hook_registry = Some(registry);
        self
    }

    /// Set the data directory used for per-worker semantic memory stores.
    pub fn with_data_dir(mut self, data_dir: String) -> Self {
        match SecureDir::open_or_create_all(&data_dir) {
            Ok(root) => self.data_root = Some(root),
            Err(error) => {
                tracing::warn!(%data_dir, %error, "worker memory root is unavailable");
                self.data_root = None;
            }
        }
        self
    }

    /// Attach the exact data-root capability already opened by the daemon.
    pub fn with_data_root(mut self, data_root: SecureDir) -> Self {
        self.data_root = Some(data_root);
        self
    }

    /// Set the coordinator's own model. Ad-hoc workers inherit it so they run on
    /// the coordinator's provider/model instead of the `gpt-4o` default.
    pub fn with_model(mut self, model: String) -> Self {
        self.model = model;
        self
    }

    /// Bind coordinator-side decomposition/frontier/synthesis calls to the same
    /// directory Session context supplied to standalone agents and workers.
    pub fn with_session_context(mut self, working_dir: impl std::fmt::Display) -> Self {
        let working_dir = working_dir.to_string();
        self.session_context = Some(format!(
            "You are working inside a directory session. Your working \
             directory is `{working_dir}`. All file and shell tools operate \
             inside a sandboxed container with that directory mounted — you \
             cannot reach anything outside it."
        ));
        self.session_working_dir = Some(working_dir);
        self
    }

    /// Load the same secure, root-to-leaf project instructions used by a
    /// standalone agent for coordinator-side provider calls.
    pub fn with_project_instructions(mut self, working_dir: &std::path::Path) -> Self {
        self.project_instructions = load_project_instructions(working_dir);
        self.project_instructions_root = Some(working_dir.to_path_buf());
        self
    }

    /// Add a worker configuration. Workers with these configs can be spawned on demand.
    pub fn add_worker_config(mut self, config: WorkerConfig) -> Self {
        self.worker_logical_ids
            .entry(config.id.clone())
            .or_insert_with(|| config.id.to_string());
        self.worker_configs.push(config);
        self
    }

    /// Add a worker whose actor/checkpoint id is runtime-scoped while exposing
    /// a stable configured identity to progress reporters and tool evidence.
    pub fn add_worker_config_with_logical_id(
        mut self,
        config: WorkerConfig,
        logical_id: impl Into<String>,
    ) -> Self {
        self.worker_logical_ids
            .insert(config.id.clone(), logical_id.into());
        self.worker_configs.push(config);
        self
    }

    fn validate_worker_identities(&self) -> Result<(), AgentError> {
        let mut runtime_ids = HashSet::new();
        let mut logical_ids = HashSet::new();
        for worker in &self.worker_configs {
            if !runtime_ids.insert(worker.id.clone()) {
                return Err(AgentError::Internal(format!(
                    "coordinator has duplicate worker runtime id '{}'",
                    worker.id
                )));
            }
            let logical_id = self
                .worker_logical_ids
                .get(&worker.id)
                .cloned()
                .unwrap_or_else(|| worker.id.to_string());
            if !logical_ids.insert(logical_id.clone()) {
                return Err(AgentError::Internal(format!(
                    "coordinator has duplicate logical worker id '{logical_id}'"
                )));
            }
        }
        Ok(())
    }

    /// Attach an HTN planner. When set, `decompose_task` tries symbolic
    /// decomposition (no LLM call) before falling back to the LLM.
    pub fn with_htn_methods(mut self, planner: HtnPlanner) -> Self {
        self.htn_planner = Some(planner);
        self
    }

    /// Spawn a worker agent and return its ID.
    async fn spawn_worker(
        &mut self,
        config: &WorkerConfig,
        exact_executor_tools: bool,
        persist_worker_session: bool,
    ) -> Result<AgentId, AgentError> {
        let worker_provider = config
            .provider
            .clone()
            .unwrap_or_else(|| self.provider.clone());
        let agent_config = AgentConfig {
            id: config.id.clone(),
            name: config.name.clone(),
            provider: worker_provider.provider_id().to_string(),
            // The worker's own model — without this it would fall back to
            // `AgentConfig::default()`'s `gpt-4o`, which 404s on a local provider.
            model: config.model.clone(),
            token_budget: config.token_budget.clone(),
            system_prompt: Some(config.system_prompt.clone()),
            tools: config.tools.clone(),
            sampling: config.sampling.clone(),
            memory: config.memory.clone(),
            ..AgentConfig::default()
        };

        // Build the worker with the full agent stack so it is a first-class
        // agent, not a bare provider+tools shell: checkpointing, daily/core/
        // semantic memory, the global hook registry, and tool execution.
        let mut behavior = DefaultAgentBehavior::new(worker_provider, self.counter.clone());
        if let Some(executor) = &self.tool_executor {
            behavior = behavior.with_tool_executor(executor.clone());
        }
        if exact_executor_tools {
            behavior = behavior.with_executor_tool_allowlist(config.tools.clone());
        }
        if persist_worker_session {
            if let Some(store) = &self.checkpoint_store {
                behavior = behavior.with_checkpoint_store(store.clone());
            }
        }
        if let Some(hooks) = &self.hook_registry {
            behavior = behavior.with_hook_registry(hooks.clone());
        }
        if let Some(context) = &config.session_context {
            behavior = behavior.with_session_context(context);
        }
        if let Some(root) = &config.project_instructions_root {
            behavior = behavior.with_project_instructions(root);
        }
        // Per-worker Tier-2/3/4 stores under the coordinator's data dir (same
        // scheme as a standalone agent). Built only when a data dir is configured
        // (the daemon sets it); omitted in lightweight/embedded use. Non-fatal.
        if persist_worker_session {
            if let Some(data_root) = &self.data_root {
                // Daily log is an optional bounded model-memory cache. Canonical
                // Session history remains the exact durable transcript.
                let daily = DailyLogMemory::new_in_secure(
                    config.id.to_string(),
                    data_root,
                    "memory/daily_log",
                )
                .map_err(|error| AgentError::Internal(error.to_string()))?;
                behavior = behavior.with_daily_log(Arc::new(daily));
                let semantic = SemanticMemory::new_in_secure(
                    &config.id.to_string(),
                    data_root,
                    "memory/semantic",
                )
                .map_err(|error| {
                    AgentError::InitFailed(format!(
                        "worker '{}' semantic memory is unavailable: {error}",
                        config.id
                    ))
                })?;
                behavior = behavior.with_semantic_memory(Arc::new(semantic));
                // Core memory (Tier 3) — local blocks plus only the shared labels
                // explicitly declared by this worker.
                let specs: Vec<axocoatl_memory::MemoryBlock> = config
                    .memory
                    .core
                    .blocks
                    .iter()
                    .map(axocoatl_memory::MemoryBlock::from)
                    .collect();
                let store = axocoatl_memory::build_store_with_legacy_in_secure(
                    &config.id.to_string(),
                    data_root,
                    &specs,
                )
                .await
                .map_err(|error| AgentError::Internal(error.to_string()))?;
                let worker_shared_blocks = self.shared_blocks_for_worker(&config.memory);
                behavior = behavior.with_core_memory(
                    Arc::new(tokio::sync::RwLock::new(store)),
                    worker_shared_blocks,
                );
            }
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
            session_messages: self.session.messages().to_vec(),
            cumulative_token_usage: self.cumulative_token_usage_snapshot(),
            cumulative_token_usage_known: self.cumulative_token_usage_known(),
            behavior_state,
        };
        if let Err(e) = store.save(&ckpt).await {
            tracing::warn!(coordinator = %self.agent_id, error = %e, "failed to checkpoint orchestration");
        }
    }

    /// Clear resumable actor-internal orchestration after a terminal failure.
    /// Canonical Session history remains authoritative; this only tombstones
    /// the coordinator's behavior cache so a later turn cannot silently resume
    /// work that the Session already terminalized as failed/interrupted.
    async fn clear_orchestration_state(&mut self) {
        self.resumed_state = None;
        let Some(store) = self.checkpoint_store.clone() else {
            return;
        };
        self.checkpoint_version = self.checkpoint_version.saturating_add(1);
        let checkpoint_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let checkpoint = AgentCheckpoint {
            version: self.checkpoint_version,
            agent_id: self.agent_id.clone(),
            checkpoint_time,
            session_messages: self.session.messages().to_vec(),
            cumulative_token_usage: self.cumulative_token_usage_snapshot(),
            cumulative_token_usage_known: self.cumulative_token_usage_known(),
            behavior_state: None,
        };
        if let Err(error) = store.save(&checkpoint).await {
            tracing::warn!(
                coordinator = %self.agent_id,
                %error,
                "failed to clear terminal coordinator behavior state"
            );
        }
    }

    /// Persist lifetime accounting for request-local coordinator executions
    /// without adopting their history or orchestration state into Tier 1.
    ///
    /// Supplied-history and stateless calls are conversation-pure, but they can
    /// still incur provider usage.  The checkpoint therefore keeps the
    /// coordinator's canonical actor transcript and any pre-existing resumable
    /// ActorSession state unchanged while advancing only lifetime accounting.
    async fn checkpoint_accounting_only(&mut self) -> Result<(), AgentError> {
        let Some(store) = self.checkpoint_store.clone() else {
            return Ok(());
        };
        self.checkpoint_version = self.checkpoint_version.saturating_add(1);
        let checkpoint_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let behavior_state = self
            .resumed_state
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| {
                AgentError::Internal(format!(
                    "failed to serialize coordinator accounting checkpoint: {error}"
                ))
            })?;
        let checkpoint = AgentCheckpoint {
            version: self.checkpoint_version,
            agent_id: self.agent_id.clone(),
            checkpoint_time,
            session_messages: self.session.messages().to_vec(),
            cumulative_token_usage: self.cumulative_token_usage_snapshot(),
            cumulative_token_usage_known: self.cumulative_token_usage_known(),
            behavior_state,
        };
        store.save(&checkpoint).await.map_err(|error| {
            AgentError::Internal(format!(
                "failed to persist coordinator usage accounting: {error}"
            ))
        })
    }

    fn build_provider_request(
        &self,
        context: &CoordinatorRequestContext,
        internal_system: &str,
        user: String,
    ) -> (ChatRequest, usize) {
        let mut system_parts = Vec::new();
        if let Some(configured) = context.system.as_deref() {
            system_parts.push(configured.to_string());
        }
        if let Some(session_context) = &self.session_context {
            system_parts.push(session_context.clone());
        }
        if let Some(project_instructions) = &self.project_instructions {
            system_parts.push(project_instructions.clone());
        }
        if !internal_system.is_empty() {
            system_parts.push(internal_system.to_string());
        }
        let system = system_parts.join("\n\n");
        let mut messages = Vec::new();
        if !system.is_empty() {
            messages.push(ChatMessage::system(system));
        }
        let history_start = messages.len();
        messages.extend(context.history.iter().cloned());
        let final_prompt_index = messages.len();
        let protected_suffix_start = context
            .history
            .iter()
            .rposition(|message| message.role == MessageRole::User)
            .map_or(final_prompt_index, |index| {
                history_start.saturating_add(index)
            });
        messages.push(ChatMessage::user(user));
        let mut request = ChatRequest {
            messages,
            tools: Vec::new(),
            max_tokens: self.sampling.max_tokens,
            temperature: self.sampling.temperature,
            top_p: self.sampling.top_p,
            response_format: self.sampling.response_format,
            stop_sequences: Vec::new(),
            provider_options: None,
            model_override: context.model.clone(),
        };
        attach_to_last_user_message(&mut request, &context.attachments);
        (request, protected_suffix_start)
    }

    /// Decompose a goal into subtasks. Prefers the symbolic HTN planner: it
    /// plans, resolves any LLM frontiers (decomposing only those tasks with the
    /// model, not the whole goal), and errors if the plan can't be made fully
    /// primitive. Only when no planner is configured does it decompose the whole
    /// goal with the LLM. Either way, an empty decomposition is an error.
    async fn decompose_task(
        &self,
        task: &str,
        request_context: &CoordinatorRequestContext,
        control: Option<&AgentRunControl>,
    ) -> Result<(Vec<Subtask>, TokenUsageStats), AgentError> {
        if let Some(planner) = &self.htn_planner {
            let root = HtnTask {
                name: task.to_string(),
                parameters: HashMap::new(),
                task_type: HtnTaskType::Compound,
            };
            // resolve_frontiers takes &mut self; clone so the shared planner is
            // left untouched across runs.
            let mut planner = planner.clone();
            let resolver = LlmFrontierResolver::new(self.provider.clone(), self.counter.clone())
                .with_tracker(self.tracker.clone())
                .with_control(control.cloned())
                .with_model(request_context.model.clone())
                .with_request_context(
                    request_context.history.clone(),
                    request_context.system.clone(),
                    request_context.attachments.clone(),
                    self.sampling.clone(),
                );
            let plan_result = planner.resolve_frontiers(root, &resolver, 4).await;
            let resolver_usage = resolver.usage();
            self.merge_active_run_usage(&resolver_usage);
            if !resolver.usage_known() {
                self.mark_active_run_usage_unknown();
            }
            let plan = match plan_result {
                Ok(plan) => plan,
                Err(message) => {
                    if let Some(error) = resolver.take_failure() {
                        return Err(error);
                    }
                    return Err(AgentError::Internal(message));
                }
            };
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
            return Ok((subtasks, resolver_usage));
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
        let (request, protected_suffix_start) = self.build_provider_request(
            request_context,
            "You decompose tasks into subtasks. Return only valid JSON.",
            decompose_prompt,
        );
        let response = match provider_budget::chat(
            self.provider.as_ref(),
            self.counter.as_ref(),
            self.tracker.as_ref(),
            Some(&self.active_run_usage),
            request,
            protected_suffix_start,
            control,
        )
        .await?
        {
            ControlledChat::Response(response) => response,
            ControlledChat::Cancelled => {
                return Err(AgentError::Internal(
                    "coordinator decomposition cancelled".to_string(),
                ));
            }
        };

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
        Ok((subtasks, response.usage))
    }
}

#[async_trait::async_trait]
impl AgentBehavior for CoordinatorBehavior {
    fn cumulative_token_usage(&self) -> Option<TokenUsageStats> {
        Some(self.cumulative_token_usage_snapshot())
    }

    fn cumulative_token_usage_measurement(&self) -> Option<axocoatl_core::MeasuredTokenUsage> {
        Some(CoordinatorBehavior::cumulative_token_usage_measurement(
            self,
        ))
    }

    fn last_execution_token_usage(&self) -> Option<TokenUsageStats> {
        let measured = self
            .last_execution_usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        measured.complete.then_some(measured.usage)
    }

    fn last_execution_token_usage_measurement(&self) -> Option<axocoatl_core::MeasuredTokenUsage> {
        Some(
            self.last_execution_usage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
    }

    async fn on_start(&mut self, config: &AgentConfig) -> Result<(), AgentError> {
        self.system_prompt = config.system_prompt.clone();
        self.agent_id = config.id.to_string();
        self.model = config.model.clone();
        self.lifetime_token_usage = TokenUsageStats::default();
        self.lifetime_token_usage_known = true;
        self.set_active_run_usage(TokenUsageStats::default());

        // Restore an incomplete orchestration so the next run can resume it
        // (same model as a normal agent restoring its session on restart).
        if let Some(store) = &self.checkpoint_store {
            if let Ok(Some(ckpt)) = store.load_latest(&config.id).await {
                self.checkpoint_version = ckpt.version;
                self.lifetime_token_usage = ckpt.cumulative_token_usage.clone();
                self.lifetime_token_usage_known = ckpt.cumulative_token_usage_known;
                self.session.restore(ckpt.session_messages);
                if let Some(json) = ckpt.behavior_state {
                    match serde_json::from_str::<OrchestrationState>(&json) {
                        Ok(state) if !state.completed => {
                            self.lifetime_token_usage = state.lifetime_usage_before_run.clone();
                            self.lifetime_token_usage_known = state.lifetime_usage_before_run_known;
                            self.restore_active_run_usage(
                                state.token_usage.clone(),
                                state.token_usage_known,
                            );
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

        self.sampling = config.sampling.clone();
        self.token_budget = config.token_budget.clone();
        self.tracker = self
            .token_budget
            .clone()
            .map(|budget| TokenTracker::new(budget, self.counter.clone()));
        self.run_provider_usage = TokenUsageStats::default();
        *self
            .last_execution_usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            axocoatl_core::MeasuredTokenUsage::known(TokenUsageStats::default());

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
        let persist_actor_session =
            input.effective_conversation_mode() == ConversationMode::ActorSession;
        let result = self.run_once(input, None).await;
        self.stop_all_workers().await;
        if result.is_err() && persist_actor_session {
            self.clear_orchestration_state().await;
        }
        self.finalize_active_run_usage();
        if !persist_actor_session {
            if let Err(checkpoint_error) = self.checkpoint_accounting_only().await {
                return match result {
                    Ok(_) => Err(checkpoint_error),
                    Err(run_error) => Err(AgentError::Internal(format!(
                        "{run_error}; additionally {checkpoint_error}"
                    ))),
                };
            }
        }
        result.map(|outcome| match outcome {
            CoordinatorRunOutcome::Completed(output) | CoordinatorRunOutcome::Cancelled(output) => {
                output
            }
        })
    }

    async fn execute_controlled(
        &mut self,
        input: AgentInput,
        control: AgentRunControl,
    ) -> Result<AgentRunOutcome, AgentError> {
        let run_id = control.id().clone();
        let persist_actor_session =
            input.effective_conversation_mode() == ConversationMode::ActorSession;
        let result = self.run_once(input, Some(&control)).await;
        // Cleanup is deliberately outside every cancellation race. Once a
        // worker exists, its actor and task are always joined before returning.
        self.stop_all_workers().await;
        if result.is_err() && persist_actor_session {
            self.clear_orchestration_state().await;
        }
        self.finalize_active_run_usage();
        if !persist_actor_session {
            if let Err(checkpoint_error) = self.checkpoint_accounting_only().await {
                return match result {
                    Ok(_) => Err(checkpoint_error),
                    Err(run_error) => Err(AgentError::Internal(format!(
                        "{run_error}; additionally {checkpoint_error}"
                    ))),
                };
            }
        }
        result.map(|outcome| match outcome {
            CoordinatorRunOutcome::Completed(output) => AgentRunOutcome::Completed(output),
            CoordinatorRunOutcome::Cancelled(partial_output) => AgentRunOutcome::Cancelled {
                run_id,
                partial_output,
            },
        })
    }

    fn set_stream_sink(&mut self, sink: Option<StreamSink>) {
        self.stream_sink = sink;
    }

    async fn on_stop(&mut self) -> Result<(), AgentError> {
        self.stop_all_workers().await;
        tracing::info!(coordinator = %self.agent_id, "Coordinator stopped");
        Ok(())
    }
}

impl CoordinatorBehavior {
    async fn finish_cancelled_run(
        &mut self,
        _goal: &str,
        items: &[OrchestrationItem],
        outputs: &[Option<AgentOutput>],
        token_usage: TokenUsageStats,
        persist_actor_session: bool,
    ) -> CoordinatorRunOutcome {
        // A cancelled actor-owned run is terminal, not resumable. Persist a
        // completed tombstone after every started worker has reached its safe
        // boundary so actor replacement cannot silently resume stopped work.
        // Supplied/stateless calls never mutate actor-owned orchestration state.
        if persist_actor_session {
            self.resumed_state = None;
        }
        let partial_output = cancelled_coordinator_output(items, outputs, token_usage.clone());
        if !partial_output.content.is_empty() {
            if let Some(sink) = &self.stream_sink {
                let _ = sink.send(AgentStreamChunk::Text(partial_output.content.clone()));
            }
            if persist_actor_session {
                let tokens = self.counter.count_text(&partial_output.content);
                self.session
                    .append(MessageRole::Assistant, &partial_output.content, tokens);
            }
        }
        if persist_actor_session {
            self.clear_orchestration_state().await;
        }
        CoordinatorRunOutcome::Cancelled(partial_output)
    }

    /// One coordination pass: decompose, assign each subtask to a worker by
    /// auction, run the workers in parallel, and synthesize their results.
    /// Worker teardown is the caller's responsibility — [`execute`] always tears
    /// down afterward, on success and on every error path.
    async fn run_once(
        &mut self,
        input: AgentInput,
        control: Option<&AgentRunControl>,
    ) -> Result<CoordinatorRunOutcome, AgentError> {
        // A configured worker is removed from the per-run auction pool after it
        // wins once, so parallel assignments are unique. Reject duplicate
        // runtime/logical configuration up front as those would otherwise
        // collide in the actor registry or collapse reporter/UI state.
        self.validate_worker_identities()?;
        self.tracker = self
            .token_budget
            .clone()
            .map(|budget| TokenTracker::new(budget, self.counter.clone()));
        self.run_provider_usage = TokenUsageStats::default();
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
        let conversation_mode = input.effective_conversation_mode();
        let persist_actor_session = conversation_mode == ConversationMode::ActorSession;
        let goal = input.content.clone();
        let mut raw_history = if persist_actor_session {
            let mut history = self.session.as_chat_messages();
            history.extend(input.history.iter().cloned());
            history
        } else {
            input.history.clone()
        };
        let mut history = sanitize_coordinator_history(&raw_history)?;
        raw_history.clear();
        history.push(ChatMessage::user(&goal));
        if persist_actor_session {
            self.session
                .replace_with_chat_messages(&history, |text| self.counter.count_text(text));
        }
        let request_context = CoordinatorRequestContext {
            history,
            system: input
                .system_override
                .clone()
                .or_else(|| self.system_prompt.clone()),
            model: input
                .model_override
                .clone()
                .or_else(|| (!self.model.is_empty()).then(|| self.model.clone())),
            attachments: input.attachments.clone(),
            conversation_mode,
        };
        let mut resumed_for_run = if persist_actor_session {
            self.resumed_state.take()
        } else {
            None
        };
        let mut total_usage = TokenUsageStats::default();
        if control.is_some_and(AgentRunControl::is_cancelled) {
            let resumed_items = match resumed_for_run.take() {
                Some(state) if state.goal == goal => state.items,
                Some(state) => {
                    if persist_actor_session {
                        self.resumed_state = Some(state);
                    }
                    Vec::new()
                }
                None => Vec::new(),
            };
            return Ok(self
                .finish_cancelled_run(
                    &goal,
                    &resumed_items,
                    &[],
                    total_usage,
                    persist_actor_session,
                )
                .await);
        }

        // 1. Build the work list: resume an incomplete checkpointed run for the
        //    same goal (skipping work already done), else decompose fresh.
        let mut items: Vec<OrchestrationItem> = match resumed_for_run.take() {
            Some(state) if !state.completed && state.goal == goal => {
                let done = state.items.iter().filter(|i| i.outcome.is_some()).count();
                self.lifetime_token_usage = state.lifetime_usage_before_run.clone();
                self.lifetime_token_usage_known = state.lifetime_usage_before_run_known;
                self.restore_active_run_usage(state.token_usage.clone(), state.token_usage_known);
                total_usage = state.token_usage.clone();
                self.run_provider_usage = state.coordinator_provider_usage.clone();
                charge_shared_tracker(self.tracker.as_ref(), &self.run_provider_usage)?;
                tracing::info!(
                    coordinator = %self.agent_id,
                    done,
                    total = state.items.len(),
                    "Resuming orchestration from checkpoint"
                );
                state.items
            }
            _ => {
                // A nonmatching stale resumable run is terminal for accounting
                // purposes even though its work is not resumed into this goal.
                // Fold its already-incurred usage exactly once before starting
                // the fresh run.
                if self.active_run_usage_snapshot().total() > 0 {
                    self.finalize_active_run_usage();
                }
                self.set_active_run_usage(TokenUsageStats::default());
                let (subtasks, decomposition_usage) =
                    match self.decompose_task(&goal, &request_context, control).await {
                        Ok(result) => result,
                        Err(_) if control.is_some_and(AgentRunControl::is_cancelled) => {
                            return Ok(self
                                .finish_cancelled_run(
                                    &goal,
                                    &[],
                                    &[],
                                    total_usage,
                                    persist_actor_session,
                                )
                                .await);
                        }
                        Err(error) => return Err(error),
                    };
                total_usage.merge(&decomposition_usage);
                self.run_provider_usage.merge(&decomposition_usage);
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
        let mut ordered_outputs: Vec<Option<AgentOutput>> = items
            .iter()
            .map(|item| match &item.outcome {
                Some(OrchestrationOutcome::Succeeded {
                    content,
                    tool_calls,
                    token_usage,
                }) => Some(AgentOutput {
                    content: content.clone(),
                    tool_calls: tool_calls.clone(),
                    token_usage: token_usage.clone(),
                }),
                _ => None,
            })
            .collect();
        if control.is_some_and(AgentRunControl::is_cancelled) {
            return Ok(self
                .finish_cancelled_run(
                    &goal,
                    &items,
                    &ordered_outputs,
                    total_usage,
                    persist_actor_session,
                )
                .await);
        }

        // Persist the plan so a crash after decomposition doesn't re-decompose.
        let mut state = OrchestrationState {
            goal: goal.clone(),
            items: items.clone(),
            lifetime_usage_before_run: self.lifetime_token_usage.clone(),
            lifetime_usage_before_run_known: self.lifetime_token_usage_known,
            token_usage: total_usage.clone(),
            token_usage_known: self.active_run_usage_known(),
            coordinator_provider_usage: self.run_provider_usage.clone(),
            completed: false,
        };
        if persist_actor_session {
            self.checkpoint_orchestration(&state).await;
        }

        // 2. Assign each PENDING subtask to a worker by auction (best fit by tool
        //    match and budget); already-completed items are skipped entirely.
        let pending: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.outcome.is_none())
            .map(|(i, _)| i)
            .collect();
        if conversation_mode == ConversationMode::Stateless {
            let mut required = pending
                .iter()
                .flat_map(|index| items[*index].required_tools.iter().cloned())
                .collect::<Vec<_>>();
            required.sort();
            required.dedup();
            if !required.is_empty() {
                return Err(AgentError::ToolFailed {
                    tool: required.join(", "),
                    reason: "stateless coordination is a pure single inference and cannot execute required tools"
                        .to_string(),
                });
            }
        }
        let mut available = self.worker_configs.clone();
        let coord_id = self.agent_id.clone();
        let coord_model = request_context
            .model
            .clone()
            .unwrap_or_else(|| self.model.clone());
        let coord_run_seq = self.run_seq;
        let coord_sampling = self.sampling.clone();
        let coord_session_context = self.session_working_dir.clone();
        let coord_project_root = self.project_instructions_root.clone();
        let worker_logical_ids = self.worker_logical_ids.clone();
        let inherited_executor_tools: HashSet<String> = self
            .tool_executor
            .as_ref()
            .map(|executor| executor.tool_names())
            .unwrap_or_default()
            .into_iter()
            .collect();
        let declared_workers_have_memory = persist_actor_session && self.data_root.is_some();
        let mut assignments: Vec<(usize, AgentId, String)> = Vec::new();
        let mut assigned_workers: Vec<Option<AgentId>> = vec![None; items.len()];
        let mut assigned_reporter_ids: Vec<Option<String>> = vec![None; items.len()];
        // The auction outcome per subtask, reported to observers (the dashboard
        // run view) once the whole plan is assigned.
        let mut plan: Vec<ReportedSubtask> = Vec::new();

        for &idx in &pending {
            if control.is_some_and(AgentRunControl::is_cancelled) {
                return Ok(self
                    .finish_cancelled_run(
                        &goal,
                        &items,
                        &ordered_outputs,
                        total_usage,
                        persist_actor_session,
                    )
                    .await);
            }
            let item = &items[idx];
            let required_tools = &item.required_tools;
            // An ad-hoc worker granted exactly the subtask's required tools —
            // used when the pool is empty or no pooled worker can cover the
            // tools, so a subtask is never forced onto an unfit worker.
            let make_adhoc = || -> Result<WorkerConfig, AgentError> {
                let mut callable: Vec<_> = inherited_executor_tools.iter().cloned().collect();
                callable.sort();
                let missing = missing_required_tools(required_tools, &callable);
                if !missing.is_empty() {
                    return Err(AgentError::ToolFailed {
                        tool: missing.join(", "),
                        reason: "no worker can call the required canonical executor tool(s); \
                                 ad-hoc workers have no intrinsic memory tools"
                            .to_string(),
                    });
                }
                Ok(WorkerConfig {
                    id: AgentId::new(format!("{coord_id}:adhoc:{coord_run_seq}:{idx}")),
                    name: format!("Worker {idx}"),
                    system_prompt: format!(
                        "You are a worker agent. Your task: {}",
                        item.description
                    ),
                    tools: required_tools.clone(),
                    // Inherit the coordinator's model so an ad-hoc worker runs on the
                    // same (local) provider rather than the `gpt-4o` default.
                    model: coord_model.clone(),
                    provider: None,
                    token_budget: None,
                    sampling: coord_sampling.clone(),
                    memory: MemoryConfig::default(),
                    session_context: coord_session_context.clone(),
                    project_instructions_root: coord_project_root.clone(),
                })
            };
            let mut reported_bids: Vec<ReportedBid> = Vec::new();
            let mut adhoc = false;
            let worker_config = if available.is_empty() {
                adhoc = true;
                make_adhoc()?
            } else {
                let bids: Vec<AgentBid> = available
                    .iter()
                    .map(|wc| {
                        let effective_tools = callable_tools_for_declared_worker(
                            &wc.tools,
                            &inherited_executor_tools,
                            declared_workers_have_memory,
                        );
                        let ac = AgentConfig {
                            id: wc.id.clone(),
                            tools: effective_tools,
                            ..AgentConfig::default()
                        };
                        let bid_budget = wc
                            .token_budget
                            .as_ref()
                            .map(|budget| budget.per_execution)
                            .unwrap_or(DEFAULT_WORKER_BUDGET);
                        compute_bid(&ac, required_tools, 0, bid_budget)
                    })
                    .collect();
                reported_bids = bids
                    .iter()
                    .map(|b| ReportedBid {
                        worker: worker_logical_ids
                            .get(&b.agent_id)
                            .cloned()
                            .unwrap_or_else(|| b.agent_id.to_string()),
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
                        make_adhoc()?
                    }
                }
            };
            let worker_id = self
                .spawn_worker(&worker_config, adhoc, persist_actor_session && !adhoc)
                .await?;
            let reporter_worker_id = if adhoc {
                format!("adhoc-{idx}")
            } else {
                worker_logical_ids
                    .get(&worker_id)
                    .cloned()
                    .unwrap_or_else(|| worker_id.to_string())
            };
            let score = reported_bids
                .iter()
                .find(|b| b.worker == reporter_worker_id)
                .map(|b| b.score)
                .unwrap_or(0.0);
            plan.push(ReportedSubtask {
                name: item.name.clone(),
                description: item.description.clone(),
                winner: reporter_worker_id.clone(),
                score,
                adhoc,
                bids: reported_bids,
            });
            assigned_workers[idx] = Some(worker_id.clone());
            assigned_reporter_ids[idx] = Some(reporter_worker_id.clone());
            assignments.push((idx, worker_id, reporter_worker_id));
        }

        if control.is_some_and(AgentRunControl::is_cancelled) {
            return Ok(self
                .finish_cancelled_run(
                    &goal,
                    &items,
                    &ordered_outputs,
                    total_usage,
                    persist_actor_session,
                )
                .await);
        }

        // Report the decomposition + auction outcome before the workers run, so
        // the dashboard can render the Layer-2 plan (goal → subtasks → winners).
        if let Some(reporter) = &self.reporter {
            reporter.plan(&workflow_id, &coord_id, &goal, &plan);
        }
        if control.is_some_and(AgentRunControl::is_cancelled) {
            return Ok(self
                .finish_cancelled_run(
                    &goal,
                    &items,
                    &ordered_outputs,
                    total_usage,
                    persist_actor_session,
                )
                .await);
        }

        // 3. Delegate the pending subtasks to workers IN PARALLEL.
        let mut join_set = tokio::task::JoinSet::new();
        let mut worker_terminal = vec![false; items.len()];
        let next_provider_group = Arc::new(std::sync::atomic::AtomicU64::new(1));
        for (idx, worker_id, reporter_worker_id) in assignments {
            if control.is_some_and(AgentRunControl::is_cancelled) {
                break;
            }
            if let Some(reporter) = &self.reporter {
                reporter.worker_started(&workflow_id, &reporter_worker_id);
            }
            let actor = self.active_workers.get(&worker_id).cloned();
            let desc = items[idx].description.clone();
            let name = items[idx].name.clone();
            let wid = worker_id.clone();
            let worker_source = reporter_worker_id.clone();
            let worker_control = control.cloned();
            let parent_sink = self.stream_sink.clone();
            let next_group = next_provider_group.clone();
            let run_seq = self.run_seq;
            let worker_history = request_context.history.clone();
            let worker_attachments = request_context.attachments.clone();
            let worker_mode = request_context.conversation_mode;
            join_set.spawn(async move {
                let worker_content = worker_task_content(&worker_history, &desc);
                let worker_input = match worker_mode {
                    ConversationMode::ActorSession => AgentInput::text(worker_content),
                    ConversationMode::SuppliedHistory => {
                        AgentInput::text(worker_content).with_supplied_history(Vec::new())
                    }
                    ConversationMode::Stateless => {
                        AgentInput::text(worker_content).with_stateless(true)
                    }
                }
                .with_attachments(worker_attachments);
                let result = if let Some(actor_ref) = actor {
                    if let Some(parent_sink) = parent_sink {
                        let (child_sink, receiver) = tokio::sync::mpsc::unbounded_channel();
                        let forwarder = tokio::spawn(forward_worker_tool_stream(
                            receiver,
                            parent_sink,
                            run_seq,
                            idx,
                            worker_source,
                            next_group,
                        ));
                        let outcome = match worker_control {
                            Some(control) => {
                                execute_agent_streaming_controlled_measured(
                                    &actor_ref,
                                    worker_input,
                                    child_sink,
                                    control,
                                )
                                .await
                            }
                            None => {
                                execute_agent_streaming_measured(
                                    &actor_ref,
                                    worker_input,
                                    child_sink,
                                )
                                .await
                            }
                        };
                        let _ = forwarder.await;
                        outcome
                    } else {
                        match worker_control {
                            Some(control) => {
                                execute_agent_controlled_measured(&actor_ref, worker_input, control)
                                    .await
                            }
                            None => execute_agent_measured(&actor_ref, worker_input).await,
                        }
                    }
                    .map_err(|mut error| {
                        error.message = format!("Worker {wid} failed: {}", error.message);
                        error
                    })
                } else {
                    Err(AgentExecutionFailure::new(
                        format!("Worker {wid} not found"),
                        axocoatl_core::MeasuredTokenUsage::known(TokenUsageStats::default()),
                    ))
                };
                (idx, name, wid, reporter_worker_id, result)
            });
        }

        // Record each outcome as it completes and checkpoint after every one, so
        // a crash never loses finished work.
        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok((
                    idx,
                    name,
                    worker_id,
                    reporter_worker_id,
                    Ok(MeasuredAgentRunOutcome {
                        outcome: AgentRunOutcome::Completed(mut output),
                        token_usage: measured_usage,
                    }),
                )) => {
                    self.adopt_worker_measurement(&mut output, &measured_usage, &mut total_usage);
                    ordered_outputs[idx] = Some(output.clone());
                    worker_terminal[idx] = true;
                    if let Some(reporter) = &self.reporter {
                        reporter.worker_done(
                            &workflow_id,
                            &reporter_worker_id,
                            &output.content,
                            output.token_usage.total() as u64,
                        );
                    }
                    items[idx].outcome = Some(OrchestrationOutcome::Succeeded {
                        content: output.content.clone(),
                        tool_calls: output.tool_calls.clone(),
                        token_usage: output.token_usage.clone(),
                    });
                    self.worker_results.push(WorkerResult {
                        worker_id,
                        task_name: name,
                        output: Ok(output),
                    });
                }
                Ok((
                    idx,
                    _name,
                    _worker_id,
                    reporter_worker_id,
                    Ok(MeasuredAgentRunOutcome {
                        outcome:
                            AgentRunOutcome::Cancelled {
                                mut partial_output, ..
                            },
                        token_usage: measured_usage,
                    }),
                )) => {
                    self.adopt_worker_measurement(
                        &mut partial_output,
                        &measured_usage,
                        &mut total_usage,
                    );
                    if let Some(reporter) = &self.reporter {
                        reporter.worker_cancelled(
                            &workflow_id,
                            &reporter_worker_id,
                            &partial_output.content,
                            partial_output.token_usage.total() as u64,
                        );
                    }
                    ordered_outputs[idx] = Some(partial_output);
                    worker_terminal[idx] = true;
                    // Leave the orchestration item pending. A cancelled worker
                    // did not complete its task and must be resumable.
                }
                Ok((idx, name, worker_id, reporter_worker_id, Err(e))) => {
                    total_usage.merge(&e.token_usage.usage);
                    self.merge_active_run_usage(&e.token_usage.usage);
                    if !e.token_usage.complete {
                        self.mark_active_run_usage_unknown();
                    }
                    tracing::warn!(worker = %worker_id, task = %name, error = %e, "Worker task failed");
                    if let Some(reporter) = &self.reporter {
                        reporter.worker_failed(&workflow_id, &reporter_worker_id, &e.message);
                    }
                    worker_terminal[idx] = true;
                    items[idx].outcome = Some(OrchestrationOutcome::Failed {
                        error: e.message.clone(),
                    });
                    self.worker_results.push(WorkerResult {
                        worker_id,
                        task_name: name,
                        output: Err(e.message),
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
            state.token_usage = total_usage.clone();
            state.token_usage_known = self.active_run_usage_known();
            state.coordinator_provider_usage = self.run_provider_usage.clone();
            if persist_actor_session {
                self.checkpoint_orchestration(&state).await;
            }
        }

        // A JoinError does not expose the tuple a panicked task would have
        // returned. Resolve any still-nonterminal assignments here so reporters
        // never leave them visibly running or mislabel them completed.
        for (index, terminal) in worker_terminal.iter_mut().enumerate() {
            if *terminal {
                continue;
            }
            let Some(worker_id) = assigned_workers[index].as_ref() else {
                continue;
            };
            let reporter_worker_id = assigned_reporter_ids[index]
                .as_deref()
                .unwrap_or(worker_id.0.as_str());
            if let Some(reporter) = &self.reporter {
                if control.is_some_and(AgentRunControl::is_cancelled) {
                    reporter.worker_cancelled(&workflow_id, reporter_worker_id, "", 0);
                } else {
                    reporter.worker_panicked(
                        &workflow_id,
                        reporter_worker_id,
                        "worker task panicked before returning an outcome",
                    );
                }
            }
            *terminal = true;
        }

        if control.is_some_and(AgentRunControl::is_cancelled) {
            return Ok(self
                .finish_cancelled_run(
                    &goal,
                    &items,
                    &ordered_outputs,
                    total_usage,
                    persist_actor_session,
                )
                .await);
        }

        // 4. Aggregate outcomes across ALL items (including any restored from a
        //    previous run). An item still pending here means its worker panicked.
        let succeeded: Vec<(String, String)> = items
            .iter()
            .filter_map(|it| match &it.outcome {
                Some(OrchestrationOutcome::Succeeded { content, .. }) => {
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

        let (request, protected_suffix_start) = self.build_provider_request(
            &request_context,
            "You are a helpful coordinator. Synthesize worker outcomes into the final answer.",
            synthesis_prompt,
        );
        let response = match provider_budget::chat(
            self.provider.as_ref(),
            self.counter.as_ref(),
            self.tracker.as_ref(),
            Some(&self.active_run_usage),
            request,
            protected_suffix_start,
            control,
        )
        .await?
        {
            ControlledChat::Response(response) => response,
            ControlledChat::Cancelled => {
                return Ok(self
                    .finish_cancelled_run(
                        &goal,
                        &items,
                        &ordered_outputs,
                        total_usage,
                        persist_actor_session,
                    )
                    .await);
            }
        };
        total_usage.merge(&response.usage);
        self.run_provider_usage.merge(&response.usage);

        if let Some(sink) = &self.stream_sink {
            let _ = sink.send(AgentStreamChunk::Text(response.content.clone()));
        }
        if persist_actor_session {
            let output_tokens = self.counter.count_text(&response.content);
            self.session
                .append(MessageRole::Assistant, &response.content, output_tokens);
        }

        // Mark the run complete so a later request for the same goal starts
        // fresh rather than resuming this finished run.
        state.items = items;
        state.token_usage = total_usage.clone();
        state.token_usage_known = self.active_run_usage_known();
        state.coordinator_provider_usage = self.run_provider_usage.clone();
        state.completed = true;
        if persist_actor_session {
            self.checkpoint_orchestration(&state).await;
        }

        Ok(CoordinatorRunOutcome::Completed(AgentOutput {
            content: response.content,
            tool_calls: ordered_worker_tool_calls(&ordered_outputs),
            token_usage: total_usage,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axocoatl_core::{AgentRole, ChatMessage};
    use axocoatl_llm::{
        ChatResponse, FinishReason, ProviderCapabilities, ProviderError, StreamEvent, ToolCall,
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

    #[test]
    fn completed_and_cancelled_worker_outputs_adopt_measured_subtotal_and_completeness() {
        for label in ["completed", "cancelled"] {
            let coordinator = CoordinatorBehavior::new(Arc::new(MockLlm), Arc::new(UnitCounter));
            let mut total = TokenUsageStats::default();
            let mut output = AgentOutput::text(label);
            let measured =
                axocoatl_core::MeasuredTokenUsage::lower_bound(TokenUsageStats::new(13, 8));

            coordinator.adopt_worker_measurement(&mut output, &measured, &mut total);

            assert_eq!(output.token_usage, measured.usage);
            assert_eq!(total, measured.usage);
            assert_eq!(coordinator.active_run_usage_snapshot(), measured.usage);
            assert!(!coordinator.active_run_usage_known());
        }
    }

    #[test]
    fn coordinator_history_omits_complete_native_tool_groups_atomically() {
        let mut openai_metadata = axocoatl_core::ProviderMetadata::new();
        openai_metadata.insert("provider".to_string(), "openai".to_string());
        let mut gemini_metadata = axocoatl_core::ProviderMetadata::new();
        gemini_metadata.insert("gemini.thought_signature".to_string(), "sig".to_string());
        let history = vec![
            ChatMessage::system("untrusted imported system"),
            ChatMessage::user("safe user"),
            ChatMessage::assistant_with_tool_calls(
                "model was about to call a tool",
                vec![ToolCall {
                    id: "call_non_numeric_9x".to_string(),
                    name: "openai_tool".to_string(),
                    arguments: serde_json::json!({"x": 1}),
                    provider_metadata: openai_metadata,
                }],
            ),
            ChatMessage::tool_result("openai result", "openai_tool", "call_non_numeric_9x"),
            ChatMessage::assistant_with_tool_calls(
                "gemini parallel group",
                vec![
                    ToolCall {
                        id: String::new(),
                        name: "same_name".to_string(),
                        arguments: serde_json::json!({"slot": "a"}),
                        provider_metadata: gemini_metadata.clone(),
                    },
                    ToolCall {
                        id: String::new(),
                        name: "same_name".to_string(),
                        arguments: serde_json::json!({"slot": "b"}),
                        provider_metadata: gemini_metadata,
                    },
                ],
            ),
            ChatMessage::tool_result("first", "same_name", ""),
            ChatMessage::tool_result("second", "same_name", ""),
            ChatMessage::assistant("safe final text"),
        ];

        let safe = sanitize_coordinator_history(&history).unwrap();
        assert_eq!(safe.len(), 2);
        assert_eq!(safe[0].role, MessageRole::User);
        assert_eq!(safe[0].text_content(), Some("safe user"));
        assert_eq!(safe[1].role, MessageRole::Assistant);
        assert_eq!(safe[1].text_content(), Some("safe final text"));
        assert!(safe.iter().all(|message| message.tool_calls.is_empty()));
    }

    #[test]
    fn coordinator_history_rejects_orphan_interrupted_and_duplicate_results() {
        let orphan = vec![ChatMessage::tool_result("oops", "tool", "id")];
        assert!(sanitize_coordinator_history(&orphan).is_err());

        let call = ToolCall {
            id: "id".to_string(),
            name: "tool".to_string(),
            arguments: serde_json::json!({}),
            provider_metadata: Default::default(),
        };
        let interrupted = vec![
            ChatMessage::assistant_with_tool_calls("", vec![call.clone()]),
            ChatMessage::assistant("unrelated"),
        ];
        assert!(sanitize_coordinator_history(&interrupted).is_err());

        let duplicate = vec![
            ChatMessage::assistant_with_tool_calls("", vec![call]),
            ChatMessage::tool_result("first", "tool", "id"),
            ChatMessage::tool_result("duplicate", "tool", "id"),
        ];
        assert!(sanitize_coordinator_history(&duplicate).is_err());
    }

    #[tokio::test]
    async fn worker_tool_forwarding_namespaces_duplicate_empty_ids_and_attributes_source() {
        let (parent, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let next_group = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let mut forwarders = Vec::new();
        let mut children = Vec::new();
        for (worker_index, worker_id) in [(0, "worker-a"), (1, "worker-b")] {
            let (child, receiver) = tokio::sync::mpsc::unbounded_channel();
            forwarders.push(tokio::spawn(forward_worker_tool_stream(
                receiver,
                parent.clone(),
                7,
                worker_index,
                worker_id.to_string(),
                next_group.clone(),
            )));
            children.push(child);
        }

        for child in &children {
            child
                .send(AgentStreamChunk::ToolCallStarted {
                    source_agent: None,
                    id: String::new(),
                    name: "same_tool".to_string(),
                    arguments: serde_json::json!({}),
                    provider_arguments: serde_json::json!({}),
                    provider_metadata: Default::default(),
                    assistant_content: None,
                    provider_response_group: 1,
                    provider_call_index: 0,
                    provider_call_count: 1,
                })
                .unwrap();
        }
        // Complete in the reverse worker order. Cross-worker duplicate native
        // ids/names must still correlate within the worker namespace.
        for child in children.iter().rev() {
            child
                .send(AgentStreamChunk::ToolCallResult {
                    source_agent: None,
                    id: String::new(),
                    name: "same_tool".to_string(),
                    result: serde_json::json!({"ok": true}),
                    is_error: false,
                })
                .unwrap();
        }
        drop(children);
        for forwarder in forwarders {
            forwarder.await.unwrap();
        }
        drop(parent);

        let chunks = std::iter::from_fn(|| observed.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(chunks.len(), 4);
        let mut started = HashMap::<String, (String, u64)>::new();
        let mut finished = HashMap::<String, String>::new();
        for chunk in chunks {
            match chunk {
                AgentStreamChunk::ToolCallStarted {
                    source_agent: Some(source),
                    id,
                    provider_response_group,
                    ..
                } => {
                    assert!(!id.contains("orphan"));
                    started.insert(source, (id, provider_response_group));
                }
                AgentStreamChunk::ToolCallResult {
                    source_agent: Some(source),
                    id,
                    ..
                } => {
                    assert!(!id.contains("orphan"));
                    finished.insert(source, id);
                }
                other => panic!("unexpected forwarded chunk: {other:?}"),
            }
        }
        assert_eq!(started.len(), 2);
        assert_eq!(finished.len(), 2);
        for source in ["worker-a", "worker-b"] {
            assert_eq!(started[source].0, finished[source]);
        }
        assert_ne!(started["worker-a"].1, started["worker-b"].1);
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

    struct UnitCounter;
    impl TokenCounter for UnitCounter {
        fn count_text(&self, text: &str) -> usize {
            usize::from(!text.is_empty())
        }
        fn count_messages(&self, messages: &[ChatMessage]) -> usize {
            messages.len()
        }
        fn count_tool_definition(&self, _: &serde_json::Value) -> usize {
            1
        }
    }

    #[derive(Default)]
    struct RecordingCoordinatorReporter {
        plans: std::sync::Mutex<Vec<Vec<ReportedSubtask>>>,
        started: std::sync::Mutex<Vec<String>>,
        completed: std::sync::Mutex<Vec<String>>,
        failed: std::sync::Mutex<Vec<String>>,
        cancelled: std::sync::Mutex<Vec<String>>,
        panicked: std::sync::Mutex<Vec<String>>,
    }

    impl CoordinatorReporter for RecordingCoordinatorReporter {
        fn plan(
            &self,
            _workflow: &str,
            _coordinator: &str,
            _goal: &str,
            subtasks: &[ReportedSubtask],
        ) {
            self.plans.lock().unwrap().push(subtasks.to_vec());
        }

        fn worker_started(&self, _workflow: &str, worker: &str) {
            self.started.lock().unwrap().push(worker.to_string());
        }

        fn worker_done(&self, _workflow: &str, worker: &str, _output: &str, _tokens: u64) {
            self.completed.lock().unwrap().push(worker.to_string());
        }

        fn worker_failed(&self, _workflow: &str, worker: &str, _error: &str) {
            self.failed.lock().unwrap().push(worker.to_string());
        }

        fn worker_cancelled(
            &self,
            _workflow: &str,
            worker: &str,
            _partial_output: &str,
            _tokens: u64,
        ) {
            self.cancelled.lock().unwrap().push(worker.to_string());
        }

        fn worker_panicked(&self, _workflow: &str, worker: &str, _error: &str) {
            self.panicked.lock().unwrap().push(worker.to_string());
        }
    }

    struct CoordinatorBudgetLlm {
        direct_calls: std::sync::atomic::AtomicUsize,
        stream_calls: std::sync::atomic::AtomicUsize,
        contents: std::sync::Mutex<std::collections::VecDeque<String>>,
        usages: std::sync::Mutex<std::collections::VecDeque<TokenUsageStats>>,
        direct_requests: std::sync::Mutex<Vec<ChatRequest>>,
        stream_requests: std::sync::Mutex<Vec<ChatRequest>>,
    }

    impl CoordinatorBudgetLlm {
        fn new(contents: Vec<&str>, usages: Vec<TokenUsageStats>) -> Self {
            Self {
                direct_calls: std::sync::atomic::AtomicUsize::new(0),
                stream_calls: std::sync::atomic::AtomicUsize::new(0),
                contents: std::sync::Mutex::new(contents.into_iter().map(String::from).collect()),
                usages: std::sync::Mutex::new(usages.into_iter().collect()),
                direct_requests: std::sync::Mutex::new(Vec::new()),
                stream_requests: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for CoordinatorBudgetLlm {
        fn provider_id(&self) -> &str {
            "coordinator-budget"
        }

        fn model_id(&self) -> &str {
            "coordinator-budget-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                max_context_tokens: 10_000,
                max_output_tokens: 1_000,
                ..Default::default()
            }
        }

        fn count_tokens(&self, _: &ChatRequest) -> usize {
            10
        }

        async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
            self.direct_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.direct_requests.lock().unwrap().push(request);
            let content = self
                .contents
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "synthesized".to_string());
            let usage = self.usages.lock().unwrap().pop_front().unwrap_or_default();
            Ok(ChatResponse {
                content,
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
                usage,
                model: self.model_id().to_string(),
                provider: self.provider_id().to_string(),
            })
        }

        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.stream_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.stream_requests.lock().unwrap().push(request);
            Ok(Box::pin(tokio_stream::iter(vec![
                Ok(StreamEvent::TextDelta {
                    delta: "worker output".to_string(),
                }),
                Ok(StreamEvent::Usage(TokenUsageStats::new(1, 1))),
                Ok(StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                }),
            ])))
        }
    }

    struct CoordinatorWindowCounter;

    impl TokenCounter for CoordinatorWindowCounter {
        fn count_text(&self, text: &str) -> usize {
            text.len().saturating_add(3) / 4
        }

        fn count_messages(&self, messages: &[ChatMessage]) -> usize {
            messages.iter().fold(0_usize, |tokens, message| {
                let content = match &message.content {
                    axocoatl_core::MessageContent::Text(text) => self.count_text(text),
                    axocoatl_core::MessageContent::Parts(parts) => {
                        parts.iter().fold(0_usize, |part_tokens, part| {
                            part_tokens.saturating_add(match part {
                                axocoatl_core::ContentPart::Text(text) => self.count_text(text),
                                axocoatl_core::ContentPart::Image { .. } => 1_024,
                            })
                        })
                    }
                };
                tokens.saturating_add(content).saturating_add(1)
            })
        }

        fn count_tool_definition(&self, definition: &serde_json::Value) -> usize {
            self.count_text(&definition.to_string())
        }
    }

    struct CoordinatorWindowLlm {
        max_context_tokens: usize,
        direct_calls: std::sync::atomic::AtomicUsize,
        stream_calls: std::sync::atomic::AtomicUsize,
        direct_contents: std::sync::Mutex<std::collections::VecDeque<String>>,
        worker_output: String,
        direct_requests: std::sync::Mutex<Vec<ChatRequest>>,
    }

    impl CoordinatorWindowLlm {
        fn new(
            max_context_tokens: usize,
            direct_contents: Vec<String>,
            worker_output: impl Into<String>,
        ) -> Self {
            Self {
                max_context_tokens,
                direct_calls: std::sync::atomic::AtomicUsize::new(0),
                stream_calls: std::sync::atomic::AtomicUsize::new(0),
                direct_contents: std::sync::Mutex::new(direct_contents.into_iter().collect()),
                worker_output: worker_output.into(),
                direct_requests: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for CoordinatorWindowLlm {
        fn provider_id(&self) -> &str {
            "coordinator-window"
        }

        fn model_id(&self) -> &str {
            "coordinator-window-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                max_context_tokens: self.max_context_tokens,
                max_output_tokens: 8,
                ..Default::default()
            }
        }

        fn count_tokens(&self, request: &ChatRequest) -> usize {
            CoordinatorWindowCounter.count_messages(&request.messages)
        }

        async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
            self.direct_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.direct_requests.lock().unwrap().push(request);
            Ok(ChatResponse {
                content: self
                    .direct_contents
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| "synthesized".to_string()),
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
                usage: TokenUsageStats::new(1, 1),
                model: self.model_id().to_string(),
                provider: self.provider_id().to_string(),
            })
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.stream_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::pin(tokio_stream::iter(vec![
                Ok(StreamEvent::TextDelta {
                    delta: self.worker_output.clone(),
                }),
                Ok(StreamEvent::Usage(TokenUsageStats::new(1, 1))),
                Ok(StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                }),
            ])))
        }
    }

    struct ToolLoopCoordinatorLlm {
        direct_calls: std::sync::atomic::AtomicUsize,
        stream_calls: std::sync::atomic::AtomicUsize,
    }

    struct GatedCoordinatorToolLlm {
        direct_calls: std::sync::atomic::AtomicUsize,
        stream_calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for GatedCoordinatorToolLlm {
        fn provider_id(&self) -> &str {
            "coordinator-cancel"
        }

        fn model_id(&self) -> &str {
            "coordinator-cancel-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }

        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
            self.direct_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ChatResponse {
                content: "fresh synthesis".to_string(),
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
                usage: TokenUsageStats::new(1, 1),
                model: self.model_id().to_string(),
                provider: self.provider_id().to_string(),
            })
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let call = self
                .stream_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events = if call == 0 {
                vec![
                    Ok(StreamEvent::ToolCallDelta {
                        index: Some(0),
                        id: "side-effect-1".to_string(),
                        name: Some("side_effect".to_string()),
                        args_delta: "{}".to_string(),
                    }),
                    Ok(StreamEvent::Usage(TokenUsageStats::new(1, 1))),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::ToolUse,
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta {
                        delta: "fresh worker".to_string(),
                    }),
                    Ok(StreamEvent::Usage(TokenUsageStats::new(1, 1))),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::Stop,
                    }),
                ]
            };
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    struct GatedCoordinatorTool {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        finished: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl axocoatl_tools::BuiltinTool for GatedCoordinatorTool {
        fn description(&self) -> &str {
            "gated coordinator cancellation test tool"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
        ) -> Result<serde_json::Value, axocoatl_tools::ToolError> {
            self.started.notify_one();
            self.release.notified().await;
            self.finished
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(serde_json::json!({"changed": true}))
        }
    }

    #[async_trait]
    impl LlmProvider for ToolLoopCoordinatorLlm {
        fn provider_id(&self) -> &str {
            "coordinator-tool-loop"
        }

        fn model_id(&self) -> &str {
            "coordinator-tool-loop-model"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                max_context_tokens: 10_000,
                max_output_tokens: 1_000,
                ..Default::default()
            }
        }

        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
            self.direct_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ChatResponse {
                content: "coordinator final".to_string(),
                tool_calls: Vec::new(),
                finish_reason: FinishReason::Stop,
                usage: TokenUsageStats::new(1, 1),
                model: self.model_id().to_string(),
                provider: self.provider_id().to_string(),
            })
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let call = self
                .stream_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events = if call == 0 {
                vec![
                    Ok(StreamEvent::ToolCallDelta {
                        index: Some(0),
                        id: "echo-1".to_string(),
                        name: Some("echo".to_string()),
                        args_delta: r#"{"text":"hello"}"#.to_string(),
                    }),
                    Ok(StreamEvent::Usage(TokenUsageStats::new(1, 1))),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::ToolUse,
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta {
                        delta: "worker final".to_string(),
                    }),
                    Ok(StreamEvent::Usage(TokenUsageStats::new(1, 1))),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::Stop,
                    }),
                ]
            };
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    fn coord_config_with_budget(
        per_call: usize,
        per_execution: usize,
        overflow_policy: OverflowPolicy,
    ) -> AgentConfig {
        AgentConfig {
            token_budget: Some(TokenBudget {
                per_call,
                per_execution,
                overflow_policy,
            }),
            ..coord_config()
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

    fn request_context() -> CoordinatorRequestContext {
        CoordinatorRequestContext {
            history: Vec::new(),
            system: None,
            model: None,
            attachments: Vec::new(),
            conversation_mode: ConversationMode::ActorSession,
        }
    }

    fn coordinator_window_config() -> AgentConfig {
        AgentConfig {
            sampling: SamplingConfig {
                max_tokens: Some(8),
                ..Default::default()
            },
            ..coord_config()
        }
    }

    #[tokio::test]
    async fn coordinator_projects_only_older_text_turns_before_exact_model_dispatch() {
        let provider = Arc::new(CoordinatorWindowLlm::new(
            300,
            vec![r#"[{"name":"work","description":"do it","tools":[]}]"#.to_string()],
            "worker",
        ));
        let mut coordinator =
            CoordinatorBehavior::new(provider.clone(), Arc::new(CoordinatorWindowCounter));
        coordinator
            .on_start(&coordinator_window_config())
            .await
            .unwrap();
        coordinator
            .session
            .append(MessageRole::User, "CANONICAL SESSION SENTINEL", 1);
        let canonical_before = serde_json::to_string(coordinator.session.messages()).unwrap();
        let history = vec![
            ChatMessage::user(format!("OLD USER {}", "u".repeat(2_000))),
            ChatMessage::assistant(format!("OLD ANSWER {}", "a".repeat(2_000))),
            ChatMessage::user("CURRENT USER TURN"),
        ];
        let history_before = serde_json::to_string(&history).unwrap();
        let context = CoordinatorRequestContext {
            history,
            ..request_context()
        };

        let (subtasks, _) = coordinator
            .decompose_task("CURRENT USER TURN", &context, None)
            .await
            .unwrap();

        assert_eq!(subtasks.len(), 1);
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let requests = provider.direct_requests.lock().unwrap();
        let rendered = format!("{:?}", requests[0].messages);
        assert!(!rendered.contains("OLD USER"));
        assert!(!rendered.contains("OLD ANSWER"));
        assert!(rendered.contains("CURRENT USER TURN"));
        drop(requests);
        assert_eq!(
            serde_json::to_string(&context.history).unwrap(),
            history_before
        );
        assert_eq!(
            serde_json::to_string(coordinator.session.messages()).unwrap(),
            canonical_before,
            "provider-side projection must never rewrite canonical Session history"
        );
    }

    #[tokio::test]
    async fn coordinator_rejects_oversized_protected_attachment_before_dispatch() {
        let provider = Arc::new(CoordinatorWindowLlm::new(300, Vec::new(), "worker"));
        let mut coordinator =
            CoordinatorBehavior::new(provider.clone(), Arc::new(CoordinatorWindowCounter));
        coordinator
            .on_start(&coordinator_window_config())
            .await
            .unwrap();
        let attachment = AgentAttachment {
            id: "attachment-1".to_string(),
            name: "large.txt".to_string(),
            mime: "text/plain".to_string(),
            bytes: Vec::new(),
            size: 8_000,
            extracted_text: Some("protected attachment ".repeat(400)),
        };
        let context = CoordinatorRequestContext {
            history: vec![ChatMessage::user("CURRENT ATTACHMENT TURN")],
            attachments: vec![attachment],
            ..request_context()
        };
        let history_before = serde_json::to_string(&context.history).unwrap();

        let error = coordinator
            .decompose_task("CURRENT ATTACHMENT TURN", &context, None)
            .await
            .unwrap_err();

        assert!(matches!(error, AgentError::ContextLimitExceeded { .. }));
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            serde_json::to_string(&context.history).unwrap(),
            history_before
        );
    }

    #[tokio::test]
    async fn htn_frontier_rejects_oversized_current_suffix_before_dispatch() {
        let provider = Arc::new(CoordinatorWindowLlm::new(300, Vec::new(), "worker"));
        let history = vec![ChatMessage::user(format!(
            "CURRENT FRONTIER TURN {}",
            "x".repeat(4_000)
        ))];
        let history_before = serde_json::to_string(&history).unwrap();
        let resolver =
            LlmFrontierResolver::new(provider.clone(), Arc::new(CoordinatorWindowCounter))
                .with_request_context(
                    history.clone(),
                    None,
                    Vec::new(),
                    SamplingConfig {
                        max_tokens: Some(8),
                        ..Default::default()
                    },
                );
        let task = HtnTask {
            name: "unresolved".to_string(),
            parameters: HashMap::new(),
            task_type: HtnTaskType::Compound,
        };

        let error =
            axocoatl_coordination::FrontierResolver::resolve(&resolver, &task, &HashMap::new())
                .await
                .unwrap_err();

        assert!(error.contains("context"));
        assert!(matches!(
            resolver.take_failure(),
            Some(AgentError::ContextLimitExceeded { .. })
        ));
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(serde_json::to_string(&history).unwrap(), history_before);
    }

    #[tokio::test]
    async fn coordinator_rejects_oversized_synthesis_before_second_direct_dispatch() {
        let provider = Arc::new(CoordinatorWindowLlm::new(
            300,
            vec![r#"[{"name":"work","description":"small subtask","tools":[]}]"#.to_string()],
            "oversized worker output ".repeat(400),
        ));
        let worker = WorkerConfig {
            id: AgentId::new("window-worker"),
            name: "Window Worker".to_string(),
            system_prompt: "Complete the assigned task.".to_string(),
            tools: Vec::new(),
            model: "coordinator-window-model".to_string(),
            provider: None,
            token_budget: None,
            sampling: SamplingConfig {
                max_tokens: Some(8),
                ..Default::default()
            },
            memory: MemoryConfig::default(),
            session_context: None,
            project_instructions_root: None,
        };
        let mut coordinator =
            CoordinatorBehavior::new(provider.clone(), Arc::new(CoordinatorWindowCounter))
                .add_worker_config(worker);
        coordinator
            .on_start(&coordinator_window_config())
            .await
            .unwrap();
        coordinator
            .session
            .append(MessageRole::User, "OLDER CANONICAL USER", 1);
        coordinator
            .session
            .append(MessageRole::Assistant, "OLDER CANONICAL ANSWER", 1);

        let error = coordinator
            .execute(AgentInput::text("CURRENT SYNTHESIS GOAL"))
            .await
            .unwrap_err();

        assert!(matches!(error, AgentError::ContextLimitExceeded { .. }));
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "decomposition may run, but oversized synthesis must fail locally"
        );
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let canonical = serde_json::to_string(coordinator.session.messages()).unwrap();
        for expected in [
            "OLDER CANONICAL USER",
            "OLDER CANONICAL ANSWER",
            "CURRENT SYNTHESIS GOAL",
        ] {
            assert!(
                canonical.contains(expected),
                "missing {expected}: {canonical}"
            );
        }
    }

    #[tokio::test]
    async fn stateless_coordinator_rejects_required_tools_before_worker_or_synthesis() {
        let provider = Arc::new(CoordinatorBudgetLlm::new(
            vec![r#"[{"name":"write","description":"write it","tools":["write_file"]}]"#],
            vec![TokenUsageStats::new(1, 1)],
        ));
        let mut executor = ToolExecutor::new();
        executor.register_builtin("write_file", Arc::new(axocoatl_tools::EchoTool));
        let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(UnitCounter))
            .with_tool_executor(Arc::new(executor))
            .add_worker_config(WorkerConfig {
                id: AgentId::new("stateless-tool-worker"),
                name: "Stateless Tool Worker".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec!["write_file".to_string()],
                model: "coordinator-budget-model".to_string(),
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
            });
        coordinator.on_start(&coord_config()).await.unwrap();

        let error = coordinator
            .execute(AgentInput::text("write a file").with_stateless(true))
            .await
            .unwrap_err();

        assert!(matches!(error, AgentError::ToolFailed { .. }));
        assert!(error.to_string().contains("stateless coordination"));
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "decomposition is the call that discovers the required tool"
        );
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no stateless worker may be dispatched for a tool-required subtask"
        );
    }

    #[tokio::test]
    async fn shared_core_blocks_are_filtered_per_declared_worker() {
        let temp = tempfile::tempdir().unwrap();
        let mut registry = axocoatl_memory::SharedBlockRegistry::new(temp.path().join("shared"));
        let mut block_x = axocoatl_memory::MemoryBlock::new("x", 100);
        block_x.shared = true;
        block_x.value = "secret-x".to_string();
        let mut block_y = axocoatl_memory::MemoryBlock::new("y", 100);
        block_y.shared = true;
        block_y.value = "secret-y".to_string();
        let shared_x = registry.ensure(block_x).await;
        let shared_y = registry.ensure(block_y).await;
        let coordinator =
            CoordinatorBehavior::new(Arc::new(MockLlm), Arc::new(UnitCounter)).with_shared_blocks(
                HashMap::from([("x".to_string(), shared_x), ("y".to_string(), shared_y)]),
            );
        let memory_for = |label: &str| MemoryConfig {
            core: axocoatl_core::CoreMemoryConfig {
                blocks: vec![axocoatl_core::CoreBlockConfig {
                    label: label.to_string(),
                    value: String::new(),
                    limit: 100,
                    shared: true,
                    description: None,
                }],
            },
            ..MemoryConfig::default()
        };

        let for_a = coordinator.shared_blocks_for_worker(&memory_for("x"));
        let for_b = coordinator.shared_blocks_for_worker(&memory_for("y"));
        assert_eq!(for_a.keys().cloned().collect::<Vec<_>>(), vec!["x"]);
        assert_eq!(for_b.keys().cloned().collect::<Vec<_>>(), vec!["y"]);
        assert!(!for_b.contains_key("x"), "worker B cannot render or edit X");
        assert!(!for_a.contains_key("y"), "worker A cannot render or edit Y");
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
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("w2"),
                name: "W2".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
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
    async fn coordinator_propagates_request_controls_and_streams_only_final_text() {
        let provider = Arc::new(CoordinatorBudgetLlm::new(
            vec![
                r#"[{"name":"only","description":"worker task","tools":[]}]"#,
                "final synthesis",
            ],
            vec![TokenUsageStats::new(2, 1), TokenUsageStats::new(3, 2)],
        ));
        let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(UnitCounter))
            .with_session_context("/workspace")
            .add_worker_config(WorkerConfig {
                id: AgentId::new("request-worker"),
                name: "Request worker".to_string(),
                system_prompt: "worker system".to_string(),
                tools: Vec::new(),
                model: "worker-model".to_string(),
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: Some("/workspace".to_string()),
                project_instructions_root: None,
            });
        let mut config = coord_config();
        config.sampling = SamplingConfig {
            temperature: Some(0.25),
            top_p: Some(0.75),
            max_tokens: Some(77),
            response_format: Some(axocoatl_core::ResponseFormat::Json),
        };
        coordinator.on_start(&config).await.unwrap();
        let (sink, mut chunks) = tokio::sync::mpsc::unbounded_channel();
        coordinator.set_stream_sink(Some(sink));
        let attachment = AgentAttachment {
            id: "att-1".to_string(),
            name: "notes.txt".to_string(),
            mime: "text/plain".to_string(),
            bytes: b"attachment body".to_vec(),
            size: 15,
            extracted_text: Some("ATTACHMENT_SENTINEL".to_string()),
        };
        let input = AgentInput::text("CURRENT GOAL")
            .with_supplied_history(vec![
                ChatMessage::user("EARLIER USER"),
                ChatMessage::assistant("EARLIER ANSWER"),
            ])
            .with_system_override(Some("TURN SYSTEM".to_string()))
            .with_model_override(Some("turn-model".to_string()))
            .with_attachments(vec![attachment]);

        let output = coordinator.execute(input).await.unwrap();
        assert_eq!(output.content, "final synthesis");
        let direct = provider.direct_requests.lock().unwrap();
        assert_eq!(direct.len(), 2, "decomposition and synthesis");
        for request in direct.iter() {
            assert_eq!(request.model_override.as_deref(), Some("turn-model"));
            assert_eq!(request.temperature, Some(0.25));
            assert_eq!(request.top_p, Some(0.75));
            assert_eq!(request.max_tokens, Some(77));
            assert_eq!(
                request.response_format,
                Some(axocoatl_core::ResponseFormat::Json)
            );
            let rendered = format!("{:?}", request.messages);
            assert!(rendered.contains("TURN SYSTEM"));
            assert!(rendered.contains("/workspace"));
            assert!(rendered.contains("EARLIER USER"));
            assert!(rendered.contains("EARLIER ANSWER"));
            assert!(rendered.contains("CURRENT GOAL"));
            assert!(rendered.contains("ATTACHMENT_SENTINEL"));
        }
        drop(direct);
        let worker_requests = provider.stream_requests.lock().unwrap();
        assert_eq!(worker_requests.len(), 1);
        assert_eq!(
            worker_requests[0].model_override.as_deref(),
            Some("worker-model")
        );
        let rendered_worker_request = format!("{:?}", worker_requests[0].messages);
        for expected in [
            "EARLIER USER",
            "EARLIER ANSWER",
            "CURRENT GOAL",
            "worker task",
            "ATTACHMENT_SENTINEL",
        ] {
            assert_eq!(
                rendered_worker_request.matches(expected).count(),
                1,
                "worker request must contain {expected:?} exactly once: {rendered_worker_request}"
            );
        }
        drop(worker_requests);

        let observed = std::iter::from_fn(|| chunks.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(observed.len(), 1, "worker text must remain isolated");
        assert!(matches!(
            &observed[0],
            AgentStreamChunk::Text(text) if text == "final synthesis"
        ));
    }

    #[tokio::test]
    async fn actor_session_worker_receives_parent_context_without_native_history_replay() {
        let provider = Arc::new(CoordinatorBudgetLlm::new(
            vec![
                r#"[{"name":"only","description":"ACTOR SUBTASK","tools":[]}]"#,
                "final synthesis",
            ],
            vec![TokenUsageStats::new(2, 1), TokenUsageStats::new(3, 2)],
        ));
        let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(UnitCounter))
            .add_worker_config(WorkerConfig {
                id: AgentId::new("actor-context-worker"),
                name: "Actor context worker".to_string(),
                system_prompt: "worker system".to_string(),
                tools: Vec::new(),
                model: "worker-model".to_string(),
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
            });
        coordinator.on_start(&coord_config()).await.unwrap();
        coordinator
            .session
            .append(MessageRole::User, "EARLIER ACTOR USER", 1);
        coordinator
            .session
            .append(MessageRole::Assistant, "EARLIER ACTOR ANSWER", 1);

        coordinator
            .execute(AgentInput::text("CURRENT ACTOR GOAL"))
            .await
            .unwrap();

        let requests = provider.stream_requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let rendered = format!("{:?}", requests[0].messages);
        for expected in [
            "EARLIER ACTOR USER",
            "EARLIER ACTOR ANSWER",
            "CURRENT ACTOR GOAL",
            "ACTOR SUBTASK",
        ] {
            assert_eq!(
                rendered.matches(expected).count(),
                1,
                "Worker request must contain {expected:?} exactly once: {rendered}"
            );
        }
        assert!(rendered.contains("Coordinator conversation context (read-only)"));
    }

    #[tokio::test]
    async fn supplied_and_stateless_coordination_leave_actor_and_worker_memory_unchanged() {
        let methods = r#"
- task_pattern: "route"
  preconditions: []
  subtasks:
    - name: "work"
      parameters: {}
      task_type: Primitive
"#;
        let temp = tempfile::tempdir().unwrap();
        let checkpoint_store = Arc::new(CheckpointStore::new(
            temp.path().join("checkpoints"),
            axocoatl_memory::CheckpointPolicy::Manual,
        ));
        let baseline = AgentCheckpoint {
            version: 7,
            agent_id: "lead".to_string(),
            checkpoint_time: 11,
            session_messages: Vec::new(),
            cumulative_token_usage: TokenUsageStats::new(2, 3),
            cumulative_token_usage_known: true,
            behavior_state: None,
        };
        checkpoint_store.save(&baseline).await.unwrap();
        let data_path = temp.path().join("data");
        let data_root = SecureDir::open_or_create_all(&data_path).unwrap();
        let provider = Arc::new(ToolLoopCoordinatorLlm {
            direct_calls: std::sync::atomic::AtomicUsize::new(0),
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(axocoatl_tools::EchoTool));
        let worker_id = AgentId::new("request-local-worker");
        let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(UnitCounter))
            .with_htn_methods(HtnPlanner::from_methods_yaml(methods).unwrap())
            .with_checkpoint_store(checkpoint_store.clone())
            .with_data_root(data_root)
            .with_tool_executor(Arc::new(executor))
            .add_worker_config(WorkerConfig {
                id: worker_id.clone(),
                name: "Worker".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec!["echo".to_string()],
                model: "worker-model".to_string(),
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
            });
        coordinator.on_start(&coord_config()).await.unwrap();
        coordinator
            .session
            .append(MessageRole::User, "actor-owned history", 1);
        let actor_session_before = serde_json::to_string(coordinator.session.messages()).unwrap();

        let supplied = coordinator
            .execute(
                AgentInput::text("route")
                    .with_supplied_history(vec![ChatMessage::user("request-only history")]),
            )
            .await
            .unwrap();
        assert!(supplied
            .tool_calls
            .iter()
            .any(|call| call.tool_name == "echo"));
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "SuppliedHistory workers retain the real multi-round tool loop"
        );
        assert_eq!(
            serde_json::to_string(coordinator.session.messages()).unwrap(),
            actor_session_before
        );
        let after_supplied = checkpoint_store
            .load_latest(&AgentId::new("lead"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_supplied.version, baseline.version + 1);
        assert_eq!(after_supplied.behavior_state, baseline.behavior_state);
        assert_eq!(
            serde_json::to_vec(&after_supplied.session_messages).unwrap(),
            serde_json::to_vec(coordinator.session.messages()).unwrap()
        );
        assert_eq!(after_supplied.cumulative_token_usage.input_tokens, 5);
        assert_eq!(after_supplied.cumulative_token_usage.output_tokens, 6);
        assert!(after_supplied.cumulative_token_usage_known);
        assert!(checkpoint_store
            .load_latest(&worker_id)
            .await
            .unwrap()
            .is_none());
        assert_eq!(std::fs::read_dir(&data_path).unwrap().count(), 0);

        coordinator
            .execute(AgentInput::text("route").with_stateless(true))
            .await
            .unwrap();
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            3,
            "Stateless is intentionally a single worker provider round"
        );
        assert_eq!(
            serde_json::to_string(coordinator.session.messages()).unwrap(),
            actor_session_before
        );
        let after_stateless = checkpoint_store
            .load_latest(&AgentId::new("lead"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_stateless.version, baseline.version + 2);
        assert_eq!(after_stateless.behavior_state, baseline.behavior_state);
        assert_eq!(
            serde_json::to_vec(&after_stateless.session_messages).unwrap(),
            serde_json::to_vec(coordinator.session.messages()).unwrap()
        );
        assert_eq!(after_stateless.cumulative_token_usage.input_tokens, 7);
        assert_eq!(after_stateless.cumulative_token_usage.output_tokens, 8);
        assert!(after_stateless.cumulative_token_usage_known);
        assert!(checkpoint_store
            .load_latest(&worker_id)
            .await
            .unwrap()
            .is_none());
        assert_eq!(std::fs::read_dir(&data_path).unwrap().count(), 0);

        let mut restored = CoordinatorBehavior::new(provider, Arc::new(UnitCounter))
            .with_checkpoint_store(checkpoint_store);
        restored.on_start(&coord_config()).await.unwrap();
        let restored_usage = restored.cumulative_token_usage_measurement();
        assert!(restored_usage.complete);
        assert_eq!(restored_usage.usage.input_tokens, 7);
        assert_eq!(restored_usage.usage.output_tokens, 8);
        assert_eq!(
            serde_json::to_string(restored.session.messages()).unwrap(),
            actor_session_before,
            "request-local coordinator calls must persist accounting without adopting their history"
        );
    }

    #[tokio::test]
    async fn request_local_provider_error_persists_unknown_usage_without_adopting_history() {
        let temp = tempfile::tempdir().unwrap();
        let checkpoint_store = Arc::new(CheckpointStore::new(
            temp.path(),
            axocoatl_memory::CheckpointPolicy::Manual,
        ));
        let mut canonical_session = SessionMemory::new();
        canonical_session.append(MessageRole::User, "canonical actor history", 3);
        let baseline = AgentCheckpoint {
            version: 4,
            agent_id: "lead".to_string(),
            checkpoint_time: 11,
            session_messages: canonical_session.messages().to_vec(),
            cumulative_token_usage: TokenUsageStats::new(2, 3),
            cumulative_token_usage_known: true,
            behavior_state: None,
        };
        checkpoint_store.save(&baseline).await.unwrap();

        let mut coordinator = CoordinatorBehavior::new(Arc::new(FailingLlm), Arc::new(UnitCounter))
            .with_checkpoint_store(checkpoint_store.clone());
        coordinator.on_start(&coord_config()).await.unwrap();

        let error = coordinator
            .execute(
                AgentInput::text("request-local stateless goal")
                    .with_history(vec![ChatMessage::user("request-only history")])
                    .with_stateless(true),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::Provider(_)));

        let after_error = checkpoint_store
            .load_latest(&AgentId::new("lead"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_error.version, baseline.version + 1);
        assert_eq!(
            serde_json::to_vec(&after_error.session_messages).unwrap(),
            serde_json::to_vec(&baseline.session_messages).unwrap()
        );
        assert_eq!(after_error.behavior_state, baseline.behavior_state);
        assert_eq!(
            after_error.cumulative_token_usage,
            baseline.cumulative_token_usage
        );
        assert!(
            !after_error.cumulative_token_usage_known,
            "a dispatched provider failure has unknown usage even when no terminal usage was returned"
        );

        let mut restored = CoordinatorBehavior::new(Arc::new(FailingLlm), Arc::new(UnitCounter))
            .with_checkpoint_store(checkpoint_store);
        restored.on_start(&coord_config()).await.unwrap();
        let restored_usage = restored.cumulative_token_usage_measurement();
        assert!(!restored_usage.complete);
        assert_eq!(restored_usage.usage, baseline.cumulative_token_usage);
        assert_eq!(
            serde_json::to_vec(restored.session.messages()).unwrap(),
            serde_json::to_vec(&baseline.session_messages).unwrap()
        );
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
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("h2"),
                name: "H2".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("h3"),
                name: "H3".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
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
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
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
        let mut executor = ToolExecutor::new();
        executor.register_builtin("special", Arc::new(axocoatl_tools::EchoTool));
        let mut coord = CoordinatorBehavior::new(provider, counter)
            .with_htn_methods(planner)
            .with_tool_executor(Arc::new(executor))
            .add_worker_config(WorkerConfig {
                id: AgentId::new("generalist"),
                name: "Generalist".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("specialist"),
                name: "Specialist".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec!["special".to_string()],
                model: "test-model".to_string(),
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
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
    async fn auction_rejects_unknown_required_tool_before_worker_dispatch() {
        let methods = r#"
- task_pattern: "route"
  preconditions: []
  subtasks:
    - name: "needs_typo"
      parameters:
        tools: ["definitely_not_registered"]
      task_type: Primitive
"#;
        let provider = Arc::new(CoordinatorBudgetLlm::new(Vec::new(), Vec::new()));
        let reporter = Arc::new(RecordingCoordinatorReporter::default());
        let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(UnitCounter))
            .with_htn_methods(HtnPlanner::from_methods_yaml(methods).unwrap())
            .with_reporter(reporter.clone())
            .add_worker_config(WorkerConfig {
                id: AgentId::new("misconfigured-worker"),
                name: "Misconfigured".to_string(),
                system_prompt: "worker".to_string(),
                // Configured-but-unregistered names are not real capabilities.
                tools: vec!["definitely_not_registered".to_string()],
                model: "test-model".to_string(),
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
            });
        coordinator.on_start(&coord_config()).await.unwrap();

        let error = coordinator
            .execute(AgentInput::text("route"))
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::ToolFailed { .. }));
        assert!(error.to_string().contains("definitely_not_registered"));
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "HTN decomposition is local and synthesis must not run"
        );
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no configured or ad-hoc worker may be dispatched"
        );
        assert!(coordinator.worker_results.is_empty());
        assert!(coordinator.active_workers.is_empty());
        assert!(
            reporter.plans.lock().unwrap().is_empty(),
            "an impossible assignment must not report a winner"
        );
    }

    #[tokio::test]
    async fn recall_required_worker_fails_closed_when_semantic_store_is_unopenable() {
        let methods = r#"
- task_pattern: "route"
  preconditions: []
  subtasks:
    - name: "needs_recall"
      parameters:
        tools: ["recall_search"]
      task_type: Primitive
"#;
        let temp = tempfile::tempdir().unwrap();
        let data_path = temp.path().join("data");
        std::fs::create_dir_all(data_path.join("memory")).unwrap();
        // A regular file where the secured semantic directory must exist makes
        // construction fail before any embedder/network work or worker spawn.
        std::fs::write(data_path.join("memory/semantic"), b"not a directory").unwrap();
        let data_root = SecureDir::open(&data_path).unwrap();
        let provider = Arc::new(CoordinatorBudgetLlm::new(Vec::new(), Vec::new()));
        let reporter = Arc::new(RecordingCoordinatorReporter::default());
        let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(UnitCounter))
            .with_htn_methods(HtnPlanner::from_methods_yaml(methods).unwrap())
            .with_data_root(data_root)
            .with_reporter(reporter.clone())
            .add_worker_config(WorkerConfig {
                id: AgentId::new("semanticworker"),
                name: "Semantic worker".to_string(),
                system_prompt: "worker".to_string(),
                tools: Vec::new(),
                model: "worker-model".to_string(),
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
            });
        coordinator.on_start(&coord_config()).await.unwrap();

        let error = coordinator
            .execute(AgentInput::text("route"))
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::InitFailed(_)));
        assert!(error.to_string().contains("semantic memory"));
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a worker missing its bid capability must never reach the provider"
        );
        assert!(reporter.plans.lock().unwrap().is_empty());
        assert!(coordinator.active_workers.is_empty());
    }

    #[tokio::test]
    async fn scoped_worker_reports_only_stable_logical_identity() {
        let methods = r#"
- task_pattern: "route"
  preconditions: []
  subtasks:
    - name: "work"
      parameters: {}
      task_type: Primitive
"#;
        let provider = Arc::new(CoordinatorBudgetLlm::new(vec!["final"], vec![]));
        let reporter = Arc::new(RecordingCoordinatorReporter::default());
        let scoped_id = AgentId::new("session-9:coordinator-1:worker:researcher");
        let config = WorkerConfig {
            id: scoped_id.clone(),
            name: "Researcher".to_string(),
            system_prompt: "worker".to_string(),
            tools: Vec::new(),
            model: "worker-model".to_string(),
            provider: None,
            token_budget: None,
            sampling: SamplingConfig::default(),
            memory: MemoryConfig::default(),
            session_context: None,
            project_instructions_root: None,
        };
        let mut coordinator = CoordinatorBehavior::new(provider, Arc::new(UnitCounter))
            .with_htn_methods(HtnPlanner::from_methods_yaml(methods).unwrap())
            .with_reporter(reporter.clone())
            .add_worker_config_with_logical_id(config, "researcher");
        coordinator.on_start(&coord_config()).await.unwrap();

        coordinator
            .execute(AgentInput::text("route"))
            .await
            .unwrap();

        let plans = reporter.plans.lock().unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0][0].winner, "researcher");
        assert_eq!(plans[0][0].bids[0].worker, "researcher");
        drop(plans);
        assert_eq!(&*reporter.started.lock().unwrap(), &["researcher"]);
        assert_eq!(&*reporter.completed.lock().unwrap(), &["researcher"]);
        assert!(reporter.failed.lock().unwrap().is_empty());
        assert!(reporter.cancelled.lock().unwrap().is_empty());
        assert!(reporter.panicked.lock().unwrap().is_empty());
        assert_eq!(coordinator.worker_results[0].worker_id, scoped_id);
    }

    #[tokio::test]
    async fn duplicate_logical_worker_identity_fails_before_any_dispatch() {
        let provider = Arc::new(CoordinatorBudgetLlm::new(Vec::new(), Vec::new()));
        let worker = |id: &str| WorkerConfig {
            id: AgentId::new(id),
            name: id.to_string(),
            system_prompt: "worker".to_string(),
            tools: Vec::new(),
            model: "worker-model".to_string(),
            provider: None,
            token_budget: None,
            sampling: SamplingConfig::default(),
            memory: MemoryConfig::default(),
            session_context: None,
            project_instructions_root: None,
        };
        let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(UnitCounter))
            .add_worker_config_with_logical_id(worker("scoped-a"), "writer")
            .add_worker_config_with_logical_id(worker("scoped-b"), "writer");
        coordinator.on_start(&coord_config()).await.unwrap();

        let error = coordinator
            .execute(AgentInput::text("must not decompose"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("duplicate logical worker id"));
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
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
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("rep_b"),
                name: "B".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
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
    async fn coordinator_cancellation_waits_for_started_tool_and_stops_followup() {
        use crate::run_control::AgentRunId;

        let methods = r#"
- task_pattern: "route"
  preconditions: []
  subtasks:
    - name: "change"
      parameters:
        tools: ["side_effect"]
      task_type: Primitive
"#;
        let temp = tempfile::tempdir().unwrap();
        let checkpoint_store = Arc::new(CheckpointStore::new(
            temp.path(),
            axocoatl_memory::CheckpointPolicy::Manual,
        ));
        let provider = Arc::new(GatedCoordinatorToolLlm {
            direct_calls: std::sync::atomic::AtomicUsize::new(0),
            stream_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut executor = ToolExecutor::new();
        executor.register_builtin(
            "side_effect",
            Arc::new(GatedCoordinatorTool {
                started: started.clone(),
                release: release.clone(),
                finished: finished.clone(),
            }),
        );
        let reporter = Arc::new(RecordingCoordinatorReporter::default());
        let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(UnitCounter))
            .with_htn_methods(HtnPlanner::from_methods_yaml(methods).unwrap())
            .with_checkpoint_store(checkpoint_store.clone())
            .with_tool_executor(Arc::new(executor))
            .with_reporter(reporter.clone())
            .add_worker_config_with_logical_id(
                WorkerConfig {
                    id: AgentId::new("session:lead:worker:writer"),
                    name: "Writer".to_string(),
                    system_prompt: "worker".to_string(),
                    tools: vec!["side_effect".to_string()],
                    model: "worker-model".to_string(),
                    provider: None,
                    token_budget: None,
                    sampling: SamplingConfig::default(),
                    memory: MemoryConfig::default(),
                    session_context: None,
                    project_instructions_root: None,
                },
                "writer",
            );
        coordinator.on_start(&coord_config()).await.unwrap();
        let (sink, mut chunks) = tokio::sync::mpsc::unbounded_channel();
        coordinator.set_stream_sink(Some(sink));
        let control = AgentRunControl::new(AgentRunId::new("cancel-tool"));

        let outcome = {
            let execution =
                coordinator.execute_controlled(AgentInput::text("route"), control.clone());
            tokio::pin!(execution);
            tokio::select! {
                _ = started.notified() => {}
                result = &mut execution => panic!("coordinator finished before tool start: {result:?}"),
            }
            control.cancel();
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(20), &mut execution,)
                    .await
                    .is_err(),
                "started tool future was dropped"
            );
            assert!(!finished.load(std::sync::atomic::Ordering::SeqCst));
            release.notify_one();
            tokio::time::timeout(std::time::Duration::from_secs(2), &mut execution)
                .await
                .expect("coordinator did not reach the tool safe boundary")
                .unwrap()
        };

        let partial = match outcome {
            AgentRunOutcome::Cancelled { partial_output, .. } => partial_output,
            AgentRunOutcome::Completed(_) => panic!("cancelled coordinator reported completion"),
        };
        assert!(finished.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(partial.tool_calls.len(), 1);
        assert_eq!(partial.tool_calls[0].tool_name, "side_effect");
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "cancellation must prevent the worker follow-up provider round"
        );
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "cancellation must prevent synthesis"
        );
        assert!(coordinator.active_workers.is_empty());
        assert!(coordinator.worker_handles.is_empty());
        assert!(reporter.completed.lock().unwrap().is_empty());
        assert_eq!(&*reporter.cancelled.lock().unwrap(), &["writer"]);

        let observed = std::iter::from_fn(|| chunks.try_recv().ok()).collect::<Vec<_>>();
        let starts = observed
            .iter()
            .filter(|chunk| matches!(chunk, AgentStreamChunk::ToolCallStarted { .. }))
            .count();
        let results = observed
            .iter()
            .filter(|chunk| matches!(chunk, AgentStreamChunk::ToolCallResult { .. }))
            .count();
        assert_eq!((starts, results), (1, 1));
        assert!(observed.iter().all(|chunk| match chunk {
            AgentStreamChunk::ToolCallStarted { source_agent, .. }
            | AgentStreamChunk::ToolCallResult { source_agent, .. } => {
                source_agent.as_deref() == Some("writer")
            }
            AgentStreamChunk::Text(_) | AgentStreamChunk::Reasoning(_) => true,
        }));
        let checkpoint = checkpoint_store
            .load_latest(&AgentId::new("lead"))
            .await
            .unwrap()
            .unwrap();
        assert!(checkpoint.behavior_state.is_none());

        let fresh = coordinator
            .execute_controlled(
                AgentInput::text("route"),
                AgentRunControl::new(AgentRunId::new("fresh-run")),
            )
            .await
            .unwrap();
        assert!(matches!(fresh, AgentRunOutcome::Completed(_)));
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a fresh control starts a new worker run"
        );
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
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
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(CheckpointStore::new(
            temp.path(),
            axocoatl_memory::CheckpointPolicy::Manual,
        ));
        let reporter = Arc::new(RecordingCoordinatorReporter::default());
        let mut coord = CoordinatorBehavior::new(provider, counter)
            .with_htn_methods(planner)
            .with_checkpoint_store(store.clone())
            .with_reporter(reporter.clone())
            .add_worker_config(WorkerConfig {
                id: AgentId::new("f1"),
                name: "F1".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("f2"),
                name: "F2".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
            });

        coord.on_start(&coord_config()).await.unwrap();
        let result = coord.execute(AgentInput::text("build something")).await;
        assert!(result.is_err(), "expected an error when all workers fail");
        let checkpoint = store
            .load_latest(&AgentId::new("lead"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            checkpoint.behavior_state.is_none(),
            "a terminal failed run must tombstone resumable behavior state"
        );
        assert!(coord.resumed_state.is_none());

        let second = coord.execute(AgentInput::text("build something")).await;
        assert!(second.is_err());
        assert_eq!(
            reporter.started.lock().unwrap().len(),
            4,
            "the next turn must decompose and dispatch fresh, not resume failed outcomes"
        );
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
                        tool_calls: vec![ToolCallRecord {
                            tool_name: "prior_tool".to_string(),
                            arguments: serde_json::json!({"from": "checkpoint"}),
                            result: Some(serde_json::json!({"ok": true})),
                        }],
                        token_usage: TokenUsageStats::new(4_000, 4_000),
                    }),
                },
                OrchestrationItem {
                    name: "pending_item".to_string(),
                    description: "still to do".to_string(),
                    required_tools: vec![],
                    outcome: None,
                },
            ],
            lifetime_usage_before_run: TokenUsageStats::default(),
            lifetime_usage_before_run_known: true,
            // Aggregate evidence includes a worker-heavy completed outcome.
            // It must not be recharged into the coordinator-only tracker.
            token_usage: TokenUsageStats::new(4_001, 4_001),
            token_usage_known: true,
            coordinator_provider_usage: TokenUsageStats::new(1, 1),
            completed: false,
        };
        let ckpt = AgentCheckpoint {
            version: 1,
            agent_id: "lead".to_string(),
            checkpoint_time: 0,
            session_messages: Vec::new(),
            cumulative_token_usage: TokenUsageStats::default(),
            cumulative_token_usage_known: true,
            behavior_state: Some(serde_json::to_string(&state).unwrap()),
        };
        store.save(&ckpt).await.unwrap();

        let provider = Arc::new(CoordinatorBudgetLlm::new(
            vec!["resume synthesis"],
            vec![TokenUsageStats::new(1, 1)],
        ));
        let counter: Arc<dyn TokenCounter> = Arc::new(SimpleCounter);
        let mut coord = CoordinatorBehavior::new(provider.clone(), counter)
            .with_checkpoint_store(store.clone())
            .add_worker_config(WorkerConfig {
                id: AgentId::new("rw1"),
                name: "RW1".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                model: "test-model".to_string(),
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
            });

        // on_start restores the incomplete run; execute resumes it.
        coord
            .on_start(&coord_config_with_budget(100, 100, OverflowPolicy::Abort))
            .await
            .unwrap();
        let output = coord
            .execute(AgentInput::text("resume goal"))
            .await
            .unwrap();

        // Only the pending item ran this turn — the already-done item was skipped.
        assert_eq!(coord.worker_results.len(), 1);
        assert_eq!(coord.worker_results[0].task_name, "pending_item");
        assert_eq!(output.content, "resume synthesis");
        assert!(output
            .tool_calls
            .iter()
            .any(|call| call.tool_name == "prior_tool"));
        assert!(output.token_usage.total() >= 8_004);
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "resume must skip decomposition and still leave budget for synthesis"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn coordinator_abort_budget_blocks_decomposition_before_provider_dispatch() {
        let provider = Arc::new(CoordinatorBudgetLlm::new(
            vec![r#"[{"name":"a","description":"A","tools":[]}]"#],
            vec![TokenUsageStats::new(1, 1)],
        ));
        let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(UnitCounter));
        coordinator
            .on_start(&coord_config_with_budget(10, 100, OverflowPolicy::Abort))
            .await
            .unwrap();

        let error = coordinator
            .execute(AgentInput::text("budgeted decomposition"))
            .await
            .unwrap_err();

        assert!(matches!(error, AgentError::TokenBudgetExceeded { .. }));
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[tokio::test]
    async fn coordinator_budget_tracker_resets_for_each_new_execution() {
        let decomposition = r#"[{"name":"only","description":"work","tools":[]}]"#;
        let provider = Arc::new(CoordinatorBudgetLlm::new(
            vec![decomposition, "first final", decomposition, "second final"],
            vec![
                TokenUsageStats::new(1, 1),
                TokenUsageStats::new(1, 1),
                TokenUsageStats::new(1, 1),
                TokenUsageStats::new(1, 1),
            ],
        ));
        let worker = WorkerConfig {
            id: AgentId::new("fresh-budget-worker"),
            name: "Worker".to_string(),
            system_prompt: "worker".to_string(),
            tools: Vec::new(),
            model: "worker-model".to_string(),
            provider: None,
            token_budget: None,
            sampling: SamplingConfig::default(),
            memory: MemoryConfig::default(),
            session_context: None,
            project_instructions_root: None,
        };
        let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(UnitCounter))
            .add_worker_config(worker);
        coordinator
            .on_start(&coord_config_with_budget(13, 13, OverflowPolicy::Abort))
            .await
            .unwrap();

        assert_eq!(
            coordinator
                .execute(AgentInput::text("first"))
                .await
                .unwrap()
                .content,
            "first final"
        );
        assert_eq!(
            coordinator
                .execute(AgentInput::text("second"))
                .await
                .unwrap()
                .content,
            "second final"
        );
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            4,
            "each execution receives a fresh coordinator tracker"
        );
    }

    #[tokio::test]
    async fn coordinator_cumulative_budget_blocks_synthesis_after_decomposition() {
        let provider = Arc::new(CoordinatorBudgetLlm::new(
            vec![r#"[{"name":"a","description":"A","tools":[]}]"#],
            vec![TokenUsageStats::new(10, 10)],
        ));
        let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(UnitCounter))
            .add_worker_config(WorkerConfig {
                id: AgentId::new("budget-worker"),
                name: "Budget worker".to_string(),
                system_prompt: "worker".to_string(),
                tools: Vec::new(),
                model: "worker-model".to_string(),
                provider: None,
                token_budget: None,
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
            });
        coordinator
            .on_start(&coord_config_with_budget(20, 30, OverflowPolicy::Abort))
            .await
            .unwrap();

        let error = coordinator
            .execute(AgentInput::text("budgeted synthesis"))
            .await
            .unwrap_err();

        assert!(matches!(error, AgentError::TokenBudgetExceeded { .. }));
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "synthesis must fail before a second direct provider call"
        );
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn coordinator_reported_reasoning_overrun_fails_before_workers() {
        let provider = Arc::new(CoordinatorBudgetLlm::new(
            vec![r#"[{"name":"a","description":"A","tools":[]}]"#],
            vec![TokenUsageStats::new(0, 0).with_reasoning(usize::MAX)],
        ));
        let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(UnitCounter));
        coordinator
            .on_start(&coord_config_with_budget(100, 100, OverflowPolicy::Abort))
            .await
            .unwrap();

        let error = coordinator
            .execute(AgentInput::text("hostile usage"))
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AgentError::TokenBudgetExceeded {
                used: usize::MAX,
                budget: 100
            }
        ));
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            coordinator.tracker.as_ref().unwrap().total_used(),
            usize::MAX
        );
    }

    #[tokio::test]
    async fn coordinator_missing_usage_is_estimated_and_consumes_shared_headroom() {
        let provider = Arc::new(CoordinatorBudgetLlm::new(
            vec![
                r#"[{"name":"a","description":"A","tools":[]}]"#,
                r#"[{"name":"b","description":"B","tools":[]}]"#,
            ],
            vec![TokenUsageStats::default(), TokenUsageStats::default()],
        ));
        let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(UnitCounter));
        coordinator
            .on_start(&coord_config_with_budget(11, 15, OverflowPolicy::Abort))
            .await
            .unwrap();

        let context = request_context();
        let (_, usage) = coordinator
            .decompose_task("first", &context, None)
            .await
            .unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 1);
        assert_eq!(usage.reasoning_tokens, None);
        assert_eq!(coordinator.tracker.as_ref().unwrap().total_used(), 11);
        let error = coordinator
            .decompose_task("second", &context, None)
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::TokenBudgetExceeded { .. }));
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn htn_frontiers_share_one_coordinator_budget_tracker() {
        let methods = r#"
- task_pattern: "root"
  preconditions: []
  subtasks:
    - name: "frontier_a"
      parameters: {}
      task_type: Compound
    - name: "frontier_b"
      parameters: {}
      task_type: Compound
"#;
        let provider = Arc::new(CoordinatorBudgetLlm::new(
            vec![
                r#"[{"name":"a","description":"A","tools":[]}]"#,
                r#"[{"name":"b","description":"B","tools":[]}]"#,
            ],
            vec![TokenUsageStats::new(10, 1), TokenUsageStats::new(10, 1)],
        ));
        let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(UnitCounter))
            .with_htn_methods(HtnPlanner::from_methods_yaml(methods).unwrap());
        coordinator
            .on_start(&coord_config_with_budget(11, 15, OverflowPolicy::Abort))
            .await
            .unwrap();

        let error = coordinator
            .decompose_task("root", &request_context(), None)
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::TokenBudgetExceeded { .. }));
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the second frontier must see usage recorded by the first"
        );
    }

    #[tokio::test]
    async fn coordinator_warn_policy_dispatches_and_saturates_hostile_usage() {
        let provider = Arc::new(CoordinatorBudgetLlm::new(
            vec![r#"[{"name":"a","description":"A","tools":[]}]"#],
            vec![TokenUsageStats::new(usize::MAX, 1).with_reasoning(usize::MAX)],
        ));
        let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(UnitCounter));
        coordinator
            .on_start(&coord_config_with_budget(1, 1, OverflowPolicy::Warn))
            .await
            .unwrap();

        let (_, usage) = coordinator
            .decompose_task("warn", &request_context(), None)
            .await
            .unwrap();
        assert_eq!(usage.total(), usize::MAX);
        assert_eq!(
            coordinator.tracker.as_ref().unwrap().total_used(),
            usize::MAX
        );
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn spawned_worker_preserves_full_abort_budget_before_dispatch() {
        let methods = r#"
- task_pattern: "root"
  preconditions: []
  subtasks:
    - name: "worker_task"
      parameters: {}
      task_type: Primitive
"#;
        let provider = Arc::new(CoordinatorBudgetLlm::new(vec!["synthesized"], vec![]));
        let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(UnitCounter))
            .with_htn_methods(HtnPlanner::from_methods_yaml(methods).unwrap())
            .add_worker_config(WorkerConfig {
                id: AgentId::new("guarded-worker"),
                name: "Guarded worker".to_string(),
                system_prompt: "worker".to_string(),
                tools: Vec::new(),
                model: "worker-model".to_string(),
                provider: None,
                token_budget: Some(TokenBudget {
                    per_call: 10,
                    per_execution: 100,
                    overflow_policy: OverflowPolicy::Abort,
                }),
                sampling: SamplingConfig::default(),
                memory: MemoryConfig::default(),
                session_context: None,
                project_instructions_root: None,
            });
        coordinator.on_start(&coord_config()).await.unwrap();

        let error = coordinator
            .execute(AgentInput::text("root"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("all 1 worker task"));
        assert_eq!(
            provider
                .stream_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the worker's per-call abort budget must reach DefaultAgentBehavior"
        );
        assert_eq!(
            provider
                .direct_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "failed workers leave nothing to synthesize"
        );
    }
}
