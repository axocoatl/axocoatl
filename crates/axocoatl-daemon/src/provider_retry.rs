//! Provider error policy for native Sessions: retry a transient failure
//! (429, 5xx, timeout, connection reset) once with backoff on the same
//! pinned model; never retry 400, 401, 402 or 403, a refusal or a safety
//! stop. Owner: workstream `runtime`.
//!
//! The policy is applied at the Session's provider boundary
//! (`session_dispatch_provider.rs`), where every call is admitted against
//! its grant: a retry is a new provider call with its own reservation, and
//! the failed call keeps the accounting 1.2 gives it (an incomplete call
//! keeps its whole reservation). Only a failure that arrives before the
//! provider yielded anything is retried, so nothing of the failed attempt
//! ever reached the Agent. Each retry is recorded on the activation's stream
//! as a `provider_retry` observation, which the control-plane projection
//! shows as evidence; [`run_events`] turns those into the run record's
//! [`RunEvent::ProviderRetry`] events.

use std::time::Duration;

use axocoatl_llm::ProviderError;
use axocoatl_session::run_record::RunEvent;

use crate::session_control_plane::{
    ControlPlaneActivationRef, EvidenceValue, SessionTurnControlPlane,
};

/// Retries after the first attempt.
pub const MAX_PROVIDER_RETRIES: u32 = 1;
/// Backoff before the retry when the provider gives no `Retry-After`.
pub const DEFAULT_RETRY_BACKOFF: Duration = Duration::from_secs(2);
/// Longest `Retry-After` honored; a longer one is not retried.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
/// How native providers state a response's `Retry-After` at the end of an
/// [`ProviderError::ApiError`] message: `… (Retry-After: 5 s)`.
pub const RETRY_AFTER_SUFFIX: &str = " (Retry-After: ";

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

/// Statuses that may succeed when sent again: request timeout, rate limit
/// and every server error.
fn transient_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..=599).contains(&status)
}

/// Decide whether attempt `attempt` (0 = the first) is retried.
pub fn decide(failure: &ProviderFailure, attempt: u32) -> RetryDecision {
    if attempt >= MAX_PROVIDER_RETRIES {
        return RetryDecision::GiveUp;
    }
    match failure {
        ProviderFailure::Status {
            status,
            retry_after,
        } if transient_status(*status) => match retry_after {
            Some(wait) if *wait > MAX_RETRY_AFTER => RetryDecision::GiveUp,
            Some(wait) => RetryDecision::RetryAfter(*wait),
            None => RetryDecision::RetryAfter(DEFAULT_RETRY_BACKOFF),
        },
        ProviderFailure::Timeout | ProviderFailure::ConnectionReset => {
            RetryDecision::RetryAfter(DEFAULT_RETRY_BACKOFF)
        }
        ProviderFailure::Status { .. } | ProviderFailure::Refusal | ProviderFailure::Other(_) => {
            RetryDecision::GiveUp
        }
    }
}

/// The `Retry-After` a native provider appended to an API error message.
pub fn retry_after_hint(message: &str) -> Option<Duration> {
    let (_, rest) = message.rsplit_once(RETRY_AFTER_SUFFIX)?;
    let seconds = rest.strip_suffix(" s)")?;
    seconds.parse::<u64>().ok().map(Duration::from_secs)
}

/// `message` with `retry_after` appended as [`retry_after_hint`] reads it.
pub fn with_retry_after(message: String, retry_after: Option<Duration>) -> String {
    match retry_after {
        Some(wait) => format!("{message}{RETRY_AFTER_SUFFIX}{} s)", wait.as_secs()),
        None => message,
    }
}

/// Whether a transport message names a timeout.
fn names_timeout(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("timed out") || lower.contains("timeout")
}

/// Whether a transport message names a connection the peer reset or closed
/// before the response.
fn names_reset(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "connection reset",
        "connection closed",
        "connection aborted",
        "broken pipe",
        "unexpected eof",
        "reset by peer",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Classify one provider error for [`decide`].
pub fn failure_of(error: &ProviderError) -> ProviderFailure {
    match error {
        ProviderError::ApiError {
            status, message, ..
        } => ProviderFailure::Status {
            status: *status,
            retry_after: retry_after_hint(message),
        },
        ProviderError::RateLimited {
            retry_after_secs, ..
        } => ProviderFailure::Status {
            status: 429,
            retry_after: retry_after_secs.map(Duration::from_secs),
        },
        ProviderError::AuthError { .. } => ProviderFailure::Status {
            status: 401,
            retry_after: None,
        },
        ProviderError::ContentFiltered { .. } => ProviderFailure::Refusal,
        ProviderError::Network(message) | ProviderError::Stream(message) => {
            if names_timeout(message) {
                ProviderFailure::Timeout
            } else if names_reset(message) {
                ProviderFailure::ConnectionReset
            } else {
                ProviderFailure::Other(error.to_string())
            }
        }
        // A stream that ended before its completion record is retried by the
        // Agent's own loop (1.2), never twice.
        other => ProviderFailure::Other(other.to_string()),
    }
}

/// The note recorded on the activation for one retry. It starts with
/// `HTTP <status>` when the failure was a provider status, which
/// [`retry_status`] reads back.
pub fn retry_note(
    failure: &ProviderFailure,
    wait: Duration,
    provider: &str,
    model: &str,
) -> String {
    let what = match failure {
        ProviderFailure::Status { status, .. } => format!("HTTP {status} from {provider}"),
        ProviderFailure::Timeout => format!("{provider} timed out"),
        ProviderFailure::ConnectionReset => format!("{provider} closed the connection"),
        ProviderFailure::Refusal => format!("{provider} refused"),
        ProviderFailure::Other(message) => message.chars().take(200).collect(),
    };
    let note = format!(
        "{what}; retrying once in {} s on the same model ({model}) as a new, separately \
         reserved call",
        wait.as_secs()
    );
    note.chars().take(512).collect()
}

/// The provider status a recorded retry note names, if any.
pub fn retry_status(note: &str) -> Option<u16> {
    note.strip_prefix("HTTP ")?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// One [`RunEvent::ProviderRetry`] for each provider retry the turn's
/// control-plane projection records, in node and activation order: the
/// retries this policy made and the 1.2 retry of a stream that ended
/// early. This is the hook core's run driver uses for loadout runs: the
/// activation path does not know a run id, the projection does not change
/// after the turn, and the events carry the time each retry was recorded.
pub fn run_events(plane: &SessionTurnControlPlane) -> Vec<RunEvent> {
    let mut events = Vec::new();
    for node in &plane.nodes {
        for activation in &node.activations {
            let node_id = match &activation.reference {
                ControlPlaneActivationRef::Exact { activation } => {
                    activation.node_id.as_str().to_owned()
                }
                ControlPlaneActivationRef::Legacy { node_id, .. } => node_id.clone(),
            };
            for evidence in activation
                .evidence
                .iter()
                .filter(|evidence| evidence.kind == "provider_retry")
            {
                let reason = match &evidence.summary {
                    EvidenceValue::Available { value } | EvidenceValue::Truncated { value, .. } => {
                        value.clone()
                    }
                    _ => String::new(),
                };
                let at_ms = match evidence.recorded_at {
                    EvidenceValue::Available { value } => value,
                    _ => 0,
                };
                events.push(RunEvent::ProviderRetry {
                    at_ms,
                    node_id: node_id.clone(),
                    status: retry_status(&reason),
                    reason,
                });
            }
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(status: u16) -> ProviderFailure {
        ProviderFailure::Status {
            status,
            retry_after: None,
        }
    }

    /// The whole table: what is retried, after how long, and that a second
    /// failure is never retried.
    #[test]
    fn transient_failures_are_retried_once_and_rejections_never() {
        for failure in [
            status(429),
            status(500),
            status(502),
            status(503),
            status(504),
            status(408),
            ProviderFailure::Timeout,
            ProviderFailure::ConnectionReset,
        ] {
            assert_eq!(
                decide(&failure, 0),
                RetryDecision::RetryAfter(DEFAULT_RETRY_BACKOFF),
                "{failure:?}"
            );
            assert_eq!(decide(&failure, 1), RetryDecision::GiveUp, "{failure:?}");
            assert_eq!(decide(&failure, 7), RetryDecision::GiveUp, "{failure:?}");
        }
        for failure in [
            status(400),
            status(401),
            status(402),
            status(403),
            status(404),
            status(422),
            ProviderFailure::Refusal,
            ProviderFailure::Other("invalid request".into()),
        ] {
            assert_eq!(decide(&failure, 0), RetryDecision::GiveUp, "{failure:?}");
        }
    }

    #[test]
    fn retry_after_is_honored_up_to_thirty_seconds() {
        let after = |seconds| ProviderFailure::Status {
            status: 429,
            retry_after: Some(Duration::from_secs(seconds)),
        };
        assert_eq!(
            decide(&after(0), 0),
            RetryDecision::RetryAfter(Duration::ZERO)
        );
        assert_eq!(
            decide(&after(7), 0),
            RetryDecision::RetryAfter(Duration::from_secs(7))
        );
        assert_eq!(
            decide(&after(30), 0),
            RetryDecision::RetryAfter(MAX_RETRY_AFTER)
        );
        assert_eq!(decide(&after(31), 0), RetryDecision::GiveUp);
        assert_eq!(decide(&after(3600), 0), RetryDecision::GiveUp);
        // A 503 with Retry-After too.
        let unavailable = ProviderFailure::Status {
            status: 503,
            retry_after: Some(Duration::from_secs(5)),
        };
        assert_eq!(
            decide(&unavailable, 0),
            RetryDecision::RetryAfter(Duration::from_secs(5))
        );
        // A rejection is not retried whatever it says.
        let rejected = ProviderFailure::Status {
            status: 401,
            retry_after: Some(Duration::from_secs(1)),
        };
        assert_eq!(decide(&rejected, 0), RetryDecision::GiveUp);
    }

    #[test]
    fn provider_errors_map_to_failures() {
        let api = |status, message: &str| ProviderError::ApiError {
            provider: "openrouter".into(),
            status,
            message: message.into(),
        };
        assert_eq!(
            failure_of(&api(503, "busy")),
            ProviderFailure::Status {
                status: 503,
                retry_after: None
            }
        );
        assert_eq!(
            failure_of(&api(
                429,
                &with_retry_after("slow down".into(), Some(Duration::from_secs(9)))
            )),
            ProviderFailure::Status {
                status: 429,
                retry_after: Some(Duration::from_secs(9))
            }
        );
        assert_eq!(
            failure_of(&ProviderError::RateLimited {
                provider: "p".into(),
                retry_after_secs: Some(3)
            }),
            ProviderFailure::Status {
                status: 429,
                retry_after: Some(Duration::from_secs(3))
            }
        );
        assert_eq!(
            failure_of(&ProviderError::AuthError {
                provider: "p".into()
            }),
            status(401)
        );
        assert_eq!(
            failure_of(&ProviderError::ContentFiltered {
                provider: "p".into(),
                reason: "safety".into()
            }),
            ProviderFailure::Refusal
        );
        for timeout in [
            ProviderError::Network("timed out: operation timed out".into()),
            ProviderError::Stream("native OpenRouter: response headers timed out".into()),
            ProviderError::Stream("openrouter stream idle timeout".into()),
            ProviderError::Stream("native Ollama: request header timeout".into()),
        ] {
            assert_eq!(failure_of(&timeout), ProviderFailure::Timeout, "{timeout}");
        }
        for reset in [
            ProviderError::Network("connection reset: error sending request".into()),
            ProviderError::Network("connection closed before message completed".into()),
            ProviderError::Network("broken pipe".into()),
        ] {
            assert_eq!(
                failure_of(&reset),
                ProviderFailure::ConnectionReset,
                "{reset}"
            );
        }
        for other in [
            ProviderError::Network("dns error: no such host".into()),
            ProviderError::Stream("native OpenRouter: invalid SSE JSON".into()),
            ProviderError::IncompleteStream {
                provider: "ollama".into(),
                message: "EOF before native terminal".into(),
            },
            ProviderError::InvalidRequest {
                provider: "p".into(),
                message: "too big".into(),
            },
            ProviderError::BudgetExhausted {
                provider: "p".into(),
                message: "no budget".into(),
            },
        ] {
            assert!(
                matches!(failure_of(&other), ProviderFailure::Other(_)),
                "{other}"
            );
            assert_eq!(decide(&failure_of(&other), 0), RetryDecision::GiveUp);
        }
    }

    #[test]
    fn retry_after_hint_reads_only_the_suffix_the_providers_write() {
        assert_eq!(
            retry_after_hint("busy (Retry-After: 12 s)"),
            Some(Duration::from_secs(12))
        );
        assert_eq!(retry_after_hint("busy"), None);
        assert_eq!(retry_after_hint("busy (Retry-After: soon s)"), None);
        assert_eq!(retry_after_hint("busy (Retry-After: 12 s) trailing"), None);
        assert_eq!(
            with_retry_after("x".into(), None),
            "x",
            "no header, no suffix"
        );
    }

    #[test]
    fn retry_notes_name_the_status_they_retried() {
        let note = retry_note(&status(503), DEFAULT_RETRY_BACKOFF, "openrouter", "m/x");
        assert!(note.starts_with("HTTP 503 from openrouter; retrying once in 2 s"));
        assert_eq!(retry_status(&note), Some(503));
        let note = retry_note(
            &ProviderFailure::Timeout,
            DEFAULT_RETRY_BACKOFF,
            "ollama",
            "q",
        );
        assert_eq!(retry_status(&note), None);
        assert_eq!(retry_status("LLM provider stream ended early: EOF"), None);
        assert!(
            retry_note(
                &ProviderFailure::Other("x".repeat(2000)),
                Duration::ZERO,
                "p",
                "m"
            )
            .len()
                <= 512
        );
    }
}
