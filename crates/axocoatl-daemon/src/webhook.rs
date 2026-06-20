//! Lattice event egress — outbound webhooks.
//!
//! When the lattice publishes an event, a background dispatcher matches it
//! against the configured webhooks and POSTs a signed JSON payload to each
//! matching URL. This is the outbound counterpart to inbound A2A: signals leave,
//! opt-in, to systems *you* own.
//!
//! **Opt-in:** with no `webhooks:` configured the dispatcher is never spawned, so
//! a default install makes zero outbound requests — the air-gapped story holds.
//!
//! **Delivery semantics:** at-least-once *best-effort*, in-process. Transient
//! failures (timeout / 5xx / 429) are retried with exponential backoff + jitter,
//! honoring `Retry-After`; 4xx (other than 429) is not retried. Concurrency is
//! bounded; under a flood, deliveries are dropped with a warning rather than
//! spawning unbounded tasks. Deliveries are **not durable across a daemon
//! restart** — pair with a message broker if you need stronger guarantees.
//!
//! **Payload** (stable schema): `delivery_id`, `event_id`, `event_type` (the
//! canonical name), `produced_by`, `timestamp`, `payload`. When a `secret` is
//! configured, the body is HMAC-SHA256 signed and sent in
//! `X-Axocoatl-Signature: sha256=<hex>` so receivers can verify authenticity.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axocoatl_config::WebhookConfigYaml;
use axocoatl_coordination::EventNotification;
use hmac::{Hmac, Mac};
use reqwest::{Client, StatusCode};
use sha2::Sha256;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, Semaphore};
use tracing::{debug, info, warn};

type HmacSha256 = Hmac<Sha256>;

/// Max concurrent in-flight deliveries across all webhooks.
const MAX_INFLIGHT: usize = 32;
/// Attempts per delivery (1 initial + retries).
const MAX_ATTEMPTS: u32 = 4;
/// Per-request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Base backoff between retries (grows exponentially, with jitter).
const BASE_BACKOFF: Duration = Duration::from_millis(500);

/// A resolved, validated webhook in runtime form. Secrets are exposed from
/// `SecretString` once here and held in memory to sign/send — never logged
/// (this struct intentionally does not derive `Debug`).
struct Sink {
    name: String,
    url: String,
    events: Vec<String>,
    secret: Option<String>,
    headers: Vec<(String, String)>,
}

impl Sink {
    fn from_config(c: WebhookConfigYaml) -> Self {
        Self {
            name: c.name,
            url: c.url,
            events: c.events,
            secret: c.secret.map(|s| s.expose_secret().to_string()),
            headers: c
                .headers
                .into_iter()
                .map(|(k, v)| (k, v.expose_secret().to_string()))
                .collect(),
        }
    }

    /// Whether this sink wants the given event. An empty filter means all
    /// coordination events (pure telemetry excluded); a named filter matches
    /// exactly and may opt back into telemetry by naming it.
    fn matches(&self, event_name: &str, is_telemetry: bool) -> bool {
        if self.events.is_empty() {
            !is_telemetry
        } else {
            self.events.iter().any(|e| e == event_name)
        }
    }
}

/// Background dispatcher. Spawned by the daemon only when webhooks are
/// configured (see `bootstrap`).
pub async fn run_webhook_dispatcher(
    mut rx: broadcast::Receiver<EventNotification>,
    configs: Vec<WebhookConfigYaml>,
) {
    let sinks: Vec<Arc<Sink>> = configs
        .into_iter()
        .filter(|c| c.enabled)
        .map(|c| Arc::new(Sink::from_config(c)))
        .collect();
    if sinks.is_empty() {
        return;
    }

    let hosts: Vec<&str> = sinks.iter().map(|s| host_of(&s.url)).collect();
    info!(
        count = sinks.len(),
        ?hosts,
        "Lattice event egress active — webhooks POST to these hosts"
    );

    let client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .unwrap_or_else(|e| {
            warn!(error = %e, "falling back to default webhook HTTP client");
            Client::new()
        });
    let limiter = Arc::new(Semaphore::new(MAX_INFLIGHT));

    loop {
        let notif = match rx.recv().await {
            Ok(n) => n,
            // The lattice produced events faster than we drained them. Drop the
            // skipped ones and keep going — never die, never block the lattice.
            Err(RecvError::Lagged(skipped)) => {
                warn!(skipped, "webhook dispatcher lagged; dropped events");
                continue;
            }
            Err(RecvError::Closed) => break,
        };

        let event_name = notif.event_type.name();
        let is_telemetry = notif.event_type.is_telemetry();

        for sink in &sinks {
            if !sink.matches(event_name, is_telemetry) {
                continue;
            }
            // Bound total in-flight deliveries. Under a flood, drop rather than
            // spawn unbounded tasks or block the lattice receive loop.
            let Ok(permit) = limiter.clone().try_acquire_owned() else {
                warn!(webhook = %sink.name, event = event_name, "egress saturated — delivery dropped");
                continue;
            };
            let client = client.clone();
            let sink = sink.clone();
            let notif = notif.clone();
            tokio::spawn(async move {
                let _permit = permit; // released when the delivery (and its retries) finish
                deliver(&client, &sink, &notif).await;
            });
        }
    }
}

async fn deliver(client: &Client, sink: &Sink, notif: &EventNotification) {
    let event_name = notif.event_type.name();
    let delivery_id = uuid::Uuid::new_v4().to_string();
    let sent_at = now_secs();

    let payload = serde_json::json!({
        "delivery_id": delivery_id,
        "event_id": notif.event_id.0,
        "event_type": event_name,
        "produced_by": notif.produced_by,
        "timestamp": notif.timestamp,
        "payload": notif.payload,
    });
    let body = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(e) => {
            warn!(webhook = %sink.name, error = %e, "failed to serialize webhook payload");
            return;
        }
    };
    let signature = sink.secret.as_ref().map(|secret| sign(secret, &body));

    for attempt in 1..=MAX_ATTEMPTS {
        let mut req = client
            .post(&sink.url)
            .header("content-type", "application/json")
            .header("x-axocoatl-event", event_name)
            .header("x-axocoatl-delivery", &delivery_id)
            .header("x-axocoatl-timestamp", sent_at.to_string());
        if let Some(sig) = &signature {
            req = req.header("x-axocoatl-signature", format!("sha256={sig}"));
        }
        for (k, v) in &sink.headers {
            req = req.header(k, v);
        }

        match req.body(body.clone()).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    debug!(webhook = %sink.name, %status, attempt, "webhook delivered");
                    return;
                }
                if !is_retryable_status(status) {
                    warn!(webhook = %sink.name, %status, "webhook rejected (not retried)");
                    return;
                }
                warn!(webhook = %sink.name, %status, attempt, "webhook transient failure");
                if attempt < MAX_ATTEMPTS {
                    let delay = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
                    tokio::time::sleep(delay).await;
                }
            }
            Err(e) => {
                warn!(webhook = %sink.name, error = %e, attempt, "webhook request error");
                if attempt < MAX_ATTEMPTS {
                    tokio::time::sleep(backoff(attempt)).await;
                }
            }
        }
    }
    warn!(webhook = %sink.name, attempts = MAX_ATTEMPTS, "webhook delivery failed after retries");
}

/// HMAC-SHA256 over the body, hex-encoded.
fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Retry transient failures only: server errors and explicit rate limiting.
fn is_retryable_status(status: StatusCode) -> bool {
    status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS
}

/// Honor a numeric `Retry-After` (seconds) if the server sends one.
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Exponential backoff with ±25% jitter (cheap entropy — just spreads retries).
fn backoff(attempt: u32) -> Duration {
    let base = BASE_BACKOFF * 2u32.saturating_pow(attempt - 1);
    let jitter_pct = (now_nanos() % 50) as i64 - 25; // -25..=24
    let millis = base.as_millis() as i64;
    let jittered = millis + millis * jitter_pct / 100;
    Duration::from_millis(jittered.max(1) as u64)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// Host (no scheme/path) for transparency logging — never the full URL, which
/// could carry a token in a query string.
fn host_of(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_config::SecretString;
    use axocoatl_coordination::{EventId, EventType};
    use std::collections::HashMap;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg(url: String, events: Vec<&str>, secret: Option<&str>) -> WebhookConfigYaml {
        WebhookConfigYaml {
            name: "test".into(),
            url,
            events: events.into_iter().map(String::from).collect(),
            secret: secret.map(SecretString::from),
            headers: HashMap::new(),
            enabled: true,
        }
    }

    fn notif(event_type: EventType) -> EventNotification {
        EventNotification {
            event_id: EventId::new("evt-1"),
            event_type,
            payload: serde_json::json!({ "status": "done" }),
            produced_by: "agent-x".into(),
            timestamp: 1_700_000_000,
        }
    }

    /// Poll the mock server until it has at least `n` requests, or ~2s elapses.
    async fn wait_for(server: &MockServer, n: usize) -> Vec<wiremock::Request> {
        for _ in 0..40 {
            let reqs = server.received_requests().await.unwrap_or_default();
            if reqs.len() >= n {
                return reqs;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        server.received_requests().await.unwrap_or_default()
    }

    #[test]
    fn sign_is_deterministic_hmac() {
        let a = sign("topsecret", b"{\"x\":1}");
        let b = sign("topsecret", b"{\"x\":1}");
        assert_eq!(a, b, "signing must be deterministic");
        assert_ne!(a, sign("other", b"{\"x\":1}"), "key changes the signature");
        assert_eq!(a.len(), 64, "SHA-256 hex is 64 chars");
    }

    #[test]
    fn retryable_status_classification() {
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
    }

    #[tokio::test]
    async fn delivers_signed_payload_with_stable_schema() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let config = cfg(
            format!("{}/hook", server.uri()),
            vec!["TaskCompleted"],
            Some("topsecret"),
        );
        let (tx, rx) = broadcast::channel(16);
        tokio::spawn(run_webhook_dispatcher(rx, vec![config]));
        tx.send(notif(EventType::TaskCompleted {
            task_id: "t-1".into(),
        }))
        .unwrap();

        let reqs = wait_for(&server, 1).await;
        assert_eq!(reqs.len(), 1);
        let req = &reqs[0];

        // Stable schema.
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["event_type"], "TaskCompleted");
        assert_eq!(body["event_id"], "evt-1");
        assert_eq!(body["produced_by"], "agent-x");
        assert_eq!(body["timestamp"], 1_700_000_000u64);
        assert!(body["delivery_id"].as_str().unwrap().len() > 10);
        assert_eq!(body["payload"]["status"], "done");

        // Signature verifies against the exact bytes received.
        let got = req
            .headers
            .get("x-axocoatl-signature")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(got, format!("sha256={}", sign("topsecret", &req.body)));
        assert_eq!(
            req.headers.get("x-axocoatl-event").unwrap(),
            "TaskCompleted"
        );
    }

    #[tokio::test]
    async fn empty_filter_excludes_telemetry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        // events: [] => all coordination events, telemetry excluded.
        let config = cfg(server.uri(), vec![], None);
        let (tx, rx) = broadcast::channel(16);
        tokio::spawn(run_webhook_dispatcher(rx, vec![config]));

        // AgentActivated is pure telemetry — must NOT be delivered.
        tx.send(notif(EventType::AgentActivated {
            agent_id: "a".into(),
        }))
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "telemetry must be excluded from the default 'all'"
        );
    }

    #[tokio::test]
    async fn custom_event_filters_by_its_own_name() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let config = cfg(server.uri(), vec!["CodeReady"], None);
        let (tx, rx) = broadcast::channel(16);
        tokio::spawn(run_webhook_dispatcher(rx, vec![config]));

        tx.send(notif(EventType::Custom("Unrelated".into())))
            .unwrap();
        tx.send(notif(EventType::Custom("CodeReady".into())))
            .unwrap();

        let reqs = wait_for(&server, 1).await;
        tokio::time::sleep(Duration::from_millis(150)).await; // let any stray delivery land
        let reqs = server.received_requests().await.unwrap_or(reqs);
        assert_eq!(reqs.len(), 1, "only the matching custom event is delivered");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["event_type"], "CodeReady");
    }

    #[tokio::test]
    async fn retries_on_server_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let config = cfg(server.uri(), vec!["TaskCompleted"], None);
        let (tx, rx) = broadcast::channel(16);
        tokio::spawn(run_webhook_dispatcher(rx, vec![config]));
        tx.send(notif(EventType::TaskCompleted {
            task_id: "t".into(),
        }))
        .unwrap();

        // First retry lands after ~BASE_BACKOFF (500ms); wait_for polls up to 2s.
        let reqs = wait_for(&server, 2).await;
        assert!(
            reqs.len() >= 2,
            "a 500 must be retried; saw {} attempt(s)",
            reqs.len()
        );
    }

    #[tokio::test]
    async fn does_not_retry_client_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&server)
            .await;
        let config = cfg(server.uri(), vec!["TaskCompleted"], None);
        let (tx, rx) = broadcast::channel(16);
        tokio::spawn(run_webhook_dispatcher(rx, vec![config]));
        tx.send(notif(EventType::TaskCompleted {
            task_id: "t".into(),
        }))
        .unwrap();

        let reqs = wait_for(&server, 1).await;
        tokio::time::sleep(Duration::from_millis(700)).await; // past one backoff window
        let reqs = server.received_requests().await.unwrap_or(reqs);
        assert_eq!(reqs.len(), 1, "a 400 must not be retried");
    }
}
