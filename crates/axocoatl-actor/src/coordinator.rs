//! Coordinator behavior — an orchestrator agent that decomposes a goal into
//! subtasks, spawns worker agents to run them in parallel, and synthesizes the
//! results. Decomposition uses the LLM today; HTN planning and auction-based
//! worker assignment are layered in when configured.

use std::collections::HashMap;
use std::sync::Arc;

use axocoatl_coordination::{compute_bid, run_auction, AgentBid, HtnPlanner, HtnTask, HtnTaskType};
use axocoatl_core::{AgentConfig, AgentId, AgentInput, AgentOutput, TokenUsageStats};
use axocoatl_llm::{ChatRequest, LlmProvider};
use axocoatl_token::{TokenCounter, TokenTracker};
use axocoatl_tools::ToolExecutor;

use crate::actor_impl::{execute_agent, AgentActor, AgentMessage};
use crate::behavior::AgentBehavior;
use crate::default_behavior::DefaultAgentBehavior;
use crate::error::AgentError;

/// Token budget assumed per worker when scoring auction bids (workers don't
/// carry per-worker remaining budgets in this path yet).
const WORKER_TOKEN_BUDGET: usize = 10_000;

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
    /// Optional HTN planner. When set, decompose_task tries symbolic
    /// decomposition (no LLM call) before falling back to the LLM.
    htn_planner: Option<HtnPlanner>,
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
        // Try symbolic HTN decomposition first. If methods are loaded and the
        // task reduces to a fully-primitive plan, use it directly — no LLM call.
        if let Some(planner) = &self.htn_planner {
            let root = HtnTask {
                name: task.to_string(),
                parameters: HashMap::new(),
                task_type: HtnTaskType::Compound,
            };
            let plan = planner.plan(root);
            if !plan.primitives.is_empty() && plan.llm_frontiers.is_empty() {
                tracing::info!(
                    coordinator = %self.agent_id,
                    subtasks = plan.primitives.len(),
                    "Decomposed via HTN (no LLM call)"
                );
                return Ok(plan
                    .primitives
                    .into_iter()
                    .map(|t| {
                        let desc = t
                            .parameters
                            .get("description")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| t.name.clone());
                        (t.name, desc)
                    })
                    .collect());
            }
        }

        // Otherwise fall back to LLM decomposition.
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

        // 2. Assign each subtask to a worker by auction (best fit by tool match
        //    and budget), removing the winner from the pool so each subtask gets
        //    a distinct worker. When the pool is empty/exhausted, fall back to an
        //    ad-hoc worker so behavior never regresses.
        let mut assignments: Vec<(AgentId, String, String)> = Vec::new();
        let mut available = self.worker_configs.clone();

        for (i, (name, description)) in subtasks.iter().enumerate() {
            // Subtasks carry no tool requirements yet, so the auction ranks by
            // budget (load is 0 here since each worker is used at most once).
            let required_tools: Vec<String> = Vec::new();
            let worker_config = if available.is_empty() {
                WorkerConfig {
                    id: AgentId::new(format!("{}-worker-{}", self.agent_id, i)),
                    name: format!("Worker {}", i),
                    system_prompt: format!("You are a worker agent. Your task: {description}"),
                    tools: Vec::new(),
                }
            } else {
                let bids: Vec<AgentBid> = available
                    .iter()
                    .map(|wc| {
                        let ac = AgentConfig {
                            id: wc.id.clone(),
                            tools: wc.tools.clone(),
                            ..AgentConfig::default()
                        };
                        compute_bid(&ac, &required_tools, 0, WORKER_TOKEN_BUDGET)
                    })
                    .collect();
                let idx = run_auction(bids)
                    .and_then(|id| available.iter().position(|w| w.id == id))
                    .unwrap_or(0);
                available.remove(idx)
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
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("w2"),
                name: "W2".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
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
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("h2"),
                name: "H2".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
            })
            .add_worker_config(WorkerConfig {
                id: AgentId::new("h3"),
                name: "H3".to_string(),
                system_prompt: "worker".to_string(),
                tools: vec![],
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
}
