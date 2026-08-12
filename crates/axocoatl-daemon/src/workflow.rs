//! Compatibility output type for Automation execution APIs.

use axocoatl_core::{AgentOutput, TokenUsageStats};

/// Final compatibility result returned by an Automation run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkflowOutput {
    pub workflow_id: String,
    pub agent_outputs: Vec<(String, AgentOutput)>,
    /// Outputs from the nodes where execution terminated, in stable
    /// Automation declaration order and separated by a blank line when the
    /// graph has multiple runtime sinks. This is not limited to Agent nodes.
    pub final_content: String,
    pub total_token_usage: TokenUsageStats,
    pub completed_agents: Vec<String>,
    pub failed_agents: Vec<(String, String)>,
}
