//! Provider error policy for native Sessions: retry a transient failure
//! (429, 5xx, timeout, connection reset) once with backoff on the same
//! pinned model; never retry 400, 401, 402 or 403. Owner: workstream
//! `runtime`.

use std::time::Duration;

/// Retries after the first attempt.
pub const MAX_PROVIDER_RETRIES: u32 = 1;
/// Backoff before the retry when the provider gives no `Retry-After`.
pub const DEFAULT_RETRY_BACKOFF: Duration = Duration::from_secs(2);
/// Longest `Retry-After` honored; a longer one is not retried.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

/// What failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderFailure {
    Status {
        status: u16,
        retry_after: Option<Duration>,
    },
    Timeout,
    ConnectionReset,
    /// The stream ended with a refusal or a safety stop.
    Refusal,
    Other(String),
}

/// What to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryDecision {
    RetryAfter(Duration),
    GiveUp,
}

/// Decide whether attempt `attempt` (0 = the first) is retried.
pub fn decide(_failure: &ProviderFailure, _attempt: u32) -> RetryDecision {
    // Not implemented: never retry (the 1.2 behavior) until runtime fills it in.
    RetryDecision::GiveUp
}
