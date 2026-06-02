//! Coordinator behavior — orchestrator agent that spawns and manages worker agents.
//! Uses HTN planner for task decomposition and auction for assignment.

use std::collections::HashMap;
use std::sync::Arc;

use axocoatl_core::{AgentConfig, AgentId, AgentInput, AgentOutput, TokenUsageStats};
use axocoatl_llm::{ChatRequest, LlmProvider};
use axocoatl_token::{TokenCounter, TokenTracker};
use axocoatl_tools::ToolExecutor;

use crate::actor_impl::{execute_agent, AgentActor, AgentMessage};
use crate::behavior::AgentBehavior;
use crate::default_behavior::DefaultAgentBehavior;
use crate::error::AgentError;

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
        }
    }

    pub fn with_tool_executor(mut self, executor: Arc<ToolExecutor>) -> Self {
        self.tool_executor = Some(executor);
        self
    }

    /// Add a worker configuration. Workers with these configs can be spawned on demand.
    pub fn add_worker_config(mut self, config: WorkerConfig) -> Self {
        self.worker_configs.push(config);
        self
    }

    /// Spawn a worker agent and return its ID.
    async fn spawn_worker(&mut self, config: &WorkerConfig) -> Result<AgentId, AgentError> {
        let agent_config = AgentConfig {
            id: config.id.clone(),
            name: config.name.clone(),
            system_prompt: Some(config.system_prompt.clone()),
            ..AgentConfig::default()
        };

        let mut behavior = DefaultAgentBehavior::new(self.provider.clone(), self.counter.clone());

        if let Some(executor) = &self.tool_executor {
            behavior = behavior.with_tool_executor(executor.clone());
        }

        // Use spawn (not spawn_linked — ractor 0.15 doesn't expose spawn_linked on Actor trait directly)
        // Worker crashes surface as errors from execute_agent in the JoinSet below.
        let (actor_ref, handle) = ractor::Actor::spawn(
            Some(config.id.to_string()),
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

    /// Stop all active workers.
    async fn stop_all_workers(&mut self) {
        for (id, actor) in self.active_workers.drain() {
            actor.stop(None);
            tracing::debug!(worker = %id, "Stopped worker");
        }
    }

    /// Decompose a task into subtasks using the LLM.
    async fn decompose_task(&self, task: &str) -> Result<Vec<(String, String)>, AgentError> {
        let decompose_prompt = format!(
            "You are a task decomposition engine. Break the following task into 2-5 independent subtasks.\n\
            Return ONLY a JSON array of objects with 'name' and 'description' fields.\n\
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

        // Parse the JSON response
        let subtasks: Vec<serde_json::Value> = serde_json::from_str(&response.content)
            .unwrap_or_else(|_| {
                // Fallback: treat the whole task as a single subtask
                vec![serde_json::json!({
                    "name": "main_task",
                    "description": task
                })]
            });

        Ok(subtasks
            .into_iter()
            .map(|s| {
                (
                    s["name"].as_str().unwrap_or("task").to_string(),
                    s["description"].as_str().unwrap_or(task).to_string(),
                )
            })
            .collect())
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
        // 1. Decompose the task
        let subtasks = self.decompose_task(&input.content).await?;

        tracing::info!(
            coordinator = %self.agent_id,
            subtasks = subtasks.len(),
            "Decomposed task"
        );

        // 2. Spawn workers for each subtask (reuse existing configs or create ad-hoc ones)
        let mut assignments: Vec<(AgentId, String, String)> = Vec::new();

        for (i, (name, description)) in subtasks.iter().enumerate() {
            let worker_config = if i < self.worker_configs.len() {
                self.worker_configs[i].clone()
            } else {
                WorkerConfig {
                    id: AgentId::new(format!("{}-worker-{}", self.agent_id, i)),
                    name: format!("Worker {}", i),
                    system_prompt: format!("You are a worker agent. Your task: {description}"),
                    tools: Vec::new(),
                }
            };

            let worker_id = self.spawn_worker(&worker_config).await?;
            assignments.push((worker_id, name.clone(), description.clone()));
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

        let mut results = Vec::new();
        let mut total_usage = TokenUsageStats::default();

        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok((worker_id, task_name, Ok(output))) => {
                    total_usage.merge(&output.token_usage);
                    results.push(format!("## {task_name}\n{}", output.content));
                    self.worker_results.push(WorkerResult {
                        worker_id,
                        task_name,
                        output: Ok(output),
                    });
                }
                Ok((worker_id, task_name, Err(e))) => {
                    results.push(format!("## {task_name}\n[ERROR: {e}]"));
                    self.worker_results.push(WorkerResult {
                        worker_id,
                        task_name,
                        output: Err(e.to_string()),
                    });
                }
                Err(e) => {
                    tracing::error!(error = %e, "Worker task panicked");
                }
            }
        }

        // 4. Synthesize final response
        let synthesis_prompt = format!(
            "Synthesize the following worker results into a coherent response:\n\n{}",
            results.join("\n\n")
        );

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

        // 5. Clean up workers
        self.stop_all_workers().await;

        Ok(AgentOutput {
            content: response.content,
            tool_calls: Vec::new(),
            token_usage: total_usage,
        })
    }

    async fn on_stop(&mut self) -> Result<(), AgentError> {
        self.stop_all_workers().await;
        tracing::info!(coordinator = %self.agent_id, "Coordinator stopped");
        Ok(())
    }
}
