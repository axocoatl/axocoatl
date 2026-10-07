//! External coding agents (Claude Code CLI, Codex CLI) run as programs inside
//! a locked-down Session: network egress, `--harden`, the non-root writer
//! user. Model traffic goes through routes whose credential the daemon adds
//! from the secret store (the container holds a placeholder); the Session's
//! certificate authority is trusted through `NODE_EXTRA_CA_CERTS` /
//! `SSL_CERT_FILE`; every model call lands in the network record; the
//! program's JSON output is parsed into the Session record.
//!
//! Starts from the adapter removed in b922287 (`git show b922287`).
//! Owner: workstream `agents`.

pub mod claude_code;
pub mod codex;
pub mod recipe_images;

use axocoatl_config::loadout::AgentRuntime;
use axocoatl_config::EgressRouteYaml;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ExternalAgentError {
    #[error("external agent: not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("external agent: {0}")]
    Invalid(String),
}

/// One external activation to run inside the Session container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalActivationRequest {
    pub session_id: String,
    pub turn_id: String,
    pub node_id: String,
    pub runtime: AgentRuntime,
    pub model: String,
    /// The activation's full input (system instructions and request).
    pub prompt: String,
    /// Paths the program may change (checked by the activation's captures).
    pub writes: Option<Vec<String>>,
    pub timeout_ms: u64,
}

/// One item of the program's output, as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExternalItem {
    AssistantText {
        text: String,
    },
    ToolCall {
        name: String,
        arguments: String,
    },
    ToolResult {
        name: String,
        output: String,
        is_error: bool,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cost_microunits: Option<u64>,
    },
    Error {
        message: String,
    },
}

/// What an external activation produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalActivationResult {
    pub final_answer: Option<String>,
    pub items: Vec<ExternalItem>,
    pub exit_code: Option<i32>,
    /// The program's own usage report; complete only when it reported one.
    pub usage_complete: bool,
}

/// The egress routes (with credential names) a runtime needs.
pub fn routes_for(_runtime: AgentRuntime) -> Result<Vec<EgressRouteYaml>, ExternalAgentError> {
    Err(ExternalAgentError::NotImplemented(
        "external_agent::routes_for",
    ))
}
