//! Validation for `web_search` (SearXNG) and `web_fetch`.
//!
//! Filled in by the web build. Until then the blocks parse with their bounds
//! documented in `types.rs` and are not checked further here.

use crate::egress::ConfigWarning;
use crate::error::ConfigError;
use crate::types::AxocoatlConfig;

/// Validate the web tool blocks. Returns warnings that do not stop the daemon.
pub fn validate_web(_config: &AxocoatlConfig) -> Result<Vec<ConfigWarning>, ConfigError> {
    Ok(Vec::new())
}
