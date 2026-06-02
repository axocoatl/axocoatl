//! Workflow execution tracking and output types.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Notify;

use axocoatl_core::{AgentId, AgentOutput, TokenUsageStats};

/// Final result of a workflow execution.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkflowOutput {
    pub workflow_id: String,
    pub agent_outputs: Vec<(String, AgentOutput)>,
    pub final_content: String,
    pub total_token_usage: TokenUsageStats,
    pub completed_agents: Vec<String>,
    pub failed_agents: Vec<(String, String)>,
}

/// Runtime tracker for an in-flight workflow.
///
/// Tracks which agents have completed, stores their outputs, and signals
/// when the entire workflow is done. Thread-safe for concurrent agent execution.
pub struct WorkflowExecution {
    pub workflow_id: String,
    pub expected_agents: HashSet<AgentId>,
    pub results: DashMap<AgentId, Result<AgentOutput, String>>,
    pub workflow_input: String,
    pub done: Notify,
    pub max_activations: usize,
    pub activation_count: AtomicUsize,
    /// Agent dependency map: agent_id -> list of agents it depends on.
    pub depends_on: DashMap<AgentId, Vec<AgentId>>,
}

impl WorkflowExecution {
    /// Create a new workflow execution tracker.
    pub fn new(
        workflow_id: String,
        expected_agents: HashSet<AgentId>,
        workflow_input: String,
        depends_on: DashMap<AgentId, Vec<AgentId>>,
    ) -> Self {
        let max = expected_agents.len() * 3;
        Self {
            workflow_id,
            expected_agents,
            results: DashMap::new(),
            workflow_input,
            done: Notify::new(),
            max_activations: max,
            activation_count: AtomicUsize::new(0),
            depends_on,
        }
    }

    /// Record an agent's result and signal completion if all agents are done.
    pub fn record_result(&self, agent_id: AgentId, result: Result<AgentOutput, String>) {
        self.results.insert(agent_id, result);
        if self.is_complete() {
            self.done.notify_one();
        }
    }

    /// Check if all expected agents have produced a result (success or failure).
    pub fn is_complete(&self) -> bool {
        self.results.len() >= self.expected_agents.len()
    }

    /// Increment the activation counter. Returns false if the cycle guard is exceeded.
    pub fn check_cycle_guard(&self) -> bool {
        let count = self.activation_count.fetch_add(1, Ordering::Relaxed);
        count < self.max_activations
    }

    /// Build the input text for an agent based on its upstream dependencies.
    ///
    /// For entry agents (no depends_on), returns the original workflow input.
    /// For downstream agents, formats upstream outputs into the prompt.
    pub fn build_agent_input(&self, agent_id: &AgentId) -> String {
        let deps = self
            .depends_on
            .get(agent_id)
            .map(|v| v.clone())
            .unwrap_or_default();

        if deps.is_empty() {
            return self.workflow_input.clone();
        }

        let mut parts = Vec::new();
        for dep_id in &deps {
            if let Some(result) = self.results.get(dep_id) {
                match result.value() {
                    Ok(output) => {
                        parts.push(format!("[{}]: {}", dep_id, output.content));
                    }
                    Err(e) => {
                        parts.push(format!("[{}]: (failed: {})", dep_id, e));
                    }
                }
            }
        }

        if parts.is_empty() {
            self.workflow_input.clone()
        } else {
            format!(
                "Previous agent outputs:\n{}\n\nOriginal task: {}",
                parts.join("\n"),
                self.workflow_input
            )
        }
    }

    /// Collect results into a WorkflowOutput.
    pub fn into_output(self: Arc<Self>) -> WorkflowOutput {
        let mut agent_outputs = Vec::new();
        let mut completed = Vec::new();
        let mut failed = Vec::new();
        let mut total_usage = TokenUsageStats::new(0, 0);
        let mut final_content = String::new();

        // Iterate in expected order
        for agent_id in &self.expected_agents {
            if let Some(result) = self.results.get(agent_id) {
                match result.value() {
                    Ok(output) => {
                        total_usage.merge(&output.token_usage);
                        final_content = output.content.clone();
                        completed.push(agent_id.to_string());
                        agent_outputs.push((agent_id.to_string(), output.clone()));
                    }
                    Err(e) => {
                        failed.push((agent_id.to_string(), e.clone()));
                    }
                }
            }
        }

        WorkflowOutput {
            workflow_id: self.workflow_id.clone(),
            agent_outputs,
            final_content,
            total_token_usage: total_usage,
            completed_agents: completed,
            failed_agents: failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_output(content: &str) -> AgentOutput {
        AgentOutput {
            content: content.to_string(),
            tool_calls: vec![],
            token_usage: TokenUsageStats::new(0, 0),
        }
    }

    #[test]
    fn workflow_execution_completes_when_all_agents_done() {
        let mut expected = HashSet::new();
        expected.insert(AgentId::new("a"));
        expected.insert(AgentId::new("b"));

        let exec = WorkflowExecution::new(
            "test".to_string(),
            expected,
            "input".to_string(),
            DashMap::new(),
        );

        assert!(!exec.is_complete());
        exec.record_result(AgentId::new("a"), Ok(make_output("result a")));
        assert!(!exec.is_complete());
        exec.record_result(AgentId::new("b"), Ok(make_output("result b")));
        assert!(exec.is_complete());
    }

    #[test]
    fn build_agent_input_for_entry_agent() {
        let exec = WorkflowExecution::new(
            "test".to_string(),
            HashSet::new(),
            "What is rust?".to_string(),
            DashMap::new(),
        );

        let input = exec.build_agent_input(&AgentId::new("researcher"));
        assert_eq!(input, "What is rust?");
    }

    #[test]
    fn build_agent_input_for_downstream_agent() {
        let mut expected = HashSet::new();
        expected.insert(AgentId::new("researcher"));
        expected.insert(AgentId::new("summarizer"));

        let deps = DashMap::new();
        deps.insert(AgentId::new("summarizer"), vec![AgentId::new("researcher")]);

        let exec = WorkflowExecution::new(
            "test".to_string(),
            expected,
            "What is rust?".to_string(),
            deps,
        );

        exec.record_result(
            AgentId::new("researcher"),
            Ok(make_output("Rust is a systems programming language.")),
        );

        let input = exec.build_agent_input(&AgentId::new("summarizer"));
        assert!(input.contains("[researcher]: Rust is a systems programming language."));
        assert!(input.contains("Original task: What is rust?"));
    }

    #[test]
    fn cycle_guard_prevents_runaway() {
        let mut expected = HashSet::new();
        expected.insert(AgentId::new("a"));
        let exec = WorkflowExecution::new(
            "test".to_string(),
            expected,
            "input".to_string(),
            DashMap::new(),
        );

        // max_activations = 1 * 3 = 3
        assert!(exec.check_cycle_guard()); // 0 < 3
        assert!(exec.check_cycle_guard()); // 1 < 3
        assert!(exec.check_cycle_guard()); // 2 < 3
        assert!(!exec.check_cycle_guard()); // 3 >= 3
    }
}
