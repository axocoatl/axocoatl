//! Claude Code CLI: headless `claude -p --output-format stream-json`.
//! Owner: workstream `agents`.

use super::{ExternalActivationResult, ExternalAgentError};

/// The npm package and pinned version the recipe installs.
pub const CLAUDE_CODE_PACKAGE: &str = "@anthropic-ai/claude-code";
/// Secret name `axocoatl secret set` stores the `claude setup-token` output as.
pub const CLAUDE_CODE_SECRET: &str = "claude-code-oauth";

/// The argv that runs one activation.
pub fn argv(_model: &str) -> Result<Vec<String>, ExternalAgentError> {
    Err(ExternalAgentError::NotImplemented("claude_code::argv"))
}

/// Parse the program's stream-json output.
pub fn parse_output(_stdout: &[u8]) -> Result<ExternalActivationResult, ExternalAgentError> {
    Err(ExternalAgentError::NotImplemented(
        "claude_code::parse_output",
    ))
}
