//! Codex CLI: headless `codex exec --json`. Owner: workstream `agents`.

use super::{ExternalActivationResult, ExternalAgentError};

/// The npm package the recipe installs.
pub const CODEX_PACKAGE: &str = "@openai/codex";
/// Secret name for the OpenAI API key.
pub const CODEX_SECRET: &str = "codex-openai";

/// The argv that runs one activation.
pub fn argv(_model: &str) -> Result<Vec<String>, ExternalAgentError> {
    Err(ExternalAgentError::NotImplemented("codex::argv"))
}

/// Parse the program's JSON Lines output.
pub fn parse_output(_stdout: &[u8]) -> Result<ExternalActivationResult, ExternalAgentError> {
    Err(ExternalAgentError::NotImplemented("codex::parse_output"))
}
