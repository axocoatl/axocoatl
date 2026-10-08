//! Classify why a node, helper, area or provider call ended without a
//! result, so the Outcome can list it as not covered with a reason.
//!
//! The facts come from what the host recorded: the provider's HTTP status,
//! whether the stream ended with a refusal or a safety stop, and a recorded
//! class string. A failed activation's recorded failure is its host-written
//! `Activation failed: …` line, which [`facts_from_failure_text`] reads into
//! those facts; only that host-written first line decides, never the model's
//! own text after it.
//!
//! Owner: workstream `runtime`.

use crate::execution_content::classify_activation_failure;
use crate::run_outcome::FailureClass;

/// Kept so callers written against the 1.3 skeleton still compile;
/// [`classify_failure`] classifies every input and never returns it.
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

/// The class of a provider HTTP status.
fn status_class(status: u16) -> FailureClass {
    match status {
        // Retried once under the transient-error policy, then given up.
        408 | 429 | 500..=599 => FailureClass::ProviderFailure,
        // Never retried: the request or the account was refused.
        400..=499 => FailureClass::ProviderRejected,
        _ => FailureClass::Other,
    }
}

/// The class of a recorded class string, when it names one.
fn recorded(class: &str) -> Option<FailureClass> {
    Some(match class {
        "provider_refusal" | "refusal" | "content_filter" | "safety_stop" => {
            FailureClass::ProviderRefusal
        }
        "provider_failure" | "provider_error" | "provider_incomplete" => {
            FailureClass::ProviderFailure
        }
        "provider_rejected" => FailureClass::ProviderRejected,
        "budget" | "budget_limited" | "deadline" | "admission" | "grant" => FailureClass::Budget,
        "blocked" | "scope_violation" | "capture_unavailable" => FailureClass::Blocked,
        "not_reached" | "never_started" | "not_started" => FailureClass::NotReached,
        "runtime_limit" | "round_limit" | "context_limit" | "tool_round_limit" => {
            FailureClass::RuntimeLimit
        }
        "stopped" | "cancelled" | "interrupted" => FailureClass::Stopped,
        "other" => FailureClass::Other,
        _ => return None,
    })
}

/// Whether a provider error message names a refusal or a safety stop, as
/// the host words them (`Content filtered by …`).
pub fn names_refusal(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("content filtered by")
        || lower.contains("content_filter")
        || lower.contains("safety classifier")
        || lower.contains("provider refused")
}

/// The HTTP status a host-written provider error names: `… API error: 503 - …`.
pub fn provider_status(message: &str) -> Option<u16> {
    let (_, rest) = message.split_once("API error: ")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let status = digits.parse::<u16>().ok()?;
    (100..=599).contains(&status).then_some(status)
}

/// Classify one failure. A refusal outranks a status, a status outranks a
/// recorded class, and the message decides last.
pub fn classify_failure(facts: &FailureFacts<'_>) -> Result<FailureClass, FailureClassError> {
    if facts.refusal {
        return Ok(FailureClass::ProviderRefusal);
    }
    if let Some(status) = facts.http_status {
        return Ok(status_class(status));
    }
    if let Some(class) = facts.recorded_class.and_then(recorded) {
        return Ok(class);
    }
    let read = facts_from_failure_text(facts.message);
    if read.refusal {
        return Ok(FailureClass::ProviderRefusal);
    }
    if let Some(status) = read.http_status {
        return Ok(status_class(status));
    }
    if let Some(class) = read.recorded_class.and_then(recorded) {
        return Ok(class);
    }
    let first = facts.message.lines().next().unwrap_or("").trim();
    Ok(match first {
        "Stopped" => FailureClass::Stopped,
        _ => FailureClass::Other,
    })
}

/// The facts a recorded activation failure states in its host-written first
/// line (`Activation failed: …`): its class as the workbench shows it, the
/// provider's HTTP status and whether the provider refused. Text after the
/// first line, which can be the model's own, never decides.
pub fn facts_from_failure_text(text: &str) -> FailureFacts<'_> {
    let first = text.lines().next().unwrap_or("");
    let Some(view) = classify_activation_failure(first) else {
        return FailureFacts {
            message: text,
            ..FailureFacts::default()
        };
    };
    FailureFacts {
        message: text,
        http_status: view.http_status,
        recorded_class: Some(view.class),
        refusal: view.refusal,
    }
}

/// Classify a recorded activation failure text (see
/// [`facts_from_failure_text`]).
pub fn classify_failure_text(text: &str) -> FailureClass {
    classify_failure(&facts_from_failure_text(text)).unwrap_or(FailureClass::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(facts: FailureFacts<'_>) -> FailureClass {
        classify_failure(&facts).unwrap()
    }

    #[test]
    fn facts_classify_in_precedence_order() {
        let status = |status| FailureFacts {
            http_status: Some(status),
            ..FailureFacts::default()
        };
        for (code, expected) in [
            (400, FailureClass::ProviderRejected),
            (401, FailureClass::ProviderRejected),
            (402, FailureClass::ProviderRejected),
            (403, FailureClass::ProviderRejected),
            (404, FailureClass::ProviderRejected),
            (408, FailureClass::ProviderFailure),
            (429, FailureClass::ProviderFailure),
            (500, FailureClass::ProviderFailure),
            (502, FailureClass::ProviderFailure),
            (503, FailureClass::ProviderFailure),
            (504, FailureClass::ProviderFailure),
            (302, FailureClass::Other),
        ] {
            assert_eq!(class(status(code)), expected, "{code}");
        }
        // A refusal outranks a status and a class.
        assert_eq!(
            class(FailureFacts {
                refusal: true,
                http_status: Some(503),
                recorded_class: Some("round_limit"),
                ..FailureFacts::default()
            }),
            FailureClass::ProviderRefusal
        );
        // A status outranks a class.
        assert_eq!(
            class(FailureFacts {
                http_status: Some(401),
                recorded_class: Some("budget"),
                ..FailureFacts::default()
            }),
            FailureClass::ProviderRejected
        );
        for (recorded, expected) in [
            ("provider_refusal", FailureClass::ProviderRefusal),
            ("provider_failure", FailureClass::ProviderFailure),
            ("provider_error", FailureClass::ProviderFailure),
            ("provider_incomplete", FailureClass::ProviderFailure),
            ("provider_rejected", FailureClass::ProviderRejected),
            ("budget", FailureClass::Budget),
            ("budget_limited", FailureClass::Budget),
            ("deadline", FailureClass::Budget),
            ("admission", FailureClass::Budget),
            ("blocked", FailureClass::Blocked),
            ("scope_violation", FailureClass::Blocked),
            ("capture_unavailable", FailureClass::Blocked),
            ("not_reached", FailureClass::NotReached),
            ("never_started", FailureClass::NotReached),
            ("round_limit", FailureClass::RuntimeLimit),
            ("context_limit", FailureClass::RuntimeLimit),
            ("runtime_limit", FailureClass::RuntimeLimit),
            ("stopped", FailureClass::Stopped),
            ("interrupted", FailureClass::Stopped),
            ("other", FailureClass::Other),
        ] {
            assert_eq!(
                class(FailureFacts {
                    recorded_class: Some(recorded),
                    ..FailureFacts::default()
                }),
                expected,
                "{recorded}"
            );
        }
        // An unknown class falls back to the message.
        assert_eq!(
            class(FailureFacts {
                recorded_class: Some("something_new"),
                message: "Stopped",
                ..FailureFacts::default()
            }),
            FailureClass::Stopped
        );
        assert_eq!(class(FailureFacts::default()), FailureClass::Other);
    }

    /// What the host writes for a failed activation classifies from its
    /// first line alone.
    #[test]
    fn recorded_activation_failures_classify_from_their_host_line() {
        for (text, expected) in [
            (
                "Activation failed: LLM provider error: Content filtered by openrouter: the \
                 provider stopped the response (finish reason content_filter)",
                FailureClass::ProviderRefusal,
            ),
            (
                "Activation failed: LLM provider error: openrouter API error: 401 - no auth",
                FailureClass::ProviderRejected,
            ),
            (
                "Activation failed: LLM provider error: openrouter API error: 402 - credits",
                FailureClass::ProviderRejected,
            ),
            (
                "Activation failed: LLM provider error: openrouter API error: 503 - busy \
                 (Retry-After: 2 s)",
                FailureClass::ProviderFailure,
            ),
            (
                "Activation failed: LLM provider error: Network error: timed out: request",
                FailureClass::ProviderFailure,
            ),
            (
                "Activation failed: LLM provider stream ended early: EOF",
                FailureClass::ProviderFailure,
            ),
            (
                "Activation failed: This Agent reached its tool-round limit for this activation \
                 (1,024 rounds) and still asked for more.",
                FailureClass::RuntimeLimit,
            ),
            (
                "Activation failed: Current request needs 9000 tokens",
                FailureClass::RuntimeLimit,
            ),
            (
                "Activation failed: The Session budget for this Agent is used up: 0 left.",
                FailureClass::Budget,
            ),
            (
                "Activation failed: This Agent reached its token limit for this activation.",
                FailureClass::Budget,
            ),
            (
                "Activation failed: it changed a.txt outside the paths this Agent may change \
                 (none); the change is kept for review.",
                FailureClass::Blocked,
            ),
            (
                "Activation failed: its repository captures cannot establish which files it \
                 changed, so changes outside the paths this Agent may change (lib/) cannot be \
                 ruled out; any change is kept for review.",
                FailureClass::Blocked,
            ),
            (
                "Activation failed: it was stopped before the host could capture which files \
                 it changed, so changes outside the paths this Agent may change (lib/) cannot \
                 be ruled out; any change is kept for review.",
                FailureClass::Stopped,
            ),
            ("Activation failed: something new", FailureClass::Other),
            ("Stopped", FailureClass::Stopped),
            ("", FailureClass::Other),
        ] {
            assert_eq!(classify_failure_text(text), expected, "{text}");
        }
        // The model's own text after the host line never decides.
        assert_eq!(
            classify_failure_text(
                "Activation failed: something new\nLLM provider error: Content filtered by x"
            ),
            FailureClass::Other
        );
        let facts = facts_from_failure_text(
            "Activation failed: LLM provider error: openrouter API error: 429 - slow",
        );
        assert_eq!(facts.http_status, Some(429));
        assert_eq!(facts.recorded_class, Some("provider_error"));
        assert!(!facts.refusal);
        let facts = facts_from_failure_text(
            "Activation failed: LLM provider error: Content filtered by openrouter: stopped",
        );
        assert_eq!(facts.recorded_class, Some("provider_refusal"));
        assert!(facts.refusal);
    }

    #[test]
    fn provider_status_reads_only_the_host_format() {
        assert_eq!(provider_status("openrouter API error: 503 - x"), Some(503));
        assert_eq!(provider_status("ollama API error: 500 - x"), Some(500));
        assert_eq!(provider_status("API error: 99999 - x"), None);
        assert_eq!(provider_status("error 503"), None);
    }
}
