//! What a native OpenRouter call says when it fails before producing
//! anything, which failures another endpoint of the same model may avoid,
//! and which endpoints answered.
//!
//! Every failure is said in plain words: OpenRouter's own message at most,
//! never its raw JSON, its upstream's raw text (which carries a hint to add a
//! provider key, BYOK, that native Sessions refuse) or account data. A
//! failure where nothing reached the model, or that OpenRouter refused with
//! an HTTP status before generating anything, says so in words a metered
//! host reads to settle the call at zero ([`charge_free`]).
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
/// upstream provider that refused. A body that is not OpenRouter's JSON is
/// quoted as text; other JSON is not quoted.
struct Said {
    message: Option<String>,
    upstream: bool,
}

impl Said {
    fn parse(body: &str) -> Self {
        let Ok(value) = serde_json::from_str::<Value>(body) else {
            // JSON cut at the bounded body, or other structured text, is
            // never quoted: it may carry the upstream's raw text.
            let text = quoted(body);
            let structured = text.starts_with('{') || text.starts_with('[');
            return Self {
                message: (!text.is_empty() && !structured).then_some(text),
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

/// What a request to `observation` that OpenRouter refused with `status`
/// and `body` says, before its `Retry-After`.
pub(super) fn refusal(
    observation: &NativeOpenRouterObservation,
    status: u16,
    body: &str,
) -> String {
    let said = Said::parse(body);
    let endpoint = describe(observation);
    let model = &observation.model;
    match status {
        429 if !said.upstream => format!(
            "{ACCOUNT_LIMITED} right now (HTTP 429{}); this is OpenRouter's limit on the \
             account, not one endpoint's: wait and try again; {NOTHING_GENERATED}",
            said.detail()
        ),
        429 => format!(
            "OpenRouter reports endpoint {endpoint} of {model} as rate-limited upstream right \
             now (HTTP 429), a provider outage, not your setup: try again later, or choose \
             another model; {NOTHING_GENERATED}"
        ),
        408 | 500..=599 => format!(
            "OpenRouter reports endpoint {endpoint} of {model} as unavailable right now (HTTP \
             {status}{}), a provider outage, not your setup: try again later, or choose another \
             model; {NOTHING_GENERATED}",
            said.detail()
        ),
        _ => format!(
            "OpenRouter refused the call to endpoint {endpoint} of {model} (HTTP \
             {status}{}); {NOTHING_GENERATED}",
            said.detail()
        ),
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
/// before a call: the call was never sent. A refused read already says what
/// it is ([`catalog_refusal`]); a transport failure is said as one of the
/// catalog's.
pub(super) fn catalog_unreadable(error: ProviderError) -> ProviderError {
    match error {
        ProviderError::ApiError { .. } | ProviderError::InvalidRequest { .. } => {
            with_words(error, &format!("; {NOTHING_SENT}"))
        }
        other => with_words(other, &format!("; {CATALOG_UNREADABLE}; {NOTHING_SENT}")),
    }
}

/// The text of a failure, whatever its kind.
fn words(error: &ProviderError) -> &str {
    match error {
        ProviderError::ApiError { message, .. }
        | ProviderError::InvalidRequest { message, .. }
        | ProviderError::Network(message)
        | ProviderError::Stream(message) => message,
        _ => "",
    }
}

/// Whether a transport failure's words name a timeout or a connection the
/// peer closed before the answer.
fn timed_out_or_closed(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
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

/// Whether a call that failed with `error` is known to have cost nothing:
/// it was never sent, or OpenRouter refused it with an HTTP status before
/// generating anything.
pub(super) fn charge_free(error: &ProviderError) -> bool {
    let text = words(error);
    match error {
        ProviderError::ApiError { provider, .. }
        | ProviderError::InvalidRequest { provider, .. }
            if provider != PROVIDER =>
        {
            false
        }
        ProviderError::ApiError { .. }
        | ProviderError::InvalidRequest { .. }
        | ProviderError::Network(_)
        | ProviderError::Stream(_) => {
            text.contains(NOTHING_SENT) || text.contains(NOTHING_GENERATED)
        }
        _ => false,
    }
}

/// Whether `error` is one endpoint's own failure, which another endpoint of
/// the same model may not share: an upstream rate limit, an outage, a
/// timeout or a closed connection, an endpoint OpenRouter reports degraded,
/// or one whose retained contract changed. Not OpenRouter's limit on the
/// account, a catalog it could not read, or a rejection of the request.
pub(super) fn endpoint_failure(error: &ProviderError) -> bool {
    let text = words(error);
    if text.contains(ACCOUNT_LIMITED) || text.contains(CATALOG_UNREADABLE) {
        return false;
    }
    match error {
        ProviderError::ApiError { status, .. } => {
            matches!(status, 408 | 429) || (500..=599).contains(status)
        }
        ProviderError::InvalidRequest { message, .. } => message.contains(CONTRACT_CHANGED),
        ProviderError::Network(message) | ProviderError::Stream(message) => {
            timed_out_or_closed(message)
        }
        _ => false,
    }
}

/// One endpoint's failure in a few words.
fn short(error: &ProviderError) -> String {
    match error {
        ProviderError::ApiError {
            status: 429,
            message,
            ..
        } if message.contains(ACCOUNT_LIMITED) => {
            "OpenRouter limited the account (HTTP 429)".into()
        }
        ProviderError::ApiError { status: 429, .. } => "rate-limited (HTTP 429)".into(),
        ProviderError::ApiError { status: 408, .. } => "timed out (HTTP 408)".into(),
        ProviderError::ApiError {
            status, message, ..
        } if message.contains("degraded") => {
            format!("reported unavailable or degraded (HTTP {status})")
        }
        ProviderError::ApiError { status, .. } if (500..=599).contains(status) => {
            format!("unavailable (HTTP {status})")
        }
        ProviderError::InvalidRequest { message, .. } if message.contains(CONTRACT_CHANGED) => {
            "its contract changed since Team & budget reviewed it".into()
        }
        ProviderError::Network(message) | ProviderError::Stream(message)
            if timed_out_or_closed(message) =>
        {
            if message.to_ascii_lowercase().contains("time") {
                "timed out".into()
            } else {
                "closed the connection".into()
            }
        }
        other => quoted(&other.to_string()),
    }
}

/// Whether `error` is an outage: a rate limit, a server error, a timeout or
/// a closed connection, or an endpoint OpenRouter reports degraded.
fn outage(error: &ProviderError) -> bool {
    match error {
        ProviderError::ApiError { status, .. } => {
            matches!(status, 408 | 429) || (500..=599).contains(status)
        }
        ProviderError::Network(message) | ProviderError::Stream(message) => {
            timed_out_or_closed(message)
        }
        _ => false,
    }
}

/// The error of one call that every endpoint it was sent to failed before
/// producing anything. Each endpoint is named once, with its last failure.
/// When all of them were outages, it says that OpenRouter reports them
/// rate-limited or unavailable, which is a provider outage, with what to do,
/// as an API error with the last status and `Retry-After`, so a review
/// round can still be run again after it. A call that ended on a failure
/// that is not one endpoint's own (OpenRouter's limit on the account, a
/// rejection of the request or the key) ends with that failure as it is.
pub(super) fn exhausted(
    endpoints: &[NativeOpenRouterObservation],
    mut failures: Vec<(usize, ProviderError)>,
) -> ProviderError {
    if failures
        .last()
        .is_some_and(|(_, last)| !endpoint_failure(last))
    {
        if let Some((_, last)) = failures.pop() {
            return last;
        }
    }
    let model = endpoints
        .first()
        .map(|endpoint| endpoint.model.clone())
        .unwrap_or_default();
    let mut order: Vec<usize> = Vec::new();
    let mut last: BTreeMap<usize, String> = BTreeMap::new();
    for (index, error) in &failures {
        if !order.contains(index) {
            order.push(*index);
        }
        last.insert(*index, short(error));
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
    let retry_after = failures.last().and_then(|(_, error)| {
        words(error)
            .rsplit_once(RETRY_AFTER_SUFFIX)
            .map(|(_, after)| format!("{RETRY_AFTER_SUFFIX}{after}"))
    });
    if failures.iter().all(|(_, error)| outage(error)) {
        let status = failures
            .iter()
            .rev()
            .find_map(|(_, error)| match error {
                ProviderError::ApiError { status, .. } => Some(*status),
                _ => None,
            })
            .unwrap_or(503);
        return ProviderError::ApiError {
            provider: PROVIDER.into(),
            status,
            message: format!(
                "OpenRouter reports every endpoint of {model} that this Agent may use as \
                 rate-limited or unavailable right now ({list}). This is a provider outage, \
                 not your setup: try again later, or choose another model; nothing from them \
                 reached the Agent{}",
                retry_after.unwrap_or_default()
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
    /// The calls it answered.
    pub calls: u32,
    /// The calls it failed before producing anything, each of which moved
    /// to another endpoint of the model.
    pub moved: u32,
}

/// The endpoints one model's native OpenRouter calls went to, in the order
/// each was first counted: a call is counted as answered once its answer
/// names the provider of the endpoint it pinned, and as moved when it moves
/// to another endpoint after that one failed it. A loadout run's Outcome,
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

    /// A call `observation`'s endpoint answered.
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
