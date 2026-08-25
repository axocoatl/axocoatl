//! Compatibility output type for Automation execution APIs.

use axocoatl_core::{AgentOutput, TokenUsageStats};

use crate::error::DaemonError;

/// One provider-backed Agent activation inside an Automation. `activation_id`
/// is stable within the run, so repeated Map iterations and nested Subgraphs
/// remain distinguishable even when they use the same configured Agent.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentActivationOutput {
    pub activation_id: String,
    pub agent_id: String,
    pub output: AgentOutput,
}

/// Final compatibility result returned by an Automation run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkflowOutput {
    pub workflow_id: String,
    pub agent_outputs: Vec<(String, AgentOutput)>,
    /// Identity-preserving form of `agent_outputs` for Map and Subgraph runs.
    pub agent_activations: Vec<AgentActivationOutput>,
    /// Outputs from the nodes where execution terminated, in stable
    /// Automation declaration order and separated by a blank line when the
    /// graph has multiple runtime sinks. This is not limited to Agent nodes.
    pub final_content: String,
    pub total_token_usage: TokenUsageStats,
    /// False means `total_token_usage` is a known subtotal because at least one
    /// dispatched provider call ended without usable accounting.
    pub token_usage_known: bool,
    pub completed_agents: Vec<String>,
    pub failed_agents: Vec<(String, String)>,
}

impl WorkflowOutput {
    /// Project handled step failures into the same measured terminal error
    /// used for structural Automation failures. The executor keeps returning
    /// the full output so run history can persist partial evidence; outward
    /// adapters call this method before declaring the run completed.
    pub fn terminal_error(&self) -> Option<DaemonError> {
        if self.failed_agents.is_empty() {
            return None;
        }
        let detail = self
            .failed_agents
            .iter()
            .map(|(subject, error)| format!("{subject}: {error}"))
            .collect::<Vec<_>>()
            .join("; ");
        Some(DaemonError::workflow_execution_measured(
            format!(
                "automation '{}' finished with {} failed step(s): {detail}",
                self.workflow_id,
                self.failed_agents.len()
            ),
            self.total_token_usage.clone(),
            self.token_usage_known,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handled_failures_project_as_measured_terminal_errors() {
        let output = WorkflowOutput {
            workflow_id: "review".into(),
            agent_outputs: Vec::new(),
            agent_activations: Vec::new(),
            final_content: "partial".into(),
            total_token_usage: TokenUsageStats {
                input_tokens: 13,
                output_tokens: 8,
                reasoning_tokens: Some(3),
            },
            token_usage_known: false,
            completed_agents: vec!["reader".into()],
            failed_agents: vec![("writer".into(), "provider timeout".into())],
        };

        let error = output.terminal_error().expect("the run failed");
        let (usage, known) = error
            .workflow_token_usage()
            .expect("failure retains measured usage");
        assert_eq!(usage.input_tokens, 13);
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.reasoning_tokens, Some(3));
        assert!(!known);
        assert!(error.to_string().contains("writer: provider timeout"));
    }

    #[test]
    fn successful_output_has_no_terminal_error() {
        let output = WorkflowOutput {
            workflow_id: "review".into(),
            agent_outputs: Vec::new(),
            agent_activations: Vec::new(),
            final_content: "done".into(),
            total_token_usage: TokenUsageStats::default(),
            token_usage_known: true,
            completed_agents: Vec::new(),
            failed_agents: Vec::new(),
        };
        assert!(output.terminal_error().is_none());
    }
}
