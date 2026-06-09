//! Coordinator behavior — an orchestrator agent that decomposes a goal into
//! subtasks, assigns each to a worker agent, runs them in parallel, and
//! synthesizes the results. Decomposition prefers the symbolic HTN planner
//! (resolving any LLM frontiers task-by-task) and uses whole-goal LLM
//! decomposition only when no planner is configured; workers are chosen by a
//! capability/budget auction.

use std::collections::HashMap;
use std::sync::Arc;

use axocoatl_coordination::{compute_bid, run_auction, AgentBid, HtnPlanner, HtnTask, HtnTaskType};
use axocoatl_core::{AgentConfig, AgentId, AgentInput, AgentOutput, TokenUsageStats};
use axocoatl_llm::{ChatRequest, LlmProvider};
use axocoatl_memory::{CheckpointStore, LongTermMemory, SemanticMemory};
use axocoatl_token::{TokenCounter, TokenTracker};
use axocoatl_tools::{HookRegistry, ToolExecutor};

use crate::actor_impl::{execute_agent, AgentActor, AgentMessage};
use crate::behavior::AgentBehavior;
use crate::default_behavior::DefaultAgentBehavior;
use crate::error::AgentError;
use crate::frontier_resolver::LlmFrontierResolver;

/// Budget assumed for a worker that declares no explicit `token_budget` —
/// treated as ample so an unbounded worker isn't penalized in the auction.
pub const DEFAULT_WORKER_BUDGET: usize = 100_000;

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
    /// The worker's token budget, used as the budget signal in the auction.
    pub token_budget: usize,
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
    /// first-class agent (checkpointed, with long-term + semantic memory and the
    /// global hook registry), not a bare provider+tools shell.
    checkpoint_store: Option<Arc<CheckpointStore>>,
    long_term_memory: Option<Arc<tokio::sync::RwLock<LongTermMemory>>>,
    hook_registry: Option<Arc<HookRegistry>>,
    /// Data directory for per-worker semantic memory. When set (by the daemon),
    /// each worker gets a Tier-4 semantic store under it; when unset, workers
    /// run without semantic memory (and create no on-disk store).
    data_dir: Option<String>,
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
            worker_configs: Vec::new(),
            active_workers: HashMap::new(),
            worker_handles: Vec::new(),
            worker_results: Vec::new(),
            htn_planner: None,
            run_seq: 0,
            checkpoint_store: None,
            long_term_memory: None,
            hook_registry: None,
            data_dir: None,
        }
    }

    pub fn with_tool_executor(mut self, executor: Arc<ToolExecutor>) -> Self {
        self.tool_executor = Some(executor);
        self
    }

    pub fn with_checkpoint_store(mut self, store: Arc<CheckpointStore>) -> Self {
        self.checkpoint_store = Some(store);
        self
    }

    pub fn with_long_term_memory(
        mut self,
        memory: Arc<tokio::sync::RwLock<LongTermMemory>>,
    ) -> Self {
        self.long_term_memory = Some(memory);
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
            system_prompt: Some(config.system_prompt.clone()),
            tools: config.tools.clone(),
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
        if let Some(ltm) = &self.long_term_memory {
            behavior = behavior.with_long_term_memory(ltm.clone());
        }
        if let Some(hooks) = &self.hook_registry {
            behavior = behavior.with_hook_registry(hooks.clone());
        }
        // Tier-4 semantic memory, one store per worker (same scheme as a
        // standalone agent), under the coordinator's data dir. Built only when a
        // data dir is configured (the daemon sets it); omitted in lightweight or
        // embedded use so no disk store is created. Non-fatal on failure.
        if let Some(data_dir) = &self.data_dir {
            match SemanticMemory::new(
                &config.id.to_string(),
                format!("{data_dir}/memory/semantic"),
            ) {
                Ok(sem) => behavior = behavior.with_semantic_memory(Arc::new(sem)),
                Err(e) => {
                    tracing::warn!(worker = %config.id, error = %e, "semantic memory unavailable")
                }
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

        let parsed: Vec<serde_json::Value> = serde_json::from_str(response.content.trim())
            .map_err(|e| {
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

        // The original goal — passed to synthesis so the model answers the
        // actual request, not just a pile of worker outputs.
        let goal = input.content;

        // 1. Decompose the task
        let subtasks = self.decompose_task(&goal).await?;

        tracing::info!(
            coordinator = %self.agent_id,
            subtasks = subtasks.len(),
            "Decomposed task"
        );

        // 2. Assign each subtask to a worker by auction (best fit by tool match
        //    and budget), removing the winner from the pool so each subtask gets
        //    a distinct worker. When the pool is empty/exhausted, fall back to an
        //    ad-hoc worker so behavior never regresses.
        let mut assignments: Vec<(AgentId, String, String)> = Vec::new();
        let mut available = self.worker_configs.clone();
        let coord_id = self.agent_id.clone();

        for (i, subtask) in subtasks.iter().enumerate() {
            // The auction matches workers to the subtask's required tools; load
            // is 0 here since each pool worker is assigned at most once.
            let required_tools = &subtask.required_tools;
            // An ad-hoc worker granted exactly the subtask's required tools —
            // used when the pool is empty or no pooled worker can cover the
            // tools, so a subtask is never forced onto an unfit worker.
            let make_adhoc = || WorkerConfig {
                id: AgentId::new(format!("{coord_id}-worker-{i}")),
                name: format!("Worker {i}"),
                system_prompt: format!(
                    "You are a worker agent. Your task: {}",
                    subtask.description
                ),
                tools: required_tools.clone(),
                token_budget: DEFAULT_WORKER_BUDGET,
            };
            let worker_config = if available.is_empty() {
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
                match run_auction(bids).and_then(|id| available.iter().position(|w| w.id == id)) {
                    Some(idx) => available.remove(idx),
                    None => {
                        // No pooled worker has the required tools — spawn a
                        // capable ad-hoc worker rather than an unfit one.
                        tracing::warn!(
                            coordinator = %coord_id,
                            tools = ?required_tools,
                            "No worker bid for subtask; spawning an ad-hoc worker with the required tools"
                        );
                        make_adhoc()
                    }
                }
            };

            let worker_id = self.spawn_worker(&worker_config).await?;
            assignments.push((worker_id, subtask.name.clone(), subtask.description.clone()));
        }

        // 3. Delegate tasks to workers IN PARALLEL
        let mut join_set = tokio::task::JoinSet::new();
        for (worker_id, task_name, description) in assignments {
            let actor = self.active_workers.get(&worker_id).cloned();
            let desc = description.clone();
            let wid = worker_id.clone();
            let tname = task_name.clone();
            join_set.spawn(async move {
                let result = if let Some(actor_ref) = actor {
                    execute_agent(&actor_ref, AgentInput::text(desc))
                        .await
                        .map_err(|e| AgentError::Internal(format!("Worker {} failed: {e}", wid)))
                } else {
                    Err(AgentError::Internal(format!("Worker {} not found", wid)))
                };
                (wid, tname, result)
            });
        }

        let mut succeeded: Vec<(String, String)> = Vec::new();
        let mut failed: Vec<(String, String)> = Vec::new();
        let mut total_usage = TokenUsageStats::default();

        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok((worker_id, task_name, Ok(output))) => {
                    total_usage.merge(&output.token_usage);
                    succeeded.push((task_name.clone(), output.content.clone()));
                    self.worker_results.push(WorkerResult {
                        worker_id,
                        task_name,
                        output: Ok(output),
                    });
                }
                Ok((worker_id, task_name, Err(e))) => {
                    tracing::warn!(worker = %worker_id, task = %task_name, error = %e, "Worker task failed");
                    failed.push((task_name.clone(), e.to_string()));
                    self.worker_results.push(WorkerResult {
                        worker_id,
                        task_name,
                        output: Err(e.to_string()),
                    });
                }
                Err(e) => {
                    tracing::error!(error = %e, "Worker task panicked");
                    failed.push(("<panicked task>".to_string(), e.to_string()));
                }
            }
        }

        // If every worker failed there is nothing to synthesize — surface the
        // failure rather than asking the model to make something out of nothing.
        if succeeded.is_empty() {
            return Err(AgentError::Internal(format!(
                "all {} worker task(s) failed; nothing to synthesize",
                failed.len()
            )));
        }

        // 4. Synthesize: give the model the original goal and a structured view
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
                token_budget: DEFAULT_WORKER_BUDGET,
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("w2"),
                name: "W2".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                token_budget: DEFAULT_WORKER_BUDGET,
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
                token_budget: DEFAULT_WORKER_BUDGET,
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("h2"),
                name: "H2".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                token_budget: DEFAULT_WORKER_BUDGET,
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("h3"),
                name: "H3".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                token_budget: DEFAULT_WORKER_BUDGET,
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
                token_budget: DEFAULT_WORKER_BUDGET,
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
                token_budget: DEFAULT_WORKER_BUDGET,
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("specialist"),
                name: "Specialist".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec!["special".to_string()],
                token_budget: DEFAULT_WORKER_BUDGET,
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
                token_budget: DEFAULT_WORKER_BUDGET,
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("rep_b"),
                name: "B".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                token_budget: DEFAULT_WORKER_BUDGET,
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
                token_budget: DEFAULT_WORKER_BUDGET,
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("f2"),
                name: "F2".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
                token_budget: DEFAULT_WORKER_BUDGET,
            });

        coord.on_start(&coord_config()).await.unwrap();
        let result = coord.execute(AgentInput::text("build something")).await;
        assert!(result.is_err(), "expected an error when all workers fail");
    }
}
