//! Validation for the `browser` block.
//!
//! Filled in by the browser build. The network cross-field rules for
//! `browser` (no E2B, no `allow` under `network: none`) are already enforced
//! by [`crate::egress::validate_egress`].

use crate::egress::ConfigWarning;
use crate::error::ConfigError;
use crate::types::AxocoatlConfig;

/// Validate the browser block. Returns warnings that do not stop the daemon.
pub fn validate_browser(_config: &AxocoatlConfig) -> Result<Vec<ConfigWarning>, ConfigError> {
    Ok(Vec::new())
}
