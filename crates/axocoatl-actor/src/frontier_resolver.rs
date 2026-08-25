//! LLM-backed resolver for HTN frontier tasks.
//!
//! When the symbolic [`HtnPlanner`](axocoatl_coordination::HtnPlanner) reaches a
//! compound task it has no method for, that task becomes a *frontier*. This
//! resolver decomposes that single task with the model — and only that task, not
//! the whole goal — into primitive subtasks the planner can then schedule. The
//! subtasks are emitted as [`HtnTaskType::Primitive`] so re-planning converges.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axocoatl_coordination::{FrontierResolver, HtnTask, HtnTaskType};
use axocoatl_core::{AgentAttachment, ChatMessage, SamplingConfig, TokenUsageStats};
use axocoatl_llm::{ChatRequest, LlmProvider};
use axocoatl_token::{TokenCounter, TokenTracker};

use crate::behavior::ExecutionUsageState;
use crate::default_behavior::attach_to_last_user_message;
use crate::error::AgentError;
use crate::provider_budget::{self, ControlledChat};
use crate::run_control::AgentRunControl;

/// Resolves HTN frontier tasks by asking the LLM to decompose one task.
pub struct LlmFrontierResolver {
    provider: Arc<dyn LlmProvider>,
    counter: Arc<dyn TokenCounter>,
    tracker: Option<TokenTracker>,
    control: Option<AgentRunControl>,
    model: Option<String>,
    history: Vec<ChatMessage>,
    system_context: Option<String>,
    attachments: Vec<AgentAttachment>,
    sampling: SamplingConfig,
    failure: Arc<std::sync::Mutex<Option<AgentError>>>,
    usage: ExecutionUsageState,
}

impl LlmFrontierResolver {
    pub fn new(provider: Arc<dyn LlmProvider>, counter: Arc<dyn TokenCounter>) -> Self {
        Self {
            provider,
            counter,
            tracker: None,
            control: None,
            model: None,
            history: Vec::new(),
            system_context: None,
            attachments: Vec::new(),
            sampling: SamplingConfig::default(),
            failure: Arc::new(std::sync::Mutex::new(None)),
            usage: ExecutionUsageState::default(),
        }
    }

    pub fn with_tracker(mut self, tracker: Option<TokenTracker>) -> Self {
        self.tracker = tracker;
        self
    }

    pub fn with_control(mut self, control: Option<AgentRunControl>) -> Self {
        self.control = control;
        self
    }

    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    pub fn with_request_context(
        mut self,
        history: Vec<ChatMessage>,
        system_context: Option<String>,
        attachments: Vec<AgentAttachment>,
        sampling: SamplingConfig,
    ) -> Self {
        self.history = history;
        self.system_context = system_context;
        self.attachments = attachments;
        self.sampling = sampling;
        self
    }

    pub fn take_failure(&self) -> Option<AgentError> {
        self.failure.lock().unwrap().take()
    }

    pub fn usage(&self) -> TokenUsageStats {
        self.usage.usage_snapshot()
    }

    pub fn usage_known(&self) -> bool {
        self.usage.snapshot().is_some()
    }
}

#[async_trait]
impl FrontierResolver for LlmFrontierResolver {
    async fn resolve(
        &self,
        task: &HtnTask,
        _state: &HashMap<String, serde_json::Value>,
    ) -> Result<Vec<HtnTask>, String> {
        let prompt = format!(
            "Decompose this single task into 2-5 concrete, independent primitive subtasks.\n\
             Return ONLY a JSON array; each element is an object with:\n\
             - \"name\": a short snake_case identifier\n\
             - \"description\": what the subtask does\n\
             - \"tools\": array of tool names it needs (use [] if none)\n\
             Do not include any other text.\n\n\
             Task: {}",
            task.name
        );
        let internal_system =
            "You decompose one task into primitive subtasks. Return only valid JSON.";
        let system = self.system_context.as_deref().map_or_else(
            || internal_system.to_string(),
            |context| format!("{context}\n\n{internal_system}"),
        );
        let mut messages = vec![ChatMessage::system(system)];
        let history_start = messages.len();
        messages.extend(self.history.iter().cloned());
        let final_prompt_index = messages.len();
        let protected_suffix_start = self
            .history
            .iter()
            .rposition(|message| message.role == axocoatl_core::MessageRole::User)
            .map_or(final_prompt_index, |index| {
                history_start.saturating_add(index)
            });
        messages.push(ChatMessage::user(prompt));
        let mut request = ChatRequest {
            messages,
            tools: Vec::new(),
            max_tokens: self.sampling.max_tokens,
            temperature: self.sampling.temperature,
            top_p: self.sampling.top_p,
            response_format: self.sampling.response_format,
            stop_sequences: Vec::new(),
            provider_options: None,
            model_override: self.model.clone(),
        };
        attach_to_last_user_message(&mut request, &self.attachments);

        let response = match provider_budget::chat(
            self.provider.as_ref(),
            self.counter.as_ref(),
            self.tracker.as_ref(),
            Some(&self.usage),
            request,
            protected_suffix_start,
            self.control.as_ref(),
        )
        .await
        {
            Ok(ControlledChat::Response(response)) => response,
            Ok(ControlledChat::Cancelled) => {
                return Err("frontier resolver cancelled".to_string());
            }
            Err(error) => {
                let message = format!("frontier resolver LLM call failed: {error}");
                *self.failure.lock().unwrap() = Some(error);
                return Err(message);
            }
        };
        let parsed: Vec<serde_json::Value> = serde_json::from_str(response.content.trim())
            .map_err(|e| format!("frontier resolver returned invalid JSON: {e}"))?;
        if parsed.is_empty() {
            return Err(format!(
                "frontier resolver returned no subtasks for '{}'",
                task.name
            ));
        }

        Ok(parsed
            .into_iter()
            .map(|s| {
                let name = s["name"].as_str().unwrap_or("subtask").to_string();
                let mut parameters = HashMap::new();
                if let Some(desc) = s["description"].as_str() {
                    parameters.insert("description".to_string(), serde_json::json!(desc));
                }
                if let Some(tools) = s.get("tools") {
                    parameters.insert("tools".to_string(), tools.clone());
                }
                // Always primitive so re-planning converges (the resolver does
                // not emit further compound tasks).
                HtnTask {
                    name,
                    parameters,
                    task_type: HtnTaskType::Primitive,
                }
            })
            .collect())
    }
}
