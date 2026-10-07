//! `sandbox.egress.host_ollama`: an explicit, opt-in route from Session
//! containers to an Ollama server on this computer's loopback interface.
//!
//! Owner: workstream `runtime`. The skeleton refuses the setting until the
//! relay exists, so a configuration that names it never runs without it.

use crate::error::ConfigError;
use crate::types::AxocoatlConfig;

/// The host name containers use for the route (`OLLAMA_HOST`). It never
/// resolves in DNS; the decision point recognizes it by name.
pub const HOST_OLLAMA_ROUTE_HOST: &str = "ollama.host.axocoatl.internal";

/// Validate `sandbox.egress.host_ollama`. Absent is always valid.
pub fn validate_host_ollama(config: &AxocoatlConfig) -> Result<(), ConfigError> {
    let Some(egress) = &config.sandbox.egress else {
        return Ok(());
    };
    if egress.host_ollama.is_none() {
        return Ok(());
    }
    Err(ConfigError::InvalidField {
        field: "sandbox.egress.host_ollama".to_string(),
        value: "(set)".to_string(),
        reason: "not implemented in this build".to_string(),
        suggestion: "Remove sandbox.egress.host_ollama until the host Ollama route ships."
            .to_string(),
    })
}
