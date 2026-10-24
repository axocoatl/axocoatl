//! What a native OpenRouter call says when it fails before producing
//! anything, what is known of it, and which endpoints answered.
//!
//! Every failure is said in plain words: OpenRouter's own message at most,
//! never its raw JSON, its upstream's raw text (which carries a hint to add a
//! provider key, BYOK, that native Sessions refuse), a body that is not
//! OpenRouter's JSON (such as an HTML error page) or account data. What is
//! known of a failure ([`FailureFacts`]: that it cost nothing, that it was
//! the endpoint's own) is set where it happened, never read back from the
//! words, which can quote OpenRouter's message.
use super::*;

/// Every failure this provider writes where nothing was sent to the model.
pub(super) const NOTHING_SENT: &str = "nothing was sent to the model";
/// Every refusal OpenRouter answered with an HTTP status: it generated
/// nothing and does not bill it.
pub(super) const NOTHING_GENERATED: &str = "nothing was generated";
/// How a failure to read OpenRouter's catalog begins: OpenRouter's own, not
/// one endpoint's.
pub(super) const CATALOG_UNREADABLE: &str = "OpenRouter's model catalog could not be read";
/// How a rate limit OpenRouter puts on the account begins: every endpoint
/// shares it.
pub(super) const ACCOUNT_LIMITED: &str = "OpenRouter is limiting this account's requests";
/// What an endpoint whose retained contract no longer matches the catalog
/// says.
pub(super) const CONTRACT_CHANGED: &str = "changed since Team & budget reviewed it";
/// OpenRouter's message when an upstream failed; it says nothing more.
const UPSTREAM_FAILED: &str = "Provider returned error";
/// Most characters of OpenRouter's own message a failure quotes.
const QUOTED_CHARS: usize = 200;

/// `deepinfra/bf16 (DeepInfra)`.
pub(super) fn describe(observation: &NativeOpenRouterObservation) -> String {
    format!(
        "{} ({})",
        observation.endpoint_tag, observation.provider_name
    )
}

/// `text` on one line, without control characters, at most
/// [`QUOTED_CHARS`] characters.
fn quoted(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut quoted: String = line
        .chars()
        .filter(|ch| !ch.is_control())
        .take(QUOTED_CHARS)
        .collect();
    if line.chars().count() > QUOTED_CHARS {
        quoted.push('…');
    }
    quoted
}

/// What an error body of OpenRouter's says: its message (none for an
/// upstream's bare "Provider returned error") and whether it names the
/// upstream provider that refused. A body that is not OpenRouter's JSON (an
/// HTML error page, a gateway's text, JSON cut at the bounded body) says
/// nothing: it may carry an upstream's raw text.
struct Said {
    message: Option<String>,
    upstream: bool,
}

impl Said {
    fn parse(body: &str) -> Self {
        let Ok(value) = serde_json::from_str::<Value>(body) else {
            return Self {
                message: None,
                upstream: false,
            };
        };
        let error = &value["error"];
        Self {
            message: error["message"]
                .as_str()
                .map(quoted)
                .filter(|message| !message.is_empty() && message != UPSTREAM_FAILED),
            upstream: error["metadata"]["provider_name"]
                .as_str()
                .is_some_and(|name| !name.is_empty()),
        }
    }

    /// `: MESSAGE` when there is one.
    fn detail(&self) -> String {
        self.message
            .as_deref()
            .map(|message| format!(": {message}"))
            .unwrap_or_default()
    }
}

/// A failure before anything was sent: it cost nothing.
pub(super) fn unsent(error: ProviderError, endpoint_failure: bool, summary: &str) -> FailedCall {
    FailedCall {
        error,
        facts: FailureFacts {
            cost_nothing: true,
            endpoint_failure,
            summary: summary.into(),
        },
    }
}

/// A request to `observation` that OpenRouter refused with `status`, `body`
/// and `retry_after`: generated nothing, so it cost nothing. An upstream's
/// rate limit (a 429 that names the provider), a 408 and a 5xx are the
/// endpoint's own; OpenRouter's limit on the account and a rejection of the
/// request or key are not.
pub(super) fn refusal(
    observation: &NativeOpenRouterObservation,
    status: u16,
    body: &str,
    retry_after: Option<u64>,
) -> FailedCall {
    let said = Said::parse(body);
    let endpoint = describe(observation);
    let model = &observation.model;
    let (message, endpoint_failure, summary) = match status {
        429 if !said.upstream => (
            format!(
                "{ACCOUNT_LIMITED} right now (HTTP 429{}); this is OpenRouter's limit on the \
                 account, not one endpoint's: wait and try again; {NOTHING_GENERATED}",
                said.detail()
            ),
            false,
            "OpenRouter limited the account (HTTP 429)".to_owned(),
        ),
        429 => (
            format!(
                "OpenRouter reports endpoint {endpoint} of {model} as rate-limited upstream \
                 right now (HTTP 429), a provider outage, not your setup: try again later, or \
                 choose another model; {NOTHING_GENERATED}"
            ),
            true,
            "rate-limited (HTTP 429)".to_owned(),
        ),
        408 | 500..=599 => (
            format!(
                "OpenRouter reports endpoint {endpoint} of {model} as unavailable right now \
                 (HTTP {status}{}), a provider outage, not your setup: try again later, or \
                 choose another model; {NOTHING_GENERATED}",
                said.detail()
            ),
            true,
            if status == 408 {
                "timed out (HTTP 408)".to_owned()
            } else {
                format!("unavailable (HTTP {status})")
            },
        ),
        _ => (
            format!(
                "OpenRouter refused the call to endpoint {endpoint} of {model} (HTTP \
                 {status}{}); {NOTHING_GENERATED}",
                said.detail()
            ),
            false,
            format!("refused (HTTP {status})"),
        ),
    };
    FailedCall {
        error: ProviderError::ApiError {
            provider: PROVIDER.into(),
            status,
            message: with_retry_after(message, retry_after),
        },
        facts: FailureFacts {
            cost_nothing: true,
            endpoint_failure,
            summary,
        },
    }
}

/// A request to an endpoint whose transport failed: `error`, after the
/// request may have reached OpenRouter, so it is not known to be free,
/// unless the connection to OpenRouter was never made (`connect`), which
/// every endpoint shares. A timeout or a closed connection once connected
/// (`interrupted`) is the endpoint's own.
pub(super) fn transport(error: ProviderError, connect: bool, interrupted: bool) -> FailedCall {
    let interrupted = interrupted && !connect;
    let summary = match &error {
        _ if !interrupted => String::new(),
        ProviderError::Network(message) | ProviderError::Stream(message)
            if message.to_ascii_lowercase().contains("time") =>
        {
            "timed out".to_owned()
        }
        _ => "closed the connection".to_owned(),
    };
    FailedCall {
        error,
        facts: FailureFacts {
            cost_nothing: connect,
            endpoint_failure: interrupted,
            summary,
        },
    }
}

/// An error OpenRouter reported inside the answer of `observation`, said
/// in plain words. Its numeric `code` is the upstream's HTTP status when it
/// is one, so a Session can tell a transient failure (429, 5xx) from a
/// rejection. The answer had started, so its accounting stays partial.
pub(super) fn answer_error(
    observation: &NativeOpenRouterObservation,
    failure: &Value,
) -> ProviderError {
    let said = Said {
        message: failure["message"]
            .as_str()
            .map(quoted)
            .filter(|message| !message.is_empty() && message != UPSTREAM_FAILED),
        upstream: false,
    };
    let words = format!(
        "endpoint {} of {} reported an error in its answer{}; its accounting remains partial",
        describe(observation),
        observation.model,
        said.detail()
    );
    match failure
        .get("code")
        .and_then(Value::as_u64)
        .filter(|code| (400..600).contains(code))
    {
        Some(status) => ProviderError::ApiError {
            provider: PROVIDER.into(),
            status: status as u16,
            message: format!("native OpenRouter: {words}"),
        },
        None => protocol(words),
    }
}

/// What is known of a call whose answer failed with `error` before any
/// output: never that it cost nothing, since the answer had started. A 408,
/// a 429 or a 5xx the answer reported, a timeout or a closed connection is
/// the endpoint's own; anything else (a protocol violation, an answer from
/// another provider) is not, and is never moved.
pub(super) fn answer_failure(error: ProviderError) -> FailedCall {
    FailedCall {
        facts: answer_facts(&error),
        error,
    }
}

/// [`answer_failure`]'s facts.
pub(super) fn answer_facts(error: &ProviderError) -> FailureFacts {
    let summary = match error {
        ProviderError::ApiError { status: 429, .. } => "rate-limited (HTTP 429)".to_owned(),
        ProviderError::ApiError { status: 408, .. } => "timed out (HTTP 408)".to_owned(),
        ProviderError::ApiError { status, .. } if (500..=599).contains(status) => {
            format!("unavailable (HTTP {status})")
        }
        ProviderError::Network(message) => interrupted_words(message),
        // This provider's own account of a malformed answer quotes the
        // answer; only its transport's words are read.
        ProviderError::Stream(message) if !message.starts_with(PROTOCOL_PREFIX) => {
            interrupted_words(message)
        }
        _ => String::new(),
    };
    FailureFacts {
        cost_nothing: false,
        endpoint_failure: !summary.is_empty(),
        summary,
    }
}

/// `timed out` or `closed the connection` when the words of a transport
/// failure (this provider's deadlines, or the HTTP client's) name one;
/// empty otherwise.
fn interrupted_words(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if !provider_transport_interrupted(&lower) {
        return String::new();
    }
    if lower.contains("time") {
        "timed out".to_owned()
    } else {
        "closed the connection".to_owned()
    }
}

/// Whether the lowercase words of a transport failure name a timeout or a
/// connection the peer closed.
fn provider_transport_interrupted(lower: &str) -> bool {
    [
        "timed out",
        "timeout",
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

/// `error` with `extra` added to its words, before any `Retry-After` an
/// API error states at its end.
pub(super) fn with_words(error: ProviderError, extra: &str) -> ProviderError {
    match error {
        ProviderError::ApiError {
            provider,
            status,
            message,
        } => {
            let (words, retry_after) = match message.rsplit_once(RETRY_AFTER_SUFFIX) {
                Some((words, after)) => (words.to_owned(), format!("{RETRY_AFTER_SUFFIX}{after}")),
                None => (message, String::new()),
            };
            ProviderError::ApiError {
                provider,
                status,
                message: format!("{words}{extra}{retry_after}"),
            }
        }
        ProviderError::InvalidRequest { provider, message } => ProviderError::InvalidRequest {
            provider,
            message: format!("{message}{extra}"),
        },
        ProviderError::Network(message) => ProviderError::Network(format!("{message}{extra}")),
        ProviderError::Stream(message) => ProviderError::Stream(format!("{message}{extra}")),
        other => other,
    }
}

/// What a read of OpenRouter's catalog that it refused with `status` and
/// `body` says: its status and OpenRouter's own message, never the raw body.
pub(super) fn catalog_refusal(status: u16, body: &str) -> String {
    format!(
        "{CATALOG_UNREADABLE} (HTTP {status}{})",
        Said::parse(body).detail()
    )
}

/// A failure to read OpenRouter's catalog, or to find the model in it,
/// before a call: the call was never sent, so it cost nothing, and the
/// catalog is every endpoint's, so no other endpoint avoids it. A refused
/// read already says what it is ([`catalog_refusal`]); a transport failure
/// is said as one of the catalog's.
pub(super) fn catalog_unreadable(error: ProviderError) -> FailedCall {
    let error = match error {
        ProviderError::ApiError { .. } | ProviderError::InvalidRequest { .. } => {
            with_words(error, &format!("; {NOTHING_SENT}"))
        }
        other => with_words(other, &format!("; {CATALOG_UNREADABLE}; {NOTHING_SENT}")),
    };
    unsent(error, false, "its catalog could not be read")
}

/// The `Retry-After` a message states at its very end, as
/// [`with_retry_after`] writes it.
fn retry_after_of(message: &str) -> Option<u64> {
    let (_, rest) = message.rsplit_once(RETRY_AFTER_SUFFIX)?;
    rest.strip_suffix(" s)")?.parse().ok()
}

/// The error of one call that the endpoints it was sent to failed before
/// it produced anything. Each endpoint is named once, with its last
/// failure. When all of them were outages (the endpoint's own failure that
/// is not a changed contract), it says that OpenRouter reports them
/// rate-limited or unavailable, which is a provider outage, with what to do,
/// as an API error with the last status and `Retry-After`, so a review
/// round can still be run again after it. A call that ended on a failure
/// that is not one endpoint's own (OpenRouter's limit on the account, a
/// rejection of the request or the key) ends with that failure as it is.
pub(super) fn exhausted(
    endpoints: &[NativeOpenRouterObservation],
    mut failures: Vec<(usize, FailedCall)>,
) -> ProviderError {
    if failures
        .last()
        .is_some_and(|(_, last)| !last.facts.endpoint_failure)
    {
        if let Some((_, last)) = failures.pop() {
            return last.error;
        }
    }
    let model = endpoints
        .first()
        .map(|endpoint| endpoint.model.clone())
        .unwrap_or_default();
    let mut order: Vec<usize> = Vec::new();
    let mut last: BTreeMap<usize, String> = BTreeMap::new();
    for (index, failed) in &failures {
        if !order.contains(index) {
            order.push(*index);
        }
        let summary = if failed.facts.summary.is_empty() {
            quoted(&failed.error.to_string())
        } else {
            failed.facts.summary.clone()
        };
        last.insert(*index, summary);
    }
    let list = order
        .iter()
        .map(|index| {
            let name = endpoints
                .get(*index)
                .map(describe)
                .unwrap_or_else(|| format!("endpoint {index}"));
            format!("{name}: {}", last[index])
        })
        .collect::<Vec<_>>()
        .join("; ");
    let retry_after = failures.last().and_then(|(_, failed)| match &failed.error {
        ProviderError::ApiError { message, .. } => retry_after_of(message),
        _ => None,
    });
    // A changed contract is the endpoint's own, but not an outage.
    let outage = |failed: &FailedCall| {
        failed.facts.endpoint_failure
            && matches!(
                failed.error,
                ProviderError::ApiError { .. }
                    | ProviderError::Network(_)
                    | ProviderError::Stream(_)
            )
    };
    if failures.iter().all(|(_, failed)| outage(failed)) {
        let status = failures
            .iter()
            .rev()
            .find_map(|(_, failed)| match failed.error {
                ProviderError::ApiError { status, .. } => Some(status),
                _ => None,
            })
            .unwrap_or(503);
        return ProviderError::ApiError {
            provider: PROVIDER.into(),
            status,
            message: with_retry_after(
                format!(
                    "OpenRouter reports every endpoint of {model} that this Agent may use as \
                     rate-limited or unavailable right now ({list}). This is a provider outage, \
                     not your setup: try again later, or choose another model; nothing from \
                     them reached the Agent"
                ),
                retry_after,
            ),
        };
    }
    invalid(format!(
        "no endpoint of {model} that this Agent may use could take the call ({list}); nothing \
         from them reached the Agent"
    ))
}

/// One endpoint native OpenRouter calls of a model went to: the calls it
/// answered, and the calls it failed before producing anything that moved on
/// to another endpoint ([`NativeOpenRouterServed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedOpenRouterEndpoint {
    /// The endpoint's tag, `deepinfra/bf16`.
    pub endpoint: String,
    /// The provider OpenRouter names for it, `DeepInfra`.
    pub provider: String,
    /// The calls it answered to the end.
    pub calls: u32,
    /// The calls it failed before producing anything, each of which moved
    /// to another endpoint of the model.
    pub moved: u32,
}

/// The endpoints one model's native OpenRouter calls went to, in the order
/// each was first counted: a call is counted as answered once its answer,
/// named as the provider of the endpoint it pinned, completed, and as moved
/// when it moves to another endpoint after that one failed it. A call that
/// failed after its answer started is neither. A loadout run's Outcome,
/// notes and receipt name them.
#[derive(Debug, Default)]
pub struct NativeOpenRouterServed(std::sync::Mutex<Vec<ServedOpenRouterEndpoint>>);

impl NativeOpenRouterServed {
    fn count(
        &self,
        observation: &NativeOpenRouterObservation,
        add: fn(&mut ServedOpenRouterEndpoint),
    ) {
        let mut served = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        let index = match served
            .iter()
            .position(|served| served.endpoint == observation.endpoint_tag)
        {
            Some(index) => index,
            None => {
                served.push(ServedOpenRouterEndpoint {
                    endpoint: observation.endpoint_tag.clone(),
                    provider: observation.provider_name.clone(),
                    calls: 0,
                    moved: 0,
                });
                served.len() - 1
            }
        };
        add(&mut served[index]);
    }

    /// A call `observation`'s endpoint answered to the end.
    pub(super) fn record(&self, observation: &NativeOpenRouterObservation) {
        self.count(observation, |served| {
            served.calls = served.calls.saturating_add(1)
        });
    }

    /// A call `observation`'s endpoint failed that moved to another one.
    pub(super) fn moved_from(&self, observation: &NativeOpenRouterObservation) {
        self.count(observation, |served| {
            served.moved = served.moved.saturating_add(1)
        });
    }

    /// Each endpoint calls went to, in the order it was first counted.
    pub fn endpoints(&self) -> Vec<ServedOpenRouterEndpoint> {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}
