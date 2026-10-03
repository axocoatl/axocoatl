//! The terminator end to end: a client over `tokio::io::duplex`, the
//! broker, and a local HTTPS upstream with its own test authority.

use super::*;
use crate::egress_broker::rules::RouteTable;
use crate::egress_broker::upstream::LocalCheck;
use axocoatl_config::{CredentialSourceYaml, EgressRouteYaml};
use axocoatl_session::network_record::BindingKind;
use http_body_util::{BodyExt, StreamBody};
use hyper::client::conn::http1::SendRequest;
use rustls::pki_types::ServerName;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::OnceLock;
use tokio::net::TcpListener;
use tokio::sync::Notify;

const HOST: &str = "api.test";
const SECRET_ENV: &str = "AXOCOATL_TERMINATE_TEST_SECRET";
const USERNAME: &str = "x-access-token";

/// The test credential: random per run, set once in this process.
fn secret() -> &'static str {
    static SECRET: OnceLock<String> = OnceLock::new();
    SECRET.get_or_init(|| {
        let mut bytes = [0u8; 18];
        getrandom::fill(&mut bytes).unwrap();
        let value = format!("axo-test-{}", hex::encode(bytes));
        std::env::set_var(SECRET_ENV, &value);
        value
    })
}

fn basic_form() -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(format!("{USERNAME}:{}", secret()))
}

// ---------------------------------------------------------------- upstream

#[derive(Debug, Clone)]
struct SeenRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body_len: usize,
}

impl SeenRequest {
    fn header(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

struct TestUpstream {
    addr: SocketAddr,
    ca: SessionCa,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

impl TestUpstream {
    fn seen(&self) -> Vec<SeenRequest> {
        self.seen.lock().unwrap().clone()
    }
}

type TestBody = http_body_util::combinators::BoxBody<Bytes, BoxError>;

fn full(bytes: impl Into<Bytes>) -> TestBody {
    Full::new(bytes.into())
        .map_err(|never: Infallible| -> BoxError { match never {} })
        .boxed()
}

async fn upstream_handler(
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    request: Request<Incoming>,
) -> Response<TestBody> {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .unwrap_or_default();
    let body = request
        .into_body()
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .unwrap_or_default();
    seen.lock().unwrap().push(SeenRequest {
        method,
        path: path.clone(),
        headers: headers.clone(),
        body_len: body.len(),
    });
    let token = authorization
        .strip_prefix("Bearer ")
        .unwrap_or(&authorization)
        .to_string();
    match path.as_str() {
        "/headers" => {
            let json = serde_json::to_string(&headers).unwrap();
            Response::new(full(json))
        }
        "/reflect-header" => {
            let mut response = Response::new(full("nothing to see"));
            response
                .headers_mut()
                .insert("x-echo", HeaderValue::from_str(&authorization).unwrap());
            response
        }
        "/reflect-split" => {
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, BoxError>>(4);
            let (first, second) = token.split_at(token.len().min(5));
            let first = format!("prefix-{first}");
            let second = format!("{second}-suffix");
            tokio::spawn(async move {
                let _ = tx.send(Ok(Frame::data(Bytes::from(first)))).await;
                tokio::time::sleep(Duration::from_millis(150)).await;
                let _ = tx.send(Ok(Frame::data(Bytes::from(second)))).await;
            });
            Response::new(StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx)).boxed())
        }
        "/gzip" => {
            let mut response = Response::new(full(vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3]));
            response
                .headers_mut()
                .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            response
        }
        "/reflect-reason" => {
            let mut response = Response::new(full("nothing to see"));
            response
                .extensions_mut()
                .insert(hyper::ext::ReasonPhrase::try_from(format!("OK {authorization}")).unwrap());
            response
        }
        "/custom-reason" => {
            let mut response = Response::new(full("ok"));
            response
                .extensions_mut()
                .insert(hyper::ext::ReasonPhrase::from_static(b"Fine Thanks"));
            response
        }
        "/big" => Response::new(full(vec![b'z'; 1024 * 1024])),
        "/upload" => Response::new(full(format!("received {}", body.len()))),
        _ => Response::new(full("ok")),
    }
}

async fn start_upstream() -> TestUpstream {
    let ca = SessionCa::new("upstream-test-ca").unwrap();
    let (cert, key) = ca.leaf(HOST).unwrap();
    let mut config = ServerConfig::builder_with_provider(crate::egress_broker::crypto_provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_by_server = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let seen = seen_by_server.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let service = hyper::service::service_fn(move |request| {
                    let seen = seen.clone();
                    async move { Ok::<_, Infallible>(upstream_handler(seen, request).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    });
    TestUpstream { addr, ca, seen }
}

// ---------------------------------------------------------------- record

#[derive(Default)]
struct MemorySink {
    events: Mutex<Vec<NetworkEvent>>,
    fail_requests: bool,
    /// When set, an allowed `request` waits for it before it is written.
    gate: Option<Arc<Notify>>,
    waiting: AtomicBool,
}

impl MemorySink {
    fn events(&self) -> Vec<NetworkEvent> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl BrokerRecordSink for MemorySink {
    async fn append(&self, event: NetworkEvent) -> Result<(), String> {
        event.validate().map_err(|error| error.to_string())?;
        if matches!(
            event,
            NetworkEvent::Request {
                decision: Decision::Allow,
                ..
            }
        ) {
            if self.fail_requests {
                return Err("the record is full".into());
            }
            if let Some(gate) = &self.gate {
                self.waiting.store(true, Ordering::SeqCst);
                gate.notified().await;
            }
        }
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

/// Wait until `sink` holds a `response` event (the recorder writes it after
/// the body ends).
async fn wait_for_response_event(sink: &MemorySink) -> NetworkEvent {
    for _ in 0..200 {
        if let Some(event) = sink
            .events()
            .into_iter()
            .rev()
            .find(|event| matches!(event, NetworkEvent::Response { .. }))
        {
            return event;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no response event: {:#?}", sink.events());
}

// ---------------------------------------------------------------- harness

struct Harness {
    upstream: TestUpstream,
    broker: Arc<SessionBroker>,
    route: Arc<Route>,
    port: u16,
    addrs: Vec<IpAddr>,
}

fn route(yaml: &str) -> Arc<Route> {
    let parsed: EgressRouteYaml = serde_yaml::from_str(yaml).unwrap();
    let mut credentials = BTreeMap::new();
    credentials.insert(
        "test".to_string(),
        CredentialSourceYaml {
            env: Some(SECRET_ENV.into()),
            file: None,
        },
    );
    credentials.insert(
        "missing".to_string(),
        CredentialSourceYaml {
            env: Some("AXOCOATL_TERMINATE_TEST_UNSET".into()),
            file: None,
        },
    );
    RouteTable::compile(&[parsed], &credentials, &[])
        .unwrap()
        .routes()[0]
        .clone()
}

const RULES: &str = r#"
rules:
  - {methods: [GET, HEAD], path: /echo}
  - {methods: [GET], path: /headers}
  - {methods: [GET], path: /reflect-split}
  - {methods: [GET], path: /reflect-header}
  - {methods: [GET], path: /gzip}
  - {methods: [GET], path: /big}
  - {methods: [POST], path: /upload}
  - {methods: [GET], path: /reflect-reason}
  - {methods: [GET], path: /custom-reason}
"#;

fn bearer_route() -> Arc<Route> {
    route(&format!(
        "host: {HOST}\ncredential: test\ninject: {{header: Authorization, format: \"Bearer {{}}\"}}\nmax_request_bytes: 2097152\n{RULES}"
    ))
}

fn trusting(ca: &SessionCa) -> Arc<dyn rustls::client::danger::ServerCertVerifier> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der().clone()).unwrap();
    rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        crate::egress_broker::crypto_provider(),
    )
    .build()
    .unwrap()
}

/// `route` with `port` added to its ports: the test upstream listens on a
/// random port, and the broker serves a connection only on the route's ports.
fn covering(route: &Route, port: u16) -> Arc<Route> {
    let mut route = route.clone();
    if !route.ports.contains(&port) {
        route.ports.push(port);
    }
    Arc::new(route)
}

async fn harness_with(route: Arc<Route>, local: LocalCheck) -> Harness {
    secret();
    let upstream = start_upstream().await;
    let connector = UpstreamConnector::with_verifier(trusting(&upstream.ca), local);
    let broker = Arc::new(SessionBroker::new(
        Arc::new(SessionCa::new("ses-terminate").unwrap()),
        Arc::new(connector),
        Arc::new(Vec::new),
    ));
    let port = upstream.addr.port();
    Harness {
        port,
        addrs: vec![upstream.addr.ip()],
        upstream,
        broker,
        route: covering(&route, port),
    }
}

async fn harness(route: Arc<Route>) -> Harness {
    harness_with(route, Arc::new(|_| false)).await
}

struct Client {
    sender: SendRequest<TestBody>,
    served: tokio::task::JoinHandle<BrokerOutcome>,
}

impl Harness {
    fn context(&self) -> RelayContext {
        RelayContext {
            session: "ses-terminate".into(),
            conn: "g1:1".into(),
            route: self.route.clone(),
            host: HOST.into(),
            port: self.port,
            addrs: self.addrs.clone(),
            binding: Some(EgressBinding::new(BindingKind::Agent)),
            token_tag: Some("0123456789abcdef".into()),
        }
    }

    /// Start the broker on one side of a duplex pipe and a TLS client on
    /// the other, trusting only the Session authority.
    async fn tls(
        &self,
        sink: Arc<MemorySink>,
        server_name: &str,
        alpn: Vec<Vec<u8>>,
    ) -> (
        Result<tokio_rustls::client::TlsStream<tokio::io::DuplexStream>, std::io::Error>,
        tokio::task::JoinHandle<BrokerOutcome>,
    ) {
        self.tls_with(self.context(), sink, server_name, alpn).await
    }

    async fn tls_with(
        &self,
        context: RelayContext,
        sink: Arc<MemorySink>,
        server_name: &str,
        alpn: Vec<Vec<u8>>,
    ) -> (
        Result<tokio_rustls::client::TlsStream<tokio::io::DuplexStream>, std::io::Error>,
        tokio::task::JoinHandle<BrokerOutcome>,
    ) {
        let (client_io, broker_io) = tokio::io::duplex(256 * 1024);
        let broker = self.broker.clone();
        let served = tokio::spawn(async move { broker.serve(context, broker_io, sink).await });
        let mut roots = rustls::RootCertStore::empty();
        roots.add(self.broker.ca().der().clone()).unwrap();
        let mut config =
            rustls::ClientConfig::builder_with_provider(crate::egress_broker::crypto_provider())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
        config.alpn_protocols = alpn;
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let name = ServerName::try_from(server_name.to_string()).unwrap();
        (connector.connect(name, client_io).await, served)
    }

    async fn client(&self, sink: Arc<MemorySink>) -> Client {
        self.client_with(self.context(), sink).await
    }

    async fn client_with(&self, context: RelayContext, sink: Arc<MemorySink>) -> Client {
        let (tls, served) = self
            .tls_with(context, sink, HOST, vec![b"http/1.1".to_vec()])
            .await;
        let (sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(tls.unwrap()))
                .await
                .unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Client { sender, served }
    }

    fn host_header(&self) -> String {
        format!("{HOST}:{}", self.port)
    }
}

fn get(path: &str, host: &str) -> Request<TestBody> {
    Request::builder()
        .method("GET")
        .uri(path)
        .header(header::HOST, host)
        .body(full(Bytes::new()))
        .unwrap()
}

async fn body_text(response: Response<Incoming>) -> String {
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

async fn refusal_json(response: Response<Incoming>) -> serde_json::Value {
    serde_json::from_str(&body_text(response).await).unwrap()
}

fn request_events(sink: &MemorySink) -> Vec<NetworkEvent> {
    sink.events()
        .into_iter()
        .filter(|event| matches!(event, NetworkEvent::Request { .. }))
        .collect()
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn an_allowed_request_carries_the_configured_credential_and_not_the_clients() {
    let h = harness(bearer_route()).await;
    let sink = Arc::new(MemorySink::default());
    let mut client = h.client(sink.clone()).await;
    let mut request = get("/echo?x=1", &h.host_header());
    let headers = request.headers_mut();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer clients-own"),
    );
    headers.insert(
        header::PROXY_AUTHORIZATION,
        HeaderValue::from_static("Basic proxy"),
    );
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("gzip, br"),
    );
    headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-3"));
    headers.insert("x-custom", HeaderValue::from_static("kept"));
    let response = client.sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(body_text(response).await, "ok");

    let seen = h.upstream.seen();
    assert_eq!(seen.len(), 1);
    let upstream = &seen[0];
    assert_eq!(
        (upstream.method.as_str(), upstream.path.as_str()),
        ("GET", "/echo")
    );
    assert_eq!(
        upstream.header("authorization"),
        vec![format!("Bearer {}", secret()).as_str()]
    );
    assert!(upstream.header("proxy-authorization").is_empty());
    assert_eq!(upstream.header("accept-encoding"), vec!["identity"]);
    assert!(upstream.header("range").is_empty());
    assert_eq!(upstream.header("x-custom"), vec!["kept"]);
    assert_eq!(upstream.header("host"), vec![h.host_header().as_str()]);

    let response = wait_for_response_event(&sink).await;
    let events = sink.events();
    assert_eq!(
        events[0],
        NetworkEvent::Request {
            conn: "g1:1".into(),
            seq_in_conn: 1,
            method: "GET".into(),
            path: "/echo".into(),
            host: HOST.into(),
            rule: Some("route#0.rules[0]".into()),
            decision: Decision::Allow,
            reason: None,
            credential: Some("test".into()),
        }
    );
    let NetworkEvent::Response {
        seq_in_conn,
        status,
        down,
        outcome,
        ..
    } = response
    else {
        unreachable!()
    };
    assert_eq!(
        (seq_in_conn, status, down, outcome),
        (1, 200, 2, ResponseOutcome::Completed)
    );

    // A second request on the same connection; HEAD completes too.
    let mut head = get("/echo", HOST);
    *head.method_mut() = Method::HEAD;
    let response = client.sender.send_request(head).await.unwrap();
    assert_eq!(response.status(), 200);
    drop(response);
    drop(client.sender);
    let outcome = client.served.await.unwrap();
    assert_eq!(outcome.requests, 2);
    assert_eq!(outcome.error, None);
    let events = sink.events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            NetworkEvent::Response {
                seq_in_conn: 2,
                outcome: ResponseOutcome::Completed,
                ..
            }
        )),
        "{events:#?}"
    );
}

#[tokio::test]
async fn basic_credentials_are_sent_as_username_and_value() {
    let h = harness(route(&format!(
        "host: {HOST}\ncredential: test\ninject: {{basic: {{username: {USERNAME}}}}}\n{RULES}"
    )))
    .await;
    let sink = Arc::new(MemorySink::default());
    let mut client = h.client(sink.clone()).await;
    let response = client
        .sender
        .send_request(get("/echo", HOST))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let seen = h.upstream.seen();
    assert_eq!(
        seen[0].header("authorization"),
        vec![format!("Basic {}", basic_form()).as_str()]
    );
}

#[tokio::test]
async fn a_refused_rule_gets_a_403_and_never_reaches_the_upstream() {
    let h = harness(bearer_route()).await;
    let sink = Arc::new(MemorySink::default());
    let mut client = h.client(sink.clone()).await;
    let mut request = get("/echo", HOST);
    *request.method_mut() = Method::DELETE;
    let response = client.sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(
        response.headers()["x-axocoatl-egress"],
        "denied; reason=route_denied"
    );
    let json = refusal_json(response).await;
    assert_eq!(json["error"], "route_denied");
    assert_eq!(json["host"], HOST);
    assert_eq!(json["method"], "DELETE");
    assert_eq!(json["path"], "/echo");
    assert!(json["reason"]
        .as_str()
        .unwrap()
        .contains("allows DELETE /echo"));
    assert!(json["hint"].as_str().unwrap().contains("methods: [DELETE]"));
    assert!(h.upstream.seen().is_empty());
    assert!(matches!(
        &request_events(&sink)[0],
        NetworkEvent::Request { decision: Decision::Deny, reason: Some(reason), credential: None, .. }
            if reason == "route_denied"
    ));
    // The connection stays usable for allowed requests.
    let response = client
        .sender
        .send_request(get("/echo", HOST))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(h.upstream.seen().len(), 1);
}

#[tokio::test]
async fn a_wrong_server_name_or_host_is_refused() {
    let h = harness(bearer_route()).await;

    // TLS for another name on the route's connection.
    let sink = Arc::new(MemorySink::default());
    let (tls, served) = h
        .tls(sink.clone(), "other.test", vec![b"http/1.1".to_vec()])
        .await;
    assert!(tls.is_err());
    let outcome = served.await.unwrap();
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap()
            .starts_with("sni_mismatch"),
        "{outcome:?}"
    );
    assert!(outcome.error.unwrap().contains("other.test"));

    // No server name at all (an IP address).
    let (tls, served) = h.tls(sink.clone(), "127.0.0.1", vec![]).await;
    assert!(tls.is_err());
    assert!(served
        .await
        .unwrap()
        .error
        .unwrap()
        .starts_with("sni_mismatch"));

    // HTTP/2 only.
    let (tls, served) = h.tls(sink.clone(), HOST, vec![b"h2".to_vec()]).await;
    assert!(tls.is_err());
    assert!(served
        .await
        .unwrap()
        .error
        .unwrap()
        .starts_with("alpn_refused"));

    // The right name, another Host.
    let mut client = h.client(sink.clone()).await;
    for host in [
        "other.test",
        "api.test.evil.example",
        "api.test:1",
        "api.test:0443x",
    ] {
        let response = client
            .sender
            .send_request(get("/echo", host))
            .await
            .unwrap();
        assert_eq!(response.status(), 421, "{host}");
        let json = refusal_json(response).await;
        assert_eq!(json["error"], "host_mismatch");
        // The refusal closes the connection.
        client = h.client(sink.clone()).await;
    }
    assert!(h.upstream.seen().is_empty());
    assert!(request_events(&sink).iter().any(|event| matches!(
        event,
        NetworkEvent::Request { host, reason: Some(reason), .. }
            if host == "other.test" && reason == "host_mismatch"
    )));
    // Default port spelled out, and a trailing dot, are the route host.
    let response = client
        .sender
        .send_request(get("/echo", &format!("API.test.:{}", h.port)))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn upgrades_and_non_canonical_paths_are_refused() {
    let h = harness(bearer_route()).await;
    let sink = Arc::new(MemorySink::default());
    let mut client = h.client(sink.clone()).await;
    let mut request = get("/echo", HOST);
    request
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    request
        .headers_mut()
        .insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    let response = client.sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(refusal_json(response).await["error"], "upgrade_not_allowed");

    for path in [
        "/echo/../headers",
        "/%2e%2e/echo",
        "//echo",
        "/echo%2F..%2Fheaders",
        "/headers/..;/echo",
        "/headers/.;/echo",
        "/headers/;/echo",
        "/headers/..;x=1/echo",
        "/headers/%252e%252e/echo",
    ] {
        let mut client = h.client(sink.clone()).await;
        let response = client.sender.send_request(get(path, HOST)).await.unwrap();
        assert_eq!(response.status(), 400, "{path}");
        assert_eq!(refusal_json(response).await["error"], "path_not_canonical");
    }
    assert!(h.upstream.seen().is_empty());
}

#[tokio::test]
async fn headers_that_name_another_method_path_or_host_are_refused() {
    let overrides = [
        ("X-HTTP-Method-Override", "DELETE"),
        ("X-HTTP-Method", "DELETE"),
        ("X-Method-Override", "DELETE"),
        ("X-Original-Method", "DELETE"),
        ("X-Original-URL", "/admin"),
        ("X-Original-URI", "/admin"),
        ("X-Rewrite-URL", "/admin"),
        ("X-Original-Host", "other.test"),
        ("X-Host", "other.test"),
        ("X-Forwarded-Host", "other.test"),
        ("X-Forwarded-Prefix", "/admin"),
        ("X-Forwarded-For", "203.0.113.7"),
        ("Forwarded", "host=other.test"),
    ];
    // Rules apply on every route, credentialed or not.
    for route_yaml in [
        format!(
            "host: {HOST}\ncredential: test\ninject: {{header: Authorization, format: \"Bearer {{}}\"}}\n{RULES}"
        ),
        format!("host: {HOST}\n{RULES}"),
    ] {
        let h = harness(route(&route_yaml)).await;
        let sink = Arc::new(MemorySink::default());
        let mut client = h.client(sink.clone()).await;
        for (name, value) in overrides {
            let mut request = get("/echo", HOST);
            request
                .headers_mut()
                .insert(HeaderName::from_bytes(name.as_bytes()).unwrap(), HeaderValue::from_static(value));
            let response = client.sender.send_request(request).await.unwrap();
            assert_eq!(response.status(), 403, "{name}");
            assert_eq!(
                response.headers()["x-axocoatl-egress"],
                "denied; reason=override_header"
            );
            let json = refusal_json(response).await;
            assert_eq!(json["error"], "override_header", "{name}");
            assert!(
                json["reason"]
                    .as_str()
                    .unwrap()
                    .contains(&name.to_ascii_lowercase()),
                "{json}"
            );
        }
        assert!(h.upstream.seen().is_empty(), "{route_yaml}");
        let refused = request_events(&sink);
        assert_eq!(refused.len(), overrides.len());
        assert!(refused.iter().all(|event| matches!(
            event,
            NetworkEvent::Request { decision: Decision::Deny, reason: Some(reason), .. }
                if reason == "override_header"
        )));
        // The connection stays usable, and an ordinary request goes through
        // without any of them.
        let response = client
            .sender
            .send_request(get("/echo", HOST))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let seen = h.upstream.seen();
        assert_eq!(seen.len(), 1);
        for (name, _) in overrides {
            assert!(seen[0].header(&name.to_ascii_lowercase()).is_empty(), "{name}");
        }
    }
}

#[tokio::test]
async fn a_credential_in_the_reason_phrase_is_stopped() {
    let h = harness(bearer_route()).await;
    let sink = Arc::new(MemorySink::default());
    let mut client = h.client(sink.clone()).await;
    let response = client
        .sender
        .send_request(get("/reflect-reason", HOST))
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    assert!(response
        .extensions()
        .get::<hyper::ext::ReasonPhrase>()
        .is_none_or(|reason| !String::from_utf8_lossy(reason.as_bytes()).contains(secret())));
    let json = refusal_json(response).await;
    assert_eq!(json["error"], "credential_reflected");
    assert!(!json.to_string().contains(secret()));
    drop(client.sender);
    assert_eq!(
        client.served.await.unwrap().error.as_deref(),
        Some("credential_reflected")
    );
    assert!(matches!(
        wait_for_response_event(&sink).await,
        NetworkEvent::Response {
            outcome: ResponseOutcome::CredentialReflected,
            ..
        }
    ));
    // A reason phrase without the credential still reaches the client.
    let mut client = h.client(Arc::new(MemorySink::default())).await;
    let response = client
        .sender
        .send_request(get("/custom-reason", HOST))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .extensions()
            .get::<hyper::ext::ReasonPhrase>()
            .map(|reason| reason.as_bytes().to_vec()),
        Some(b"Fine Thanks".to_vec())
    );
}

#[tokio::test]
async fn a_connection_without_a_process_kind_or_on_another_port_is_refused() {
    let h = harness(bearer_route()).await;
    let sink = Arc::new(MemorySink::default());
    let mut context = h.context();
    context.binding = None;
    let mut client = h.client_with(context, sink.clone()).await;
    let response = client
        .sender
        .send_request(get("/echo", HOST))
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(
        refusal_json(response).await["error"],
        "route_not_for_binding"
    );
    assert!(h.upstream.seen().is_empty());
    assert!(matches!(
        &request_events(&sink)[0],
        NetworkEvent::Request { decision: Decision::Deny, reason: Some(reason), credential: None, .. }
            if reason == "route_not_for_binding"
    ));

    // A port the route does not cover is never served with it.
    let (_client_io, broker_io) = tokio::io::duplex(1024);
    let mut context = h.context();
    context.port = 1;
    assert!(!h.route.covers_port(context.port));
    let outcome = h
        .broker
        .serve(context, broker_io, Arc::new(MemorySink::default()))
        .await;
    let error = outcome.error.unwrap();
    assert!(error.starts_with("route_mismatch"), "{error}");
    assert!(error.contains("port"), "{error}");
}

#[tokio::test]
async fn a_request_is_recorded_before_the_upstream_sees_it() {
    let h = harness(bearer_route()).await;
    let gate = Arc::new(Notify::new());
    let sink = Arc::new(MemorySink {
        gate: Some(gate.clone()),
        ..MemorySink::default()
    });
    let mut client = h.client(sink.clone()).await;
    let pending = tokio::spawn(client.sender.send_request(get("/echo", HOST)));
    for _ in 0..200 {
        if sink.waiting.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        sink.waiting.load(Ordering::SeqCst),
        "the request was never recorded"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        h.upstream.seen().is_empty(),
        "the upstream saw the request before its record was written"
    );
    assert!(!pending.is_finished());
    gate.notify_one();
    let response = pending.await.unwrap().unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(h.upstream.seen().len(), 1);
    assert!(matches!(
        sink.events()[0],
        NetworkEvent::Request {
            decision: Decision::Allow,
            ..
        }
    ));
}

#[tokio::test]
async fn a_request_that_cannot_be_recorded_is_refused() {
    let h = harness(bearer_route()).await;
    let sink = Arc::new(MemorySink {
        fail_requests: true,
        ..MemorySink::default()
    });
    let mut client = h.client(sink.clone()).await;
    let response = client
        .sender
        .send_request(get("/echo", HOST))
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(refusal_json(response).await["error"], "record_unavailable");
    assert!(h.upstream.seen().is_empty());
    drop(client.sender);
    assert_eq!(
        client.served.await.unwrap().error.as_deref(),
        Some("record_unavailable")
    );
}

#[tokio::test]
async fn a_reflected_credential_split_across_chunks_never_reaches_the_client() {
    let h = harness(bearer_route()).await;
    let sink = Arc::new(MemorySink::default());
    let mut client = h.client(sink.clone()).await;
    let response = client
        .sender
        .send_request(get("/reflect-split", HOST))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut body = response.into_body();
    let mut received = Vec::new();
    let mut failed = false;
    while let Some(frame) = body.frame().await {
        match frame {
            Ok(frame) => {
                if let Some(data) = frame.data_ref() {
                    received.extend_from_slice(data);
                }
            }
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    let received = String::from_utf8(received).unwrap();
    assert!(failed, "the body ended normally: {received:?}");
    assert!(
        "prefix-".starts_with(&received),
        "bytes of the credential reached the client: {received:?}"
    );
    assert!(!received.contains(&secret()[..5]));
    let outcome = client.served.await.unwrap();
    assert_eq!(outcome.error.as_deref(), Some("credential_reflected"));
    let event = wait_for_response_event(&sink).await;
    assert!(matches!(
        event,
        NetworkEvent::Response {
            outcome: ResponseOutcome::CredentialReflected,
            status: 200,
            ..
        }
    ));
}

#[tokio::test]
async fn a_credential_echoed_in_a_header_or_whole_body_is_stopped() {
    let h = harness(bearer_route()).await;
    let sink = Arc::new(MemorySink::default());
    let mut client = h.client(sink.clone()).await;
    let response = client
        .sender
        .send_request(get("/reflect-header", HOST))
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    assert!(response
        .headers()
        .iter()
        .all(|(_, value)| !String::from_utf8_lossy(value.as_bytes()).contains(secret())));
    let json = refusal_json(response).await;
    assert_eq!(json["error"], "credential_reflected");
    assert!(!json.to_string().contains(secret()));

    // The whole body is one chunk: the broker stops before sending
    // anything, so the client sees the connection end or a cut body.
    let mut client = h.client(sink.clone()).await;
    if let Ok(response) = client.sender.send_request(get("/headers", HOST)).await {
        if let Ok(bytes) = response.into_body().collect().await {
            assert!(
                !String::from_utf8_lossy(&bytes.to_bytes()).contains(secret()),
                "the echoed credential reached the client"
            );
        }
    }
    assert_eq!(
        client.served.await.unwrap().error.as_deref(),
        Some("credential_reflected")
    );
    // Basic credentials are also caught in their base64 form.
    let h = harness(route(&format!(
        "host: {HOST}\ncredential: test\ninject: {{basic: {{username: {USERNAME}}}}}\n{RULES}"
    )))
    .await;
    let mut client = h.client(sink.clone()).await;
    let response = client
        .sender
        .send_request(get("/reflect-header", HOST))
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    let mut client = h.client(sink.clone()).await;
    if let Ok(response) = client.sender.send_request(get("/headers", HOST)).await {
        if let Ok(bytes) = response.into_body().collect().await {
            let text = String::from_utf8_lossy(&bytes.to_bytes()).into_owned();
            assert!(
                !text.contains(&basic_form()) && !text.contains(secret()),
                "{text}"
            );
        }
    }
    assert_eq!(
        client.served.await.unwrap().error.as_deref(),
        Some("credential_reflected")
    );
}

#[tokio::test]
async fn compressed_responses_are_refused_on_credentialed_routes_only() {
    let h = harness(bearer_route()).await;
    let sink = Arc::new(MemorySink::default());
    let mut client = h.client(sink.clone()).await;
    let response = client
        .sender
        .send_request(get("/gzip", HOST))
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    assert_eq!(refusal_json(response).await["error"], "encoded_response");
    assert_eq!(
        h.upstream.seen()[0].header("accept-encoding"),
        vec!["identity"]
    );
    assert!(matches!(
        wait_for_response_event(&sink).await,
        NetworkEvent::Response {
            outcome: ResponseOutcome::EncodedResponse,
            status: 502,
            ..
        }
    ));

    for allowed in [
        format!("host: {HOST}\n{RULES}"),
        format!(
            "host: {HOST}\ncredential: test\ninject: {{header: X-Api-Key}}\nallow_encoded_responses: true\n{RULES}"
        ),
    ] {
        let h = harness(route(&allowed)).await;
        let sink = Arc::new(MemorySink::default());
        let mut client = h.client(sink.clone()).await;
        let mut request = get("/gzip", HOST);
        request
            .headers_mut()
            .insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
        let response = client.sender.send_request(request).await.unwrap();
        assert_eq!(response.status(), 200, "{allowed}");
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
    }
}

#[tokio::test]
async fn bodies_stream_both_ways_and_oversized_uploads_are_refused() {
    let h = harness(bearer_route()).await;
    let sink = Arc::new(MemorySink::default());
    let mut client = h.client(sink.clone()).await;
    let upload = Request::builder()
        .method("POST")
        .uri("/upload")
        .header(header::HOST, HOST)
        .body(full(vec![b'u'; 1024 * 1024]))
        .unwrap();
    let response = client.sender.send_request(upload).await.unwrap();
    assert_eq!(body_text(response).await, "received 1048576");
    assert_eq!(h.upstream.seen()[0].body_len, 1024 * 1024);
    let response = client.sender.send_request(get("/big", HOST)).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(bytes.len(), 1024 * 1024);
    drop(client.sender);
    let outcome = client.served.await.unwrap();
    assert_eq!((outcome.up, outcome.down), (1024 * 1024, 1024 * 1024 + 16));

    // Declared too large: refused before anything goes upstream.
    let small = harness(route(&format!(
        "host: {HOST}\ncredential: test\ninject: {{header: X-Api-Key}}\nmax_request_bytes: 1024\n{RULES}"
    )))
    .await;
    let sink = Arc::new(MemorySink::default());
    let mut client = small.client(sink.clone()).await;
    let upload = Request::builder()
        .method("POST")
        .uri("/upload")
        .header(header::HOST, HOST)
        .body(full(vec![b'u'; 4096]))
        .unwrap();
    let response = client.sender.send_request(upload).await.unwrap();
    assert_eq!(response.status(), 413);
    assert_eq!(refusal_json(response).await["error"], "request_too_large");
    assert!(small.upstream.seen().is_empty());

    // Chunked and too large: cut off while streaming.
    let mut client = small.client(sink.clone()).await;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, BoxError>>(16);
    tokio::spawn(async move {
        for _ in 0..8 {
            if tx
                .send(Ok(Frame::data(Bytes::from(vec![b'c'; 1024]))))
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    let upload = Request::builder()
        .method("POST")
        .uri("/upload")
        .header(header::HOST, HOST)
        .body(StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx)).boxed())
        .unwrap();
    let response = client.sender.send_request(upload).await.unwrap();
    assert_eq!(response.status(), 413);
    assert!(matches!(
        wait_for_response_event(&sink).await,
        NetworkEvent::Response {
            outcome: ResponseOutcome::TooLarge,
            status: 413,
            ..
        }
    ));
    assert!(small
        .upstream
        .seen()
        .iter()
        .all(|seen| seen.body_len <= 1024));
}

#[tokio::test]
async fn unreachable_upstreams_own_addresses_and_missing_credentials_are_answered() {
    // The own-address check refuses every address.
    let h = harness_with(bearer_route(), Arc::new(|_| true)).await;
    let sink = Arc::new(MemorySink::default());
    let mut client = h.client(sink.clone()).await;
    let response = client
        .sender
        .send_request(get("/echo", HOST))
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    let json = refusal_json(response).await;
    assert_eq!(json["error"], "upstream_failed");
    assert!(json["reason"]
        .as_str()
        .unwrap()
        .contains("is this computer"));
    assert!(h.upstream.seen().is_empty());
    assert!(matches!(
        wait_for_response_event(&sink).await,
        NetworkEvent::Response {
            outcome: ResponseOutcome::UpstreamFailed,
            status: 502,
            ..
        }
    ));

    // Nothing listens on the port.
    let mut h = harness(bearer_route()).await;
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    h.port = closed.local_addr().unwrap().port();
    h.route = covering(&h.route, h.port);
    drop(closed);
    let mut client = h.client(Arc::new(MemorySink::default())).await;
    let response = client
        .sender
        .send_request(get("/echo", HOST))
        .await
        .unwrap();
    assert_eq!(response.status(), 502);

    // The credential's variable is not set.
    let h = harness(route(&format!(
        "host: {HOST}\ncredential: missing\ninject: {{header: X-Api-Key}}\n{RULES}"
    )))
    .await;
    let sink = Arc::new(MemorySink::default());
    let mut client = h.client(sink.clone()).await;
    let response = client
        .sender
        .send_request(get("/echo", HOST))
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let json = refusal_json(response).await;
    assert_eq!(json["error"], "credential_unavailable");
    assert!(!json.to_string().contains("AXOCOATL_TERMINATE_TEST_UNSET"));
    assert!(h.upstream.seen().is_empty());

    // A binding the route does not serve.
    let h = harness(bearer_route()).await;
    let (client_io, broker_io) = tokio::io::duplex(64 * 1024);
    let mut context = h.context();
    context.binding = Some(EgressBinding::new(BindingKind::Setup));
    let broker = h.broker.clone();
    let sink = Arc::new(MemorySink::default());
    tokio::spawn(async move { broker.serve(context, broker_io, sink).await });
    let mut roots = rustls::RootCertStore::empty();
    roots.add(h.broker.ca().der().clone()).unwrap();
    let config =
        rustls::ClientConfig::builder_with_provider(crate::egress_broker::crypto_provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from(HOST).unwrap(), client_io)
        .await
        .unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    tokio::spawn(connection);
    let response = sender.send_request(get("/echo", HOST)).await.unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(
        refusal_json(response).await["error"],
        "route_not_for_binding"
    );

    // A connection to another host is never served with this route.
    let (_client_io, broker_io) = tokio::io::duplex(1024);
    let mut context = h.context();
    context.host = "other.test".into();
    let outcome = h
        .broker
        .serve(context, broker_io, Arc::new(MemorySink::default()))
        .await;
    assert!(outcome.error.unwrap().starts_with("route_mismatch"));
}

/// The production verifier (this computer's trust settings) with the
/// route's `upstream_ca` added: the local upstream's own authority.
#[cfg(unix)]
#[tokio::test]
async fn an_upstream_ca_is_trusted_through_the_platform_verifier() {
    use std::os::unix::fs::PermissionsExt;
    secret();
    let upstream = start_upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let ca_path = dir.path().join("upstream-ca.pem");
    std::fs::write(&ca_path, upstream.ca.pem()).unwrap();
    std::fs::set_permissions(&ca_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let with_ca = route(&format!(
        "host: {HOST}\nupstream_ca: '{}'\n{RULES}",
        ca_path.display()
    ));
    let broker = Arc::new(SessionBroker::new(
        Arc::new(SessionCa::new("ses-platform").unwrap()),
        Arc::new(UpstreamConnector::with_local_check(Arc::new(|_| false))),
        Arc::new(Vec::new),
    ));
    let port = upstream.addr.port();
    let h = Harness {
        port,
        addrs: vec![upstream.addr.ip()],
        upstream,
        broker: broker.clone(),
        route: covering(&with_ca, port),
    };
    let mut client = h.client(Arc::new(MemorySink::default())).await;
    let response = client
        .sender
        .send_request(get("/echo", HOST))
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{:?}", refusal_json(response).await);

    // Without the upstream's authority the platform verifier refuses it.
    let h = Harness {
        route: covering(&route(&format!("host: {HOST}\n{RULES}")), h.port),
        ..h
    };
    let mut client = h.client(Arc::new(MemorySink::default())).await;
    let response = client
        .sender
        .send_request(get("/echo", HOST))
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    let json = refusal_json(response).await;
    assert!(json["reason"].as_str().unwrap().contains("TLS"), "{json}");
}

/// Captures every `tracing` event of the test's thread.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn the_credential_is_never_in_logs_or_record_lines() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    // Other tests register the broker's callsites on other threads with no
    // subscriber; make their cached interest include this one.
    tracing::callsite::rebuild_interest_cache();

    let mut events = Vec::new();
    for inject in [
        "{header: Authorization, format: \"Bearer {}\"}".to_string(),
        format!("{{basic: {{username: {USERNAME}}}}}"),
    ] {
        let h = harness(route(&format!(
            "host: {HOST}\ncredential: test\ninject: {inject}\n{RULES}"
        )))
        .await;
        let sink = Arc::new(MemorySink::default());
        for (method, path) in [
            ("GET", "/echo"),
            ("DELETE", "/echo"),
            ("GET", "/gzip"),
            ("GET", "/reflect-header"),
            ("GET", "/headers"),
            ("GET", "/reflect-split"),
        ] {
            tracing::callsite::rebuild_interest_cache();
            let mut client = h.client(sink.clone()).await;
            let mut request = get(path, HOST);
            *request.method_mut() = Method::from_bytes(method.as_bytes()).unwrap();
            if let Ok(response) = client.sender.send_request(request).await {
                let _ = response.into_body().collect().await;
            }
            drop(client.sender);
            let _ = client.served.await;
        }
        events.extend(sink.events());
    }
    let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("route request allowed"), "{logs}");
    assert!(logs.contains("route request refused"), "{logs}");
    assert!(logs.contains("carried the credential"), "{logs}");
    let lines: Vec<String> = events
        .iter()
        .map(|event| serde_json::to_string(event).unwrap())
        .collect();
    assert!(lines
        .iter()
        .any(|line| line.contains("\"credential\":\"test\"")));
    for forbidden in [secret().to_string(), basic_form()] {
        assert!(
            !logs.contains(&forbidden),
            "a log line holds the credential"
        );
        assert!(
            lines.iter().all(|line| !line.contains(&forbidden)),
            "a record line holds the credential"
        );
    }
}
