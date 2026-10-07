//! Classify why a node, helper, area or provider call ended without a
//! result, so the Outcome can list it as not covered with a reason.
//!
//! Owner: workstream `runtime`.

use crate::run_outcome::FailureClass;

#[derive(Debug, thiserror::Error)]
pub enum FailureClassError {
    #[error("failure class: not implemented: {0}")]
    NotImplemented(&'static str),
}

/// What is known about one failure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailureFacts<'a> {
    /// The recorded failure message.
    pub message: &'a str,
    /// The provider's HTTP status, when the failure was a provider response.
    pub http_status: Option<u16>,
    /// The recorded failure class string (`round_limit`, `budget`, ...).
    pub recorded_class: Option<&'a str>,
    /// The stream ended with a refusal or a safety stop.
    pub refusal: bool,
}

/// Classify one failure.
pub fn classify_failure(_facts: &FailureFacts<'_>) -> Result<FailureClass, FailureClassError> {
    Err(FailureClassError::NotImplemented("classify_failure"))
}
