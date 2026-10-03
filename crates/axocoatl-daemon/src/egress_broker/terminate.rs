//! Serving one relayed route connection: TLS accept, request checks,
//! write-ahead records, credential injection, upstream forwarding and the
//! response checks. See the module documentation of [`super`].

use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::IpAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axocoatl_session::network_record::{
    Decision, EgressBinding, NetworkEvent, ResponseOutcome, MAX_RECORDED_PATH_CHARS,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode, Uri, Version};
use hyper_util::rt::{TokioIo, TokioTimer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::ServerConfig;
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use super::ca::SessionCa;
use super::rules::{canonicalize, Injection, Route, RuleDecision};
use super::scan::ReflectionScanner;
use super::upstream::{UpstreamBody, UpstreamConnector};
use super::{BoxError, BrokerRecordSink};

/// The Workspaces a credential file or `upstream_ca` must stay out of,
/// read again for each use.
pub type WorkspaceRoots = Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>;

/// The response body type the broker sends to the client.
type ClientBody = http_body_util::combinators::BoxBody<Bytes, BoxError>;

/// Header that says the broker refused a request, and why.
pub const DENIED_HEADER: &str = "x-axocoatl-egress";
/// Longest request head (request line and headers) read.
const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_HEADERS: usize = 128;

/// Request headers never forwarded: per-connection headers.
const HOP_BY_HOP: [&str; 7] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Time limits of one relayed connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrokerTimeouts {
    /// The client's TLS handshake.
    pub handshake: Duration,
    /// Waiting for a request head, including between requests.
    pub header_read: Duration,
    /// Waiting for the upstream's response head once a request is sent.
    pub response_head: Duration,
    /// Waiting for the record to take events still queued at the end.
    pub record_drain: Duration,
}

impl Default for BrokerTimeouts {
    fn default() -> Self {
        Self {
            handshake: Duration::from_secs(10),
            header_read: Duration::from_secs(30),
            response_head: Duration::from_secs(600),
            record_drain: Duration::from_secs(5),
        }
    }
}

/// What the broker knows about one relayed connection.
#[derive(Clone)]
pub struct RelayContext {
    pub session: String,
    /// `"g{generation}:{id}"`, as in the connection's `open` event.
    pub conn: String,
    pub route: Arc<Route>,
    /// The `CONNECT` host; the route host.
    pub host: String,
    pub port: u16,
    /// The addresses the decision point resolved and allowed, in order.
    pub addrs: Vec<IpAddr>,
    pub binding: Option<EgressBinding>,
    /// The egress credential's tag, for logs.
    pub token_tag: Option<String>,
}

impl std::fmt::Debug for RelayContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RelayContext")
            .field("session", &self.session)
            .field("conn", &self.conn)
            .field("route", &self.route.label())
            .field("host", &self.host)
            .field("port", &self.port)
            .field("addrs", &self.addrs)
            .finish_non_exhaustive()
    }
}

/// How a relayed connection ended, for its `close` event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BrokerOutcome {
    /// Requests read, allowed or refused.
    pub requests: u64,
    /// Request body bytes sent upstream.
    pub up: u64,
    /// Response body bytes passed to the client.
    pub down: u64,
    pub ms: u64,
    /// Why the broker ended the connection, when it was not an ordinary
    /// close: `sni_mismatch`, `alpn_refused`, `tls_failed`,
    /// `credential_reflected`, `http_error`, `certificate`.
    pub error: Option<String>,
}

/// One Session's broker: its authority, the upstream connector and the
/// Workspaces credentials must stay out of.
pub struct SessionBroker {
    ca: Arc<SessionCa>,
    upstream: Arc<UpstreamConnector>,
    workspaces: WorkspaceRoots,
    timeouts: BrokerTimeouts,
}

impl std::fmt::Debug for SessionBroker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionBroker")
            .field("ca", &self.ca)
            .field("timeouts", &self.timeouts)
            .finish_non_exhaustive()
    }
}

/// [`SessionBroker::serve`] as a function.
pub async fn serve<S>(
    broker: &SessionBroker,
    ctx: RelayContext,
    stream: S,
    sink: Arc<dyn BrokerRecordSink>,
) -> BrokerOutcome
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    broker.serve(ctx, stream, sink).await
}

/// Serves the route host's certificate only when the client named it.
#[derive(Debug)]
struct RouteResolver {
    host: String,
    key: Arc<CertifiedKey>,
    seen: Mutex<Option<Option<String>>>,
}

impl ResolvesServerCert for RouteResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello
            .server_name()
            .map(|name| name.trim_end_matches('.').to_ascii_lowercase());
        let matches = name.as_deref() == Some(self.host.as_str());
        *self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(name);
        matches.then(|| self.key.clone())
    }
}

/// A short printable form of a client-supplied value for records and logs.
fn printable(value: &str, max: usize) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_graphic() { c } else { '?' })
        .take(max)
        .collect()
}

impl SessionBroker {
    pub fn new(
        ca: Arc<SessionCa>,
        upstream: Arc<UpstreamConnector>,
        workspaces: WorkspaceRoots,
    ) -> Self {
        Self {
            ca,
            upstream,
            workspaces,
            timeouts: BrokerTimeouts::default(),
        }
    }

    pub fn with_timeouts(mut self, timeouts: BrokerTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    pub fn ca(&self) -> &Arc<SessionCa> {
        &self.ca
    }

    /// Serve one relayed connection to its end.
    pub async fn serve<S>(
        &self,
        ctx: RelayContext,
        stream: S,
        sink: Arc<dyn BrokerRecordSink>,
    ) -> BrokerOutcome
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let started = Instant::now();
        let finish = |error: Option<String>| BrokerOutcome {
            ms: started.elapsed().as_millis() as u64,
            error,
            ..BrokerOutcome::default()
        };
        let route = ctx.route.clone();
        let requested = ctx.host.trim_end_matches('.').to_ascii_lowercase();
        if requested != route.host {
            return finish(Some(format!(
                "route_mismatch: a connection to {} was given {}",
                printable(&requested, 253),
                route.label()
            )));
        }
        if !route.covers_port(ctx.port) {
            return finish(Some(format!(
                "route_mismatch: a connection to port {} was given {}, which covers {:?}",
                ctx.port,
                route.label(),
                route.ports
            )));
        }
        let key = match self.ca.certified_key(&route.host) {
            Ok(key) => key,
            Err(error) => {
                tracing::warn!(session = %ctx.session, conn = %ctx.conn, %error, "route certificate unavailable");
                return finish(Some(format!("certificate: {error}")));
            }
        };
        let resolver = Arc::new(RouteResolver {
            host: route.host.clone(),
            key,
            seen: Mutex::new(None),
        });
        let config = ServerConfig::builder_with_provider(super::crypto_provider())
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
            .map(|builder| {
                builder
                    .with_no_client_auth()
                    .with_cert_resolver(resolver.clone())
            });
        let mut config = match config {
            Ok(config) => config,
            Err(error) => return finish(Some(format!("tls_failed: {error}"))),
        };
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let tls = match tokio::time::timeout(self.timeouts.handshake, acceptor.accept(stream)).await
        {
            Ok(Ok(tls)) => tls,
            Ok(Err(error)) => {
                let seen = resolver
                    .seen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let reason = match seen {
                    Some(name) if name.as_deref() != Some(route.host.as_str()) => format!(
                        "sni_mismatch: the client asked for {} on a connection to {}",
                        name.map_or_else(|| "no server name".into(), |name| printable(&name, 253)),
                        route.host
                    ),
                    _ if error
                        .get_ref()
                        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
                        .is_some_and(|inner| {
                            matches!(inner, rustls::Error::NoApplicationProtocol)
                        }) =>
                    {
                        "alpn_refused: the client offered no http/1.1".into()
                    }
                    _ => format!("tls_failed: {error}"),
                };
                tracing::info!(session = %ctx.session, conn = %ctx.conn, host = %route.host, %reason, "route TLS refused");
                return finish(Some(reason));
            }
            Err(_) => return finish(Some("tls_failed: the handshake timed out".into())),
        };

        let (recorder, drained) = Recorder::spawn(sink);
        let connection = Arc::new(Connection {
            ctx,
            route,
            recorder: recorder.clone(),
            upstream: self.upstream.clone(),
            workspaces: self.workspaces.clone(),
            timeouts: self.timeouts,
            sender: tokio::sync::Mutex::new(None),
            seq: AtomicU64::new(0),
            up: Arc::new(AtomicU64::new(0)),
            down: Arc::new(AtomicU64::new(0)),
            stopped: Mutex::new(None),
        });
        let service = {
            let connection = connection.clone();
            hyper::service::service_fn(move |request: Request<Incoming>| {
                let connection = connection.clone();
                async move { Ok::<_, Infallible>(connection.handle(request).await) }
            })
        };
        let result = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(self.timeouts.header_read)
            .keep_alive(true)
            .half_close(false)
            .max_buf_size(MAX_HEAD_BYTES)
            .max_headers(MAX_HEADERS)
            .serve_connection(TokioIo::new(tls), service)
            .await;
        drop(recorder);
        let outcome = BrokerOutcome {
            requests: connection.seq.load(Ordering::SeqCst),
            up: connection.up.load(Ordering::SeqCst),
            down: connection.down.load(Ordering::SeqCst),
            ms: 0,
            error: connection
                .stopped
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .or_else(|| {
                    // A client that goes away is an ordinary close; only a
                    // malformed request is worth naming.
                    result
                        .err()
                        .filter(|error| error.is_parse() || error.is_parse_too_large())
                        .map(|error| format!("http_error: {error}"))
                }),
        };
        drop(connection);
        let _ = tokio::time::timeout(self.timeouts.record_drain, drained).await;
        BrokerOutcome {
            ms: started.elapsed().as_millis() as u64,
            ..outcome
        }
    }
}

/// One queued event and, for a write-ahead event, who waits for it.
type RecordItem = (NetworkEvent, Option<oneshot::Sender<Result<(), String>>>);

/// Appends events in order on one task. A sender that needs the event
/// written before it goes on waits for the acknowledgement.
#[derive(Clone)]
struct Recorder {
    tx: mpsc::UnboundedSender<RecordItem>,
}

impl Recorder {
    fn spawn(sink: Arc<dyn BrokerRecordSink>) -> (Self, oneshot::Receiver<()>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<RecordItem>();
        let (done_tx, done_rx) = oneshot::channel();
        tokio::spawn(async move {
            while let Some((event, ack)) = rx.recv().await {
                let result = sink.append(event).await;
                match ack {
                    Some(ack) => {
                        let _ = ack.send(result);
                    }
                    None => {
                        if let Err(error) = result {
                            tracing::warn!(%error, "a route event could not be recorded");
                        }
                    }
                }
            }
            let _ = done_tx.send(());
        });
        (Self { tx }, done_rx)
    }

    /// Queue an event without waiting.
    fn note(&self, event: NetworkEvent) {
        let _ = self.tx.send((event, None));
    }

    /// Append an event and wait until it is in the record.
    async fn write_ahead(&self, event: NetworkEvent) -> Result<(), String> {
        let (ack, done) = oneshot::channel();
        self.tx
            .send((event, Some(ack)))
            .map_err(|_| "the recorder stopped".to_string())?;
        done.await
            .unwrap_or_else(|_| Err("the recorder stopped".into()))
    }
}

/// One relayed connection's state.
struct Connection {
    ctx: RelayContext,
    route: Arc<Route>,
    recorder: Recorder,
    upstream: Arc<UpstreamConnector>,
    workspaces: WorkspaceRoots,
    timeouts: BrokerTimeouts,
    sender: tokio::sync::Mutex<Option<hyper::client::conn::http1::SendRequest<UpstreamBody>>>,
    seq: AtomicU64,
    up: Arc<AtomicU64>,
    down: Arc<AtomicU64>,
    /// Set when the broker itself ended the connection.
    stopped: Mutex<Option<String>>,
}

/// One refusal: status, code, and what to tell the client.
struct Refusal {
    status: StatusCode,
    code: &'static str,
    reason: String,
    hint: String,
    close: bool,
}

/// The fields every `request` event of one request shares.
struct RequestFacts {
    seq: u64,
    method: String,
    path: String,
    host: String,
}

fn bounded_path(path: &str) -> String {
    printable(path, MAX_RECORDED_PATH_CHARS)
}

/// The JSON refusal sent to the client.
fn refusal_response(refusal: &Refusal, host: &str, facts: &RequestFacts) -> Response<ClientBody> {
    let body = json!({
        "error": refusal.code,
        "reason": refusal.reason,
        "host": host,
        "method": facts.method,
        "path": facts.path,
        "hint": refusal.hint,
    });
    let mut response = Response::new(
        Full::new(Bytes::from(body.to_string()))
            .map_err(|never: Infallible| -> BoxError { match never {} })
            .boxed(),
    );
    *response.status_mut() = refusal.status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Ok(value) = HeaderValue::from_str(&format!("denied; reason={}", refusal.code)) {
        headers.insert(HeaderName::from_static(DENIED_HEADER), value);
    }
    if refusal.close {
        headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    response
}

/// Whether `headers` hold exactly one `Host` naming `host` (with `:port`
/// optional).
fn host_header(headers: &HeaderMap, host: &str, port: u16) -> Result<(), String> {
    let mut values = headers.get_all(header::HOST).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return Err("-".into());
    };
    let text = value.to_str().map_err(|_| "?".to_string())?;
    let lower = text.to_ascii_lowercase();
    let (name, given_port) = match lower.rsplit_once(':') {
        Some((name, digits)) => (name, Some(digits)),
        None => (lower.as_str(), None),
    };
    let name = name.strip_suffix('.').unwrap_or(name);
    let port_ok = match given_port {
        None => true,
        Some(digits) => digits.parse::<u16>().ok() == Some(port) && !digits.starts_with('0'),
    };
    if name == host && port_ok {
        Ok(())
    } else {
        Err(printable(text, 253))
    }
}

/// Whether the request asks for a protocol upgrade.
fn asks_upgrade(request: &Request<Incoming>) -> bool {
    request.method() == Method::CONNECT
        || request.headers().contains_key(header::UPGRADE)
        || request
            .headers()
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        || request.headers().contains_key("http2-settings")
}

/// Request headers that servers, frameworks and origin routers read as the
/// request's method, path or host in place of the request line and `Host`
/// the route checked. Every `x-forwarded-*` header is refused too.
const OVERRIDE_HEADERS: [&str; 10] = [
    "x-http-method-override",
    "x-http-method",
    "x-method-override",
    "x-original-method",
    "x-original-url",
    "x-original-uri",
    "x-rewrite-url",
    "x-original-host",
    "x-host",
    "forwarded",
];

/// The first request header that could make the upstream read another
/// method, path or host, if any.
fn override_header(headers: &HeaderMap) -> Option<&str> {
    headers
        .keys()
        .map(HeaderName::as_str)
        .find(|name| OVERRIDE_HEADERS.contains(name) || name.starts_with("x-forwarded-"))
}

/// Whether a response body is encoded in a way the scan cannot read.
fn encoded(headers: &HeaderMap) -> bool {
    let content = headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|value| {
            value.to_str().map_or(true, |text| {
                text.split(',').any(|coding| {
                    !coding.trim().is_empty() && !coding.trim().eq_ignore_ascii_case("identity")
                })
            })
        });
    let transfer = headers
        .get_all(header::TRANSFER_ENCODING)
        .iter()
        .any(|value| {
            value.to_str().map_or(true, |text| {
                text.split(',').any(|coding| {
                    let coding = coding.trim();
                    !coding.eq_ignore_ascii_case("chunked")
                        && !coding.eq_ignore_ascii_case("identity")
                })
            })
        });
    content || transfer
}

/// The needles of a credential: the value and, for basic, the header's
/// base64 form.
fn needles(inject: &Injection, secret: &SecretString) -> Vec<Zeroizing<Vec<u8>>> {
    let mut needles = vec![Zeroizing::new(secret.expose_secret().as_bytes().to_vec())];
    if let Injection::Basic { username } = inject {
        use base64::Engine as _;
        let joined = Zeroizing::new(format!("{username}:{}", secret.expose_secret()));
        needles.push(Zeroizing::new(
            base64::engine::general_purpose::STANDARD
                .encode(joined.as_bytes())
                .into_bytes(),
        ));
    }
    needles
}

/// The header value that carries the credential.
fn injected_value(inject: &Injection, secret: &SecretString) -> Option<HeaderValue> {
    let text = Zeroizing::new(match inject {
        Injection::Basic { username } => {
            use base64::Engine as _;
            let joined = Zeroizing::new(format!("{username}:{}", secret.expose_secret()));
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(joined.as_bytes())
            )
        }
        Injection::Header { prefix, suffix, .. } => {
            format!("{prefix}{}{suffix}", secret.expose_secret())
        }
    });
    let mut value = HeaderValue::from_str(&text).ok()?;
    value.set_sensitive(true);
    Some(value)
}

impl Connection {
    fn stop(&self, reason: &str) {
        let mut stopped = self
            .stopped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if stopped.is_none() {
            *stopped = Some(reason.to_string());
        }
    }

    /// Record a refused request and answer it.
    fn refuse(&self, facts: &RequestFacts, refusal: Refusal) -> Response<ClientBody> {
        tracing::info!(
            session = %self.ctx.session,
            conn = %self.ctx.conn,
            host = %facts.host,
            method = %facts.method,
            path = %facts.path,
            code = refusal.code,
            "route request refused"
        );
        self.recorder.note(NetworkEvent::Request {
            conn: self.ctx.conn.clone(),
            seq_in_conn: facts.seq,
            method: facts.method.clone(),
            path: facts.path.clone(),
            host: facts.host.clone(),
            rule: None,
            decision: Decision::Deny,
            reason: Some(refusal.code.into()),
            credential: None,
        });
        refusal_response(&refusal, &self.route.host, facts)
    }

    fn response_event(
        &self,
        seq: u64,
        status: u16,
        up: u64,
        down: u64,
        started: Instant,
        outcome: ResponseOutcome,
    ) -> NetworkEvent {
        NetworkEvent::Response {
            conn: self.ctx.conn.clone(),
            seq_in_conn: seq,
            status,
            up,
            down,
            ms: started.elapsed().as_millis() as u64,
            outcome,
        }
    }

    async fn upstream_sender(
        &self,
    ) -> Result<hyper::client::conn::http1::SendRequest<UpstreamBody>, String> {
        let existing = self.sender.lock().await.take();
        if let Some(mut sender) = existing {
            if !sender.is_closed()
                && matches!(
                    tokio::time::timeout(Duration::from_secs(5), sender.ready()).await,
                    Ok(Ok(()))
                )
            {
                return Ok(sender);
            }
        }
        self.upstream
            .connect(&self.route, &self.ctx.addrs, self.ctx.port)
            .await
            .map_err(|error| error.0)
    }

    async fn handle(self: Arc<Self>, request: Request<Incoming>) -> Response<ClientBody> {
        let started = Instant::now();
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let route = self.route.clone();
        let target = request
            .uri()
            .path_and_query()
            .map_or_else(|| request.uri().to_string(), |pq| pq.as_str().to_string());
        let raw_path = target.split('?').next().unwrap_or_default();
        let mut facts = RequestFacts {
            seq,
            method: printable(request.method().as_str(), 32),
            path: bounded_path(raw_path),
            host: route.host.clone(),
        };
        let refusal = |status, code, reason: String, hint: &str, close| Refusal {
            status,
            code,
            reason,
            hint: hint.to_string(),
            close,
        };

        // A connection without a known process kind is never served.
        let not_for = match &self.ctx.binding {
            Some(binding) if route.allows_binding(binding.kind) => None,
            Some(binding) => Some(format!(
                "{} does not serve {:?} processes",
                route.label(),
                binding.kind
            )),
            None => Some(format!(
                "{} serves only processes of a known kind",
                route.label()
            )),
        };
        if let Some(reason) = not_for {
            return self.refuse(
                &facts,
                refusal(
                    StatusCode::FORBIDDEN,
                    "route_not_for_binding",
                    reason,
                    "Add the process kind to the route's for: list.",
                    true,
                ),
            );
        }
        if request.uri().scheme().is_some()
            || request.uri().authority().is_some()
            || !target.starts_with('/')
        {
            return self.refuse(
                &facts,
                refusal(
                    StatusCode::BAD_REQUEST,
                    "path_not_canonical",
                    "the request target is not a path".into(),
                    "Send requests in origin form (GET /path HTTP/1.1).",
                    true,
                ),
            );
        }
        if let Err(given) = host_header(request.headers(), &route.host, self.ctx.port) {
            facts.host = given;
            return self.refuse(
                &facts,
                refusal(
                    StatusCode::MISDIRECTED_REQUEST,
                    "host_mismatch",
                    format!("this connection is for {}", route.host),
                    "Send Host with the route's host. A route does not reach other sites on the same address.",
                    true,
                ),
            );
        }
        let canonical = match canonicalize(&target) {
            Ok(canonical) => canonical,
            Err(not) => {
                return self.refuse(
                    &facts,
                    refusal(
                        StatusCode::BAD_REQUEST,
                        "path_not_canonical",
                        not.0,
                        "Send the path without '.' or '..' segments (also before a ';'), '//', '\\' or escaped '/', '\\' or '.'.",
                        true,
                    ),
                )
            }
        };
        if asks_upgrade(&request) {
            return self.refuse(
                &facts,
                refusal(
                    StatusCode::FORBIDDEN,
                    "upgrade_not_allowed",
                    "routes forward HTTP/1.1 requests only".into(),
                    "WebSocket, HTTP/2 and other upgrades are refused on a route.",
                    true,
                ),
            );
        }
        if let Some(name) = override_header(request.headers()) {
            return self.refuse(
                &facts,
                refusal(
                    StatusCode::FORBIDDEN,
                    "override_header",
                    format!(
                        "the request carries {name}, which servers can read as another method, \
                         path or host than the one the route checked"
                    ),
                    "Send the method, path and host in the request itself, without X-HTTP-Method-Override, \
                     X-Original-URL, Forwarded, X-Forwarded-* or similar headers.",
                    false,
                ),
            );
        }
        let rule = match route.check(request.method().as_str(), &canonical) {
            RuleDecision::Allowed { rule } => rule,
            RuleDecision::Denied { reason, hint } => {
                return self.refuse(
                    &facts,
                    Refusal {
                        status: StatusCode::FORBIDDEN,
                        code: "route_denied",
                        reason,
                        hint,
                        close: false,
                    },
                )
            }
        };
        let declared = request
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        if declared.is_some_and(|length| length > route.max_request_bytes) {
            return self.refuse(
                &facts,
                refusal(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_too_large",
                    format!("the body is larger than {} bytes", route.max_request_bytes),
                    "Raise max_request_bytes on the route if this upload is expected.",
                    true,
                ),
            );
        }
        let secret = match &route.credential {
            None => None,
            Some(credential) => {
                let workspaces = (self.workspaces)();
                match credential.source.read(&credential.name, &workspaces) {
                    Ok(secret) => Some((credential, secret)),
                    Err(error) => {
                        tracing::warn!(session = %self.ctx.session, conn = %self.ctx.conn, %error, "route credential unavailable");
                        return self.refuse(
                            &facts,
                            refusal(
                                StatusCode::SERVICE_UNAVAILABLE,
                                "credential_unavailable",
                                format!("the daemon could not read credential {}", credential.name),
                                "Run axocoatl doctor on the computer running Axocoatl.",
                                false,
                            ),
                        );
                    }
                }
            }
        };

        // The credential's header and scanner are built before the request is
        // recorded as allowed, so a credential that cannot be sent is refused
        // with nothing recorded but the refusal.
        let injected = match &secret {
            None => None,
            Some((credential, secret)) => match injected_value(&credential.inject, secret) {
                Some(value) => Some((
                    credential.inject.header_name(),
                    value,
                    ReflectionScanner::new(needles(&credential.inject, secret)),
                )),
                None => {
                    return self.refuse(
                        &facts,
                        refusal(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "credential_unavailable",
                            format!("credential {} cannot be sent in a header", credential.name),
                            "The credential holds characters a header cannot carry.",
                            false,
                        ),
                    );
                }
            },
        };
        drop(secret);

        // Write-ahead: nothing goes upstream before this is recorded.
        let recorded = self
            .recorder
            .write_ahead(NetworkEvent::Request {
                conn: self.ctx.conn.clone(),
                seq_in_conn: seq,
                method: facts.method.clone(),
                path: bounded_path(&canonical.path),
                host: route.host.clone(),
                rule: Some(rule.clone()),
                decision: Decision::Allow,
                reason: None,
                credential: route
                    .credential
                    .as_ref()
                    .map(|credential| credential.name.clone()),
            })
            .await;
        if let Err(error) = recorded {
            tracing::warn!(session = %self.ctx.session, conn = %self.ctx.conn, %error, "route request not recorded; refused");
            self.stop("record_unavailable");
            return refusal_response(
                &refusal(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "record_unavailable",
                    "the Session's network record is full or unavailable".into(),
                    "New requests are refused until the record can be written.",
                    true,
                ),
                &route.host,
                &facts,
            );
        }
        tracing::debug!(
            session = %self.ctx.session,
            conn = %self.ctx.conn,
            host = %route.host,
            method = %facts.method,
            path = %facts.path,
            rule = %rule,
            credential = route.credential.as_ref().map(|credential| credential.name.as_str()),
            "route request allowed"
        );

        let (parts, body) = request.into_parts();
        let connection_tokens: Vec<String> = parts
            .headers
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .map(|token| token.trim().to_ascii_lowercase())
            .collect();
        let inject_name = injected.as_ref().map(|(name, _, _)| name.clone());
        let credentialed = injected.is_some();
        let mut headers = HeaderMap::with_capacity(parts.headers.len() + 2);
        for (name, value) in &parts.headers {
            let lower = name.as_str();
            if HOP_BY_HOP.contains(&lower)
                || connection_tokens.iter().any(|token| token == lower)
                || name == header::HOST
                || name == header::PROXY_AUTHORIZATION
                || name == header::EXPECT
            {
                continue;
            }
            if credentialed
                && (name == header::AUTHORIZATION
                    || Some(name) == inject_name.as_ref()
                    || name == header::ACCEPT_ENCODING
                    || name == header::RANGE
                    || name == header::IF_RANGE)
            {
                continue;
            }
            headers.append(name.clone(), value.clone());
        }
        let host_value = if self.ctx.port == 443 {
            route.host.clone()
        } else {
            format!("{}:{}", route.host, self.ctx.port)
        };
        if let Ok(value) = HeaderValue::from_str(&host_value) {
            headers.insert(header::HOST, value);
        }
        let mut scanner = None;
        if let Some((name, value, needles)) = injected {
            headers.insert(name, value);
            headers.insert(
                header::ACCEPT_ENCODING,
                HeaderValue::from_static("identity"),
            );
            scanner = needles;
        }

        let up = Arc::new(AtomicU64::new(0));
        let too_large = Arc::new(AtomicBool::new(false));
        let body = RequestBody {
            inner: body,
            sent: up.clone(),
            total: self.up.clone(),
            max: route.max_request_bytes,
            too_large: too_large.clone(),
        };
        let uri: Uri = match canonical.query.as_deref() {
            Some(query) => format!("{}?{query}", canonical.path),
            None => canonical.path.clone(),
        }
        .parse()
        .unwrap_or_else(|_| Uri::from_static("/"));
        let mut upstream_request = Request::new(body.boxed());
        *upstream_request.method_mut() = parts.method;
        *upstream_request.uri_mut() = uri;
        *upstream_request.version_mut() = Version::HTTP_11;
        *upstream_request.headers_mut() = headers;

        let fail = |status: StatusCode, code: &'static str, reason: String, hint: &str, outcome| {
            self.recorder.note(self.response_event(
                seq,
                status.as_u16(),
                up.load(Ordering::SeqCst),
                0,
                started,
                outcome,
            ));
            tracing::info!(session = %self.ctx.session, conn = %self.ctx.conn, host = %route.host, code, "route request failed");
            refusal_response(
                &refusal(status, code, reason, hint, true),
                &route.host,
                &facts,
            )
        };
        let mut sender = match self.upstream_sender().await {
            Ok(sender) => sender,
            Err(error) => {
                return fail(
                    StatusCode::BAD_GATEWAY,
                    "upstream_failed",
                    error,
                    "The route's upstream could not be reached.",
                    ResponseOutcome::UpstreamFailed,
                )
            }
        };
        let response = match tokio::time::timeout(
            self.timeouts.response_head,
            sender.send_request(upstream_request),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                if too_large.load(Ordering::SeqCst) {
                    return fail(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "request_too_large",
                        format!("the body is larger than {} bytes", route.max_request_bytes),
                        "Raise max_request_bytes on the route if this upload is expected.",
                        ResponseOutcome::TooLarge,
                    );
                }
                return fail(
                    StatusCode::BAD_GATEWAY,
                    "upstream_failed",
                    format!("the upstream failed: {error}"),
                    "The route's upstream broke off the request.",
                    ResponseOutcome::UpstreamFailed,
                );
            }
            Err(_) => {
                return fail(
                    StatusCode::GATEWAY_TIMEOUT,
                    "upstream_failed",
                    "the upstream did not answer in time".into(),
                    "The route's upstream did not answer.",
                    ResponseOutcome::UpstreamFailed,
                )
            }
        };
        let (mut head, upstream_body) = response.into_parts();
        if credentialed && !route.allow_encoded_responses && encoded(&head.headers) {
            return fail(
                StatusCode::BAD_GATEWAY,
                "encoded_response",
                "the upstream sent a compressed response, which a credentialed route refuses".into(),
                "Ask for an uncompressed response, or set allow_encoded_responses: true on the route.",
                ResponseOutcome::EncodedResponse,
            );
        }
        if let Some(scanner) = &scanner {
            // hyper keeps a reason phrase other than the usual one and writes
            // it back to the client, so it is searched with the headers.
            let reflected = head.headers.iter().any(|(name, value)| {
                scanner.contains(name.as_str().as_bytes()) || scanner.contains(value.as_bytes())
            }) || head
                .extensions
                .get::<hyper::ext::ReasonPhrase>()
                .is_some_and(|reason| scanner.contains(reason.as_bytes()));
            if reflected {
                self.stop("credential_reflected");
                tracing::warn!(session = %self.ctx.session, conn = %self.ctx.conn, host = %route.host, "a route response carried the credential; stopped");
                return fail(
                    StatusCode::BAD_GATEWAY,
                    "credential_reflected",
                    "the upstream's response carried the route's credential".into(),
                    "Axocoatl stopped the response so the credential does not reach the container.",
                    ResponseOutcome::CredentialReflected,
                );
            }
        }
        // The upstream connection is reusable once this body is read.
        *self.sender.lock().await = Some(sender);
        for name in HOP_BY_HOP {
            head.headers.remove(name);
        }
        let status = head.status.as_u16();
        let body = ResponseBody {
            inner: Some(upstream_body),
            scanner,
            queued: VecDeque::new(),
            down: 0,
            total_down: self.down.clone(),
            report: Some(ResponseReport {
                connection: self.clone(),
                seq,
                status,
                up,
                started,
            }),
        };
        Response::from_parts(head, body.boxed())
    }
}

/// Sends a request's `response` event once.
struct ResponseReport {
    connection: Arc<Connection>,
    seq: u64,
    status: u16,
    up: Arc<AtomicU64>,
    started: Instant,
}

impl ResponseReport {
    fn send(self, down: u64, outcome: ResponseOutcome) {
        let event = self.connection.response_event(
            self.seq,
            self.status,
            self.up.load(Ordering::SeqCst),
            down,
            self.started,
            outcome,
        );
        self.connection.recorder.note(event);
    }
}

/// The client's request body on its way upstream: counted, and cut off past
/// the route's `max_request_bytes`.
struct RequestBody {
    inner: Incoming,
    sent: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    max: u64,
    too_large: Arc<AtomicBool>,
}

#[derive(Debug, thiserror::Error)]
#[error("the request body is larger than the route allows")]
struct TooLarge;

impl Body for RequestBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let len = data.len() as u64;
                    let sent = this.sent.fetch_add(len, Ordering::SeqCst) + len;
                    if sent > this.max {
                        this.too_large.store(true, Ordering::SeqCst);
                        return Poll::Ready(Some(Err(Box::new(TooLarge))));
                    }
                    this.total.fetch_add(len, Ordering::SeqCst);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(Box::new(error)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// The upstream's response body on its way to the client: scanned for the
/// credential on credentialed routes, counted, and reported once.
struct ResponseBody {
    inner: Option<Incoming>,
    scanner: Option<ReflectionScanner>,
    queued: VecDeque<Frame<Bytes>>,
    down: u64,
    total_down: Arc<AtomicU64>,
    report: Option<ResponseReport>,
}

#[derive(Debug, thiserror::Error)]
#[error("the response was stopped: it carried the route's credential")]
struct Stopped;

impl ResponseBody {
    fn count(&mut self, bytes: &Bytes) {
        self.down += bytes.len() as u64;
        self.total_down
            .fetch_add(bytes.len() as u64, Ordering::SeqCst);
    }

    fn end(&mut self, outcome: ResponseOutcome) {
        if let Some(report) = self.report.take() {
            report.send(self.down, outcome);
        }
    }

    fn reflected(&mut self) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        self.inner = None;
        self.queued.clear();
        if let Some(report) = &self.report {
            report.connection.stop("credential_reflected");
            tracing::warn!(
                session = %report.connection.ctx.session,
                conn = %report.connection.ctx.conn,
                "a route response carried the credential; stopped"
            );
        }
        self.end(ResponseOutcome::CredentialReflected);
        Poll::Ready(Some(Err(Box::new(Stopped))))
    }
}

impl Drop for ResponseBody {
    fn drop(&mut self) {
        // hyper may drop a body it knows has ended (a HEAD response, or one
        // that reported its end) without polling it to the end.
        let complete = self.queued.is_empty()
            && self
                .scanner
                .as_ref()
                .is_none_or(|scanner| scanner.held() == 0)
            && self.inner.as_ref().is_none_or(Body::is_end_stream);
        self.end(if complete {
            ResponseOutcome::Completed
        } else {
            ResponseOutcome::ClientClosed
        });
    }
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = &mut *self;
        loop {
            if let Some(frame) = this.queued.pop_front() {
                if let Some(data) = frame.data_ref() {
                    let data = data.clone();
                    this.count(&data);
                }
                return Poll::Ready(Some(Ok(frame)));
            }
            let Some(inner) = this.inner.as_mut() else {
                this.end(ResponseOutcome::Completed);
                return Poll::Ready(None);
            };
            match Pin::new(inner).poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Err(error))) => {
                    this.inner = None;
                    this.end(ResponseOutcome::UpstreamFailed);
                    return Poll::Ready(Some(Err(Box::new(error))));
                }
                Poll::Ready(None) => {
                    this.inner = None;
                    if let Some(scanner) = this.scanner.as_mut() {
                        let tail = scanner.finish();
                        if !tail.is_empty() {
                            this.queued.push_back(Frame::data(tail));
                        }
                    }
                }
                Poll::Ready(Some(Ok(frame))) => {
                    let frame = match frame.into_data() {
                        Ok(data) => match this.scanner.as_mut() {
                            None => Frame::data(data),
                            Some(scanner) => match scanner.push(&data) {
                                Ok(passed) if passed.is_empty() => continue,
                                Ok(passed) => Frame::data(passed),
                                Err(_) => return this.reflected(),
                            },
                        },
                        Err(frame) => match frame.into_trailers() {
                            Ok(trailers) => {
                                if let Some(scanner) = this.scanner.as_mut() {
                                    if trailers.iter().any(|(name, value)| {
                                        scanner.contains(name.as_str().as_bytes())
                                            || scanner.contains(value.as_bytes())
                                    }) {
                                        return this.reflected();
                                    }
                                    let tail = scanner.finish();
                                    if !tail.is_empty() {
                                        this.queued.push_back(Frame::data(tail));
                                    }
                                }
                                this.queued.push_back(Frame::trailers(trailers));
                                continue;
                            }
                            Err(_) => continue,
                        },
                    };
                    if let Some(data) = frame.data_ref() {
                        let data = data.clone();
                        this.count(&data);
                    }
                    return Poll::Ready(Some(Ok(frame)));
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.queued.is_empty()
            && self
                .scanner
                .as_ref()
                .is_none_or(|scanner| scanner.held() == 0)
            && self.inner.as_ref().is_none_or(Body::is_end_stream)
    }

    fn size_hint(&self) -> SizeHint {
        match &self.inner {
            Some(inner) if self.scanner.is_none() => inner.size_hint(),
            _ => SizeHint::default(),
        }
    }
}

#[cfg(test)]
#[path = "terminate_tests.rs"]
mod tests;
