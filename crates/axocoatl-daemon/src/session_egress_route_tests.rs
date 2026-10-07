//! Egress routes through the decision point: a `CONNECT` to a route's host
//! is answered with a relay, and the bytes the sidecar carries reach the
//! route broker, which ends TLS with the Session's own authority, checks the
//! request, adds the credential and sends it to the addresses the decision
//! point resolved. The sidecar here is a fake on the real control channel;
//! the client is rustls and hyper; the upstream is a local HTTPS server with
//! its own test authority.

use super::tests::{FakeRecord, FakeResolver};
use super::*;
use crate::egress_broker::upstream::UpstreamConnector;
use axocoatl_exec::egress::protocol::{
    base64_encode, decode_daemon, encode_sidecar, DaemonFrame, SidecarFrame, MAX_DATA_BYTES,
    RELAY_WINDOW_BYTES,
};
use axocoatl_isolation::egress::CloseOutcome as WireOutcome;
use axocoatl_isolation::egress_control::{self, ControlTiming};
use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{CertificateDer, ServerName};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::OnceLock;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

const HOST: &str = "api.test";
const SECRET_ENV: &str = "AXOCOATL_ROUTE_TEST_SECRET";

/// The route's credential: random per run, set once in this process.
pub(crate) fn secret() -> &'static str {
    static SECRET: OnceLock<String> = OnceLock::new();
    SECRET.get_or_init(|| {
        let mut bytes = [0u8; 18];
        getrandom::fill(&mut bytes).unwrap();
        let value = format!("axo-route-{}", hex::encode(bytes));
        std::env::set_var(SECRET_ENV, &value);
        value
    })
}

/// Loopback is an ordinary address here: the test upstream listens on it.
pub(crate) fn loopback_is_public(ip: IpAddr) -> AddrClass {
    if ip.is_loopback() {
        AddrClass::Public
    } else {
        netaddr::classify(ip)
    }
}

// ---------------------------------------------------------------- upstream

/// One request the upstream received.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) authorization: Vec<String>,
}

/// A local HTTPS server for `api.test` with its own authority.
pub(crate) struct Upstream {
    pub(crate) addr: SocketAddr,
    pub(crate) ca: SessionCa,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Upstream {
    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub(crate) async fn start(host: &str) -> Self {
        let ca = SessionCa::new("route-upstream-test").unwrap();
        let (cert, key) = ca.leaf(host).unwrap();
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
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
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let seen = seen_by_server.clone();
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                        let seen = seen.clone();
                        async move {
                            let authorization: Vec<String> = request
                                .headers()
                                .get_all(hyper::header::AUTHORIZATION)
                                .iter()
                                .map(|value| String::from_utf8_lossy(value.as_bytes()).into())
                                .collect();
                            seen.lock().unwrap().push(Seen {
                                method: request.method().to_string(),
                                path: request.uri().path().to_string(),
                                authorization,
                            });
                            Ok::<_, Infallible>(upstream_response(&request))
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tls), service)
                        .await;
                });
            }
        });
        Self { addr, ca, seen }
    }

    /// Trust only this upstream's authority.
    pub(crate) fn verifier(&self) -> Arc<dyn rustls::client::danger::ServerCertVerifier> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(self.ca.der().clone()).unwrap();
        rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap()
    }
}

fn pkt_line(line: &str) -> String {
    format!("{:04x}{line}", line.len() + 4)
}

/// A Git smart-HTTP ref advertisement for `.../info/refs?service=
/// git-upload-pack`, so `git ls-remote` works against the upstream; any
/// other request gets a short text body.
fn upstream_response(request: &Request<Incoming>) -> Response<Full<Bytes>> {
    let advertisement = request.uri().path().ends_with(".git/info/refs")
        && request.uri().query() == Some("service=git-upload-pack");
    if !advertisement {
        return Response::new(Full::new(Bytes::from_static(b"from upstream")));
    }
    let sha = "1111111111111111111111111111111111111111";
    let body = format!(
        "{}0000{}{}0000",
        pkt_line("# service=git-upload-pack\n"),
        pkt_line(&format!(
            "{sha} HEAD\0multi_ack thin-pack side-band side-band-64k ofs-delta shallow \
             no-progress include-tag symref=HEAD:refs/heads/main agent=git/axocoatl-test\n"
        )),
        pkt_line(&format!("{sha} refs/heads/main\n")),
    );
    let mut response = Response::new(Full::new(Bytes::from(body)));
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/x-git-upload-pack-advertisement"),
    );
    response.headers_mut().insert(
        hyper::header::CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-cache"),
    );
    response
}

// ---------------------------------------------------------------- sidecar

/// What the fake sidecar reports to a test about one connection.
enum Answer {
    Relay(tokio::io::DuplexStream),
    Other(DaemonFrame),
}

/// Bytes and ends for one relayed connection, from the daemon.
enum ToClient {
    Data(Vec<u8>),
    Eof,
    Credit(u32),
    Revoked,
}

/// A fake egress sidecar on the real control channel. It answers each
/// relayed connection with one end of an in-memory pipe and carries the
/// bytes both ways in `data` frames within the relay window, as the real
/// proxy does; when both directions end, or the connection is revoked, it
/// sends `close`.
pub(crate) struct RelaySidecar {
    out: mpsc::UnboundedSender<SidecarFrame>,
    answers: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Answer>>>>,
    revoked: Arc<Mutex<Vec<u64>>>,
    task: tokio::task::JoinHandle<egress_control::ControlEnd>,
}

impl Drop for RelaySidecar {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl RelaySidecar {
    pub(crate) async fn attach(egress: &Arc<SessionEgress>, generation: u32) -> Self {
        let (daemon_read, mut sidecar_write) = tokio::io::duplex(1 << 20);
        let (sidecar_read, daemon_write) = tokio::io::duplex(1 << 20);
        sidecar_write
            .write_all(
                &encode_sidecar(&SidecarFrame::Hello {
                    protocol: axocoatl_exec::egress::protocol::EGRESS_PROTOCOL_VERSION,
                    version: "test".into(),
                    max_connections: 8,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        let (handle, task) = egress_control::start(
            generation,
            daemon_read,
            daemon_write,
            egress.clone(),
            ControlTiming::default(),
        )
        .await
        .unwrap();
        egress.attach_control(handle);
        let (out, mut outgoing) = mpsc::unbounded_channel::<SidecarFrame>();
        tokio::spawn(async move {
            while let Some(frame) = outgoing.recv().await {
                if sidecar_write
                    .write_all(&encode_sidecar(&frame).unwrap())
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        let answers: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Answer>>>> =
            Arc::default();
        let revoked: Arc<Mutex<Vec<u64>>> = Arc::default();
        let relays: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<ToClient>>>> = Arc::default();
        let (reader_answers, reader_revoked, reader_out) =
            (answers.clone(), revoked.clone(), out.clone());
        tokio::spawn(async move {
            let mut reader = BufReader::new(sidecar_read);
            let mut acked = false;
            loop {
                let mut line = Vec::new();
                match reader.read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let frame = decode_daemon(&line).unwrap();
                let route = |id: u64, message: ToClient| {
                    if let Some(relay) = relays.lock().unwrap().get(&id) {
                        let _ = relay.send(message);
                    }
                };
                match frame {
                    DaemonFrame::HelloAck { .. } if !acked => acked = true,
                    DaemonFrame::Ping => {
                        let _ = reader_out.send(SidecarFrame::Pong);
                    }
                    DaemonFrame::Relay { id } => {
                        let (client, pump) = tokio::io::duplex(1 << 16);
                        let (to_client, from_daemon) = mpsc::unbounded_channel();
                        relays.lock().unwrap().insert(id, to_client);
                        tokio::spawn(pump_relay(id, pump, from_daemon, reader_out.clone()));
                        if let Some(answer) = reader_answers.lock().unwrap().remove(&id) {
                            let _ = answer.send(Answer::Relay(client));
                        }
                    }
                    DaemonFrame::Data { id, b } => route(
                        id,
                        ToClient::Data(
                            base64::engine::general_purpose::STANDARD.decode(b).unwrap(),
                        ),
                    ),
                    DaemonFrame::Eof { id } => route(id, ToClient::Eof),
                    DaemonFrame::Credit { id, bytes } => route(id, ToClient::Credit(bytes)),
                    DaemonFrame::Revoke { ids } => {
                        for id in ids {
                            reader_revoked.lock().unwrap().push(id);
                            route(id, ToClient::Revoked);
                        }
                    }
                    other => {
                        let id = match &other {
                            DaemonFrame::Allow { id, .. } | DaemonFrame::Deny { id, .. } => *id,
                            _ => continue,
                        };
                        if let Some(answer) = reader_answers.lock().unwrap().remove(&id) {
                            let _ = answer.send(Answer::Other(other));
                        }
                    }
                }
            }
        });
        Self {
            out,
            answers,
            revoked,
            task,
        }
    }

    /// Ask for `host:port` with the credential hash `auth`; the relay's
    /// client end, or the frame the daemon answered instead.
    pub(crate) async fn open(
        &self,
        id: u64,
        kind: RequestKind,
        host: &str,
        port: u16,
        auth: &str,
    ) -> Result<tokio::io::DuplexStream, DaemonFrame> {
        let (answer, answered) = tokio::sync::oneshot::channel();
        self.answers.lock().unwrap().insert(id, answer);
        self.out
            .send(SidecarFrame::Open {
                id,
                kind,
                host: host.into(),
                port,
                auth: Some(auth.into()),
                method: (kind == RequestKind::Http).then(|| "GET".into()),
                path: (kind == RequestKind::Http).then(|| "/".into()),
                peer: None,
            })
            .unwrap();
        match tokio::time::timeout(Duration::from_secs(10), answered)
            .await
            .unwrap()
            .unwrap()
        {
            Answer::Relay(client) => Ok(client),
            Answer::Other(frame) => Err(frame),
        }
    }

    pub(crate) fn revoked(&self) -> Vec<u64> {
        self.revoked.lock().unwrap().clone()
    }
}

/// Carry one relayed connection's bytes: the client's in `data` frames
/// within the daemon's credit, the daemon's to the client with credit back.
async fn pump_relay(
    id: u64,
    pump: tokio::io::DuplexStream,
    mut from_daemon: mpsc::UnboundedReceiver<ToClient>,
    out: mpsc::UnboundedSender<SidecarFrame>,
) {
    let started = std::time::Instant::now();
    let (mut read, mut write) = tokio::io::split(pump);
    let (mut up, mut down) = (0u64, 0u64);
    let mut credit = u64::from(RELAY_WINDOW_BYTES);
    let (mut client_done, mut daemon_done) = (false, false);
    let mut buffer = vec![0u8; MAX_DATA_BYTES];
    let outcome = loop {
        if client_done && daemon_done {
            break WireOutcome::Closed;
        }
        let room = credit.min(MAX_DATA_BYTES as u64) as usize;
        tokio::select! {
            read_result = read.read(&mut buffer[..room.max(1)]), if !client_done && room > 0 => {
                match read_result {
                    Ok(0) | Err(_) => {
                        client_done = true;
                        let _ = out.send(SidecarFrame::Eof { id });
                    }
                    Ok(count) => {
                        credit -= count as u64;
                        up += count as u64;
                        let _ = out.send(SidecarFrame::Data { id, b: base64_encode(&buffer[..count]) });
                    }
                }
            }
            message = from_daemon.recv() => match message {
                Some(ToClient::Data(bytes)) => {
                    if write.write_all(&bytes).await.is_err() {
                        break WireOutcome::Reset;
                    }
                    down += bytes.len() as u64;
                    let _ = out.send(SidecarFrame::Credit { id, bytes: bytes.len() as u32 });
                }
                Some(ToClient::Eof) => {
                    daemon_done = true;
                    let _ = write.shutdown().await;
                }
                Some(ToClient::Credit(bytes)) => credit += u64::from(bytes),
                Some(ToClient::Revoked) => break WireOutcome::Revoked,
                None => break WireOutcome::Interrupted,
            }
        }
    };
    drop(write);
    drop(read);
    let _ = out.send(SidecarFrame::Close {
        id,
        ip: None,
        up,
        down,
        ms: started.elapsed().as_millis() as u64,
        outcome,
        error: None,
    });
}

// ---------------------------------------------------------------- client

/// A TLS client over a relayed connection's pipe, trusting `ca` only.
pub(crate) async fn tls_client(
    pipe: tokio::io::DuplexStream,
    ca: &CertificateDer<'static>,
    server_name: &str,
) -> std::io::Result<tokio_rustls::client::TlsStream<tokio::io::DuplexStream>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.clone()).unwrap();
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from(server_name.to_string()).unwrap(), pipe)
        .await
}

pub(crate) async fn send(
    sender: &mut hyper::client::conn::http1::SendRequest<Empty<Bytes>>,
    path: &str,
    host: &str,
    authorization: Option<&str>,
) -> (StatusCode, String) {
    let mut request = Request::get(path).header(hyper::header::HOST, host);
    if let Some(value) = authorization {
        request = request.header(hyper::header::AUTHORIZATION, value);
    }
    let response = sender
        .send_request(request.body(Empty::new()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

// ---------------------------------------------------------------- fixture

pub(crate) fn route_yaml(port: u16, extra: &str) -> EgressRouteYaml {
    serde_yaml::from_str(&format!(
        "{{host: {HOST}, ports: [{port}], credential: test, \
          inject: {{header: Authorization, format: 'Bearer {{}}'}}, \
          for: [agent, setup], env_placeholders: [API_TOKEN], \
          rules: [{{methods: [GET], path: /allowed}}{extra}]}}"
    ))
    .unwrap()
}

pub(crate) fn credentials() -> BTreeMap<String, CredentialSourceYaml> {
    BTreeMap::from([(
        "test".to_string(),
        CredentialSourceYaml {
            env: Some(SECRET_ENV.into()),
            file: None,
        },
    )])
}

fn route_config(port: u16) -> EgressPolicyConfig {
    EgressPolicyConfig {
        session_allow: vec![EgressAllowYaml::Host(axocoatl_config::EgressHostYaml {
            host: "plain.test".into(),
            ports: None,
        })],
        routes: vec![route_yaml(port, "")],
        credentials: credentials(),
        ..EgressPolicyConfig::default()
    }
}

struct Fixture {
    egress: Arc<SessionEgress>,
    record: Arc<FakeRecord>,
    upstream: Upstream,
    _dir: tempfile::TempDir,
}

async fn fixture_with(config: impl FnOnce(u16) -> EgressPolicyConfig) -> Fixture {
    secret();
    let upstream = Upstream::start(HOST).await;
    let dir = tempfile::tempdir().unwrap();
    let record = Arc::new(FakeRecord::default());
    let egress = SessionEgress::open_session(
        "ses-route",
        config(upstream.addr.port()),
        record.clone(),
        FakeResolver::with(&[(HOST, &["127.0.0.1"]), ("plain.test", &["93.184.216.34"])]),
        Some(SecureDir::open(dir.path()).unwrap()),
        loopback_is_public,
        RouteSettings {
            upstream: Arc::new(UpstreamConnector::with_verifier(
                upstream.verifier(),
                Arc::new(|_| false),
            )),
            ..RouteSettings::default()
        },
    )
    .await
    .unwrap();
    Fixture {
        egress,
        record,
        upstream,
        _dir: dir,
    }
}

async fn fixture() -> Fixture {
    fixture_with(route_config).await
}

pub(crate) fn spec(kind: GrantKind, trust_mounted: bool) -> GrantSpec {
    GrantSpec {
        invocation_id: Some("inv-r".into()),
        activation_id: Some("act-r".into()),
        agent: Some("writer".into()),
        trust_mounted,
        ..GrantSpec::new(kind)
    }
}

/// A credential and its hash, and its env file's contents.
pub(crate) async fn grant(
    egress: &SessionEgress,
    spec: GrantSpec,
) -> (EgressGrant, String, String) {
    let grant = egress.grant(spec).await.unwrap();
    let contents = std::fs::read_to_string(grant.env_file.as_ref().unwrap()).unwrap();
    let token = contents
        .lines()
        .find_map(|line| line.strip_prefix("HTTPS_PROXY=http://axo:"))
        .unwrap()
        .trim_end_matches("@127.0.0.1:3128")
        .to_string();
    (grant, credential_hash(&token), contents)
}

/// Wait until the record holds an event `wanted` matches.
pub(crate) async fn recorded(
    record: &FakeRecord,
    wanted: impl Fn(&NetworkEvent) -> bool,
) -> NetworkEvent {
    for _ in 0..500 {
        if let Some(event) = record.events().into_iter().find(|event| wanted(event)) {
            return event;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("not recorded: {:#?}", record.events());
}

pub(crate) fn position(events: &[NetworkEvent], wanted: impl Fn(&NetworkEvent) -> bool) -> usize {
    events
        .iter()
        .position(wanted)
        .unwrap_or_else(|| panic!("{events:#?}"))
}

// ---------------------------------------------------------------- tests

/// The whole route path: the `CONNECT` is answered with a relay, TLS ends
/// here with a certificate from the Session's authority, an allowed request
/// reaches the upstream with the route's credential in place of the
/// client's, a request no rule allows is refused before it leaves, and every
/// request is recorded, the allowed one before the upstream sees it. The
/// credential appears nowhere in the record or the env file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_route_ends_tls_here_adds_the_credential_and_records_each_request() {
    let f = fixture().await;
    let port = f.upstream.addr.port();
    let (_grant, hash, env) = grant(&f.egress, spec(GrantKind::Agent, true)).await;
    assert!(!env.contains(secret()), "{env}");
    let sidecar = RelaySidecar::attach(&f.egress, 1).await;
    let pipe = sidecar
        .open(1, RequestKind::Connect, HOST, port, &hash)
        .await
        .unwrap();
    let ca = f.egress.authority_der().unwrap();
    let tls = tls_client(pipe, &ca, HOST).await.unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    let host = format!("{HOST}:{port}");
    let (status, body) = send(&mut sender, "/allowed", &host, Some("Bearer client-own")).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "from upstream"));
    let (status, body) = send(&mut sender, "/denied?x=1", &host, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let refusal: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(refusal["error"], "route_denied");
    assert_eq!(refusal["path"], "/denied");
    drop(sender);
    let _ = connection.await;

    // Only the allowed request left, with the route's credential alone.
    let seen = f.upstream.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(
        (seen[0].method.as_str(), seen[0].path.as_str()),
        ("GET", "/allowed")
    );
    assert_eq!(seen[0].authorization, [format!("Bearer {}", secret())]);

    recorded(
        &f.record,
        |event| matches!(event, NetworkEvent::Close { conn, .. } if conn == "g1:1"),
    )
    .await;
    let events = f.record.events();
    for event in &events {
        event.validate().unwrap();
        let line = serde_json::to_string(event).unwrap();
        assert!(!line.contains(secret()), "{line}");
    }
    let policy = position(&events, |event| {
        matches!(event, NetworkEvent::Policy { scope: EgressScope::Session, rules, .. }
            if rules.contains(&format!("{HOST}:{port} (route#0: 1 rule, credential test, for agent, setup)")))
    });
    let bind = position(&events, |event| matches!(event, NetworkEvent::Bind { .. }));
    let open = position(&events, |event| {
        matches!(event, NetworkEvent::Open {
            conn, decision: RecordDecision::Allow, rule: Some(rule), conn_kind: ConnKind::Connect,
            addrs, binding: Some(binding), ..
        } if conn == "g1:1" && rule == "route#0" && addrs == &["127.0.0.1"] && binding.kind == BindingKind::Agent)
    });
    let allowed = position(&events, |event| {
        matches!(event, NetworkEvent::Request {
            conn, seq_in_conn: 1, method, path, decision: RecordDecision::Allow,
            rule: Some(rule), credential: Some(credential), ..
        } if conn == "g1:1" && method == "GET" && path == "/allowed"
            && rule == "route#0.rules[0]" && credential == "test")
    });
    let response = position(
        &events,
        |event| matches!(event, NetworkEvent::Response { conn, seq_in_conn: 1, status: 200, .. } if conn == "g1:1"),
    );
    let refused = position(&events, |event| {
        matches!(event, NetworkEvent::Request {
            seq_in_conn: 2, decision: RecordDecision::Deny, reason: Some(reason), credential: None, path, ..
        } if reason == "route_denied" && path == "/denied")
    });
    let close = position(
        &events,
        |event| matches!(event, NetworkEvent::Close { conn, error: None, .. } if conn == "g1:1"),
    );
    assert!(
        policy < bind && bind < open && open < allowed,
        "{events:#?}"
    );
    assert!(allowed < response && response < refused && refused < close);
    assert!(f.egress.state().relays.is_empty());
    assert!(f.egress.state().open.is_empty());
}

/// The client must name the route's host: another server name gets no
/// certificate, and the connection's `close` says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_server_name_is_refused_and_the_close_says_why() {
    let f = fixture().await;
    let port = f.upstream.addr.port();
    let (_grant, hash, _) = grant(&f.egress, spec(GrantKind::Agent, true)).await;
    let sidecar = RelaySidecar::attach(&f.egress, 1).await;
    let pipe = sidecar
        .open(1, RequestKind::Connect, HOST, port, &hash)
        .await
        .unwrap();
    let ca = f.egress.authority_der().unwrap();
    assert!(tls_client(pipe, &ca, "other.test").await.is_err());
    let close = recorded(
        &f.record,
        |event| matches!(event, NetworkEvent::Close { conn, .. } if conn == "g1:1"),
    )
    .await;
    let NetworkEvent::Close { error, .. } = close else {
        unreachable!()
    };
    assert!(
        error
            .as_deref()
            .is_some_and(|error| error.starts_with("sni_mismatch")),
        "{error:?}"
    );
    assert!(f.upstream.seen().is_empty());
    assert!(!f
        .record
        .events()
        .iter()
        .any(|event| matches!(event, NetworkEvent::Request { .. })));
}

/// On a route's ports the route decides: only `CONNECT` (TLS), only from a
/// process kind it serves, also when `allow` lists the host. An address, or
/// another port, is not the route's and the allowlist decides.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_route_host_is_reached_only_over_tls_by_the_kinds_it_serves() {
    let f = fixture_with(|port| EgressPolicyConfig {
        session_allow: vec![EgressAllowYaml::Host(axocoatl_config::EgressHostYaml {
            host: HOST.into(),
            ports: Some(vec![port, 443]),
        })],
        ..route_config(port)
    })
    .await;
    let port = f.upstream.addr.port();
    let (_agent, agent, _) = grant(&f.egress, spec(GrantKind::Agent, true)).await;
    let (_terminal, terminal, _) = grant(&f.egress, spec(GrantKind::Terminal, true)).await;
    let sidecar = RelaySidecar::attach(&f.egress, 1).await;
    let refusal = |frame: DaemonFrame| match frame {
        DaemonFrame::Deny { status, reason, .. } => (status, reason),
        other => panic!("{other:?}"),
    };
    let plain = sidecar
        .open(1, RequestKind::Http, HOST, port, &agent)
        .await
        .unwrap_err();
    assert_eq!(refusal(plain), (403, "tls_required".to_string()));
    let other_kind = sidecar
        .open(2, RequestKind::Connect, HOST, port, &terminal)
        .await
        .unwrap_err();
    assert_eq!(
        refusal(other_kind),
        (403, "route_not_for_binding".to_string())
    );
    // Port 443 is not the route's: the allow entry admits a tunnel there.
    assert!(matches!(
        sidecar
            .open(3, RequestKind::Connect, HOST, 443, &agent)
            .await
            .unwrap_err(),
        DaemonFrame::Allow { id: 3, .. }
    ));
    // The address is not the route's host; nothing lists it.
    let literal = sidecar
        .open(4, RequestKind::Connect, "127.0.0.1", port, &agent)
        .await
        .unwrap_err();
    assert_eq!(refusal(literal), (403, "not_allowed".to_string()));
    let events = f.record.events();
    for (conn, reason) in [("g1:1", "tls_required"), ("g1:2", "route_not_for_binding")] {
        assert!(
            events.iter().any(|event| matches!(event, NetworkEvent::Open {
                conn: recorded, decision: RecordDecision::Deny, reason: Some(why), rule: Some(rule), ..
            } if recorded == conn && why == reason && rule == "route#0")),
            "{conn}: {events:#?}"
        );
    }
    assert!(f.upstream.seen().is_empty());
}

/// An env file carries the route's placeholders for a process kind the
/// route serves, and points TLS clients at the trust files only when the
/// process's container mounts them. Other kinds get neither.
#[tokio::test]
async fn env_files_point_at_the_trust_files_only_where_they_are_mounted() {
    let f = fixture().await;
    let trust_lines = [
        "SSL_CERT_FILE=/etc/axocoatl/ca/bundle.pem",
        "CURL_CA_BUNDLE=/etc/axocoatl/ca/bundle.pem",
        "REQUESTS_CA_BUNDLE=/etc/axocoatl/ca/bundle.pem",
        "PIP_CERT=/etc/axocoatl/ca/bundle.pem",
        "GIT_SSL_CAINFO=/etc/axocoatl/ca/bundle.pem",
        "CARGO_HTTP_CAINFO=/etc/axocoatl/ca/bundle.pem",
        "NODE_EXTRA_CA_CERTS=/etc/axocoatl/ca/session-ca.pem",
        "DENO_CERT=/etc/axocoatl/ca/session-ca.pem",
    ];
    let placeholder = format!("API_TOKEN=axocoatl-route:{HOST}");
    for (kind, mounted, trust, placeholders) in [
        (GrantKind::Agent, true, true, true),
        (GrantKind::Setup, true, true, true),
        (GrantKind::Agent, false, false, true),
        // The route serves agent and setup only.
        (GrantKind::Terminal, true, false, false),
        (GrantKind::Provisioning, true, false, false),
    ] {
        let (_grant, _, env) = grant(&f.egress, spec(kind, mounted)).await;
        let lines: Vec<&str> = env.lines().collect();
        for line in trust_lines {
            assert_eq!(lines.contains(&line), trust, "{kind:?} {mounted}: {env}");
        }
        assert_eq!(
            lines.contains(&placeholder.as_str()),
            placeholders,
            "{kind:?}: {env}"
        );
        assert!(!env.contains(secret()));
    }
}

/// The trust files exist only while the Session has routes, and hold the
/// Session's authority: alone, and after this computer's roots.
#[tokio::test]
async fn trust_files_hold_the_sessions_authority_while_it_has_routes() {
    let f = fixture().await;
    let files = f.egress.trust_files().unwrap().unwrap();
    let names: Vec<&str> = files.iter().map(|file| file.name.as_str()).collect();
    assert_eq!(names, ["bundle.pem", "session-ca.pem"]);
    let authority = f.egress.authority_pem().unwrap();
    assert_eq!(files[1].contents, authority.as_bytes());
    let bundle = String::from_utf8(files[0].contents.clone()).unwrap();
    assert!(bundle.ends_with(&authority));
    assert!(bundle.matches("BEGIN CERTIFICATE").count() > 1);
    assert!(!bundle.contains("PRIVATE KEY"));
    // The same authority for every container started while it lasts.
    assert!(Arc::ptr_eq(
        &files,
        &f.egress.trust_files().unwrap().unwrap()
    ));

    let plain = fixture_with(|_| EgressPolicyConfig::default()).await;
    assert!(plain.egress.trust_files().unwrap().is_none());
    assert!(plain.egress.authority_pem().is_none());
}

/// `axocoatl network reload` changes routes live: a changed route closes
/// the connections it relayed and is recorded, a removed one leaves its host
/// to the allowlist, and a decision point that gains its first route gets an
/// authority and relays it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_changes_routes_live_and_closes_what_they_relayed() {
    let f = fixture().await;
    let port = f.upstream.addr.port();
    let (_grant, hash, _) = grant(&f.egress, spec(GrantKind::Agent, true)).await;
    let sidecar = RelaySidecar::attach(&f.egress, 1).await;
    let pipe = sidecar
        .open(1, RequestKind::Connect, HOST, port, &hash)
        .await
        .unwrap();
    let before = f.egress.policy(EgressScope::Session).unwrap();

    // Unchanged routes keep the connection.
    let same = f
        .egress
        .reload_config(route_config(port), "human")
        .await
        .unwrap();
    assert!(same.changed.is_empty());

    let widened = EgressPolicyConfig {
        routes: vec![route_yaml(port, ", {methods: [POST], path: /upload}")],
        ..route_config(port)
    };
    let reload = f.egress.reload_config(widened, "human").await.unwrap();
    assert_eq!(reload.changed.len(), 1);
    assert_eq!(reload.changed[0].scope, EgressScope::Session);
    assert_eq!(reload.changed[0].closed, 1);
    for _ in 0..200 {
        if sidecar.revoked() == [1] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(sidecar.revoked(), [1]);
    drop(pipe);
    let after = f.egress.policy(EgressScope::Session).unwrap();
    assert_ne!(after.digest(), before.digest());
    assert!(f.record.events().iter().any(|event| matches!(event,
        NetworkEvent::Policy { source: PolicySource::ConfigReload, rules, .. }
            if rules.iter().any(|rule| rule.contains("route#0: 2 rules")))));

    // Without routes the host is the allowlist's, which does not list it.
    let removed = EgressPolicyConfig {
        routes: Vec::new(),
        ..route_config(port)
    };
    f.egress.reload_config(removed, "human").await.unwrap();
    assert!(f
        .egress
        .policy(EgressScope::Session)
        .unwrap()
        .routes()
        .is_empty());
    assert!(f.egress.trust_files().unwrap().is_none());
    let refused = sidecar
        .open(2, RequestKind::Connect, HOST, port, &hash)
        .await
        .unwrap_err();
    assert!(
        matches!(refused, DaemonFrame::Deny { status: 403, ref reason, .. } if reason == "not_allowed")
    );

    // A decision point opened without routes gains one.
    let plain = fixture_with(|_| EgressPolicyConfig::default()).await;
    let plain_port = plain.upstream.addr.port();
    assert!(plain.egress.authority_pem().is_none());
    plain
        .egress
        .reload_config(route_config(plain_port), "human")
        .await
        .unwrap();
    assert!(plain.egress.trust_files().unwrap().is_some());
    let (_grant, hash, _) = grant(&plain.egress, spec(GrantKind::Agent, true)).await;
    let sidecar = RelaySidecar::attach(&plain.egress, 1).await;
    let pipe = sidecar
        .open(1, RequestKind::Connect, HOST, plain_port, &hash)
        .await
        .unwrap();
    let ca = plain.egress.authority_der().unwrap();
    let tls = tls_client(pipe, &ca, HOST).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    tokio::spawn(connection);
    let (status, _) = send(
        &mut sender,
        "/allowed",
        &format!("{HOST}:{plain_port}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        plain.upstream.seen()[0].authorization,
        [format!("Bearer {}", secret())]
    );
}

/// Ending the credential closes the connections its route relayed, like
/// its tunnels.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ending_the_credential_closes_its_relayed_connections() {
    let f = fixture().await;
    let port = f.upstream.addr.port();
    let (grant, hash, _) = grant(&f.egress, spec(GrantKind::Agent, true)).await;
    let sidecar = RelaySidecar::attach(&f.egress, 1).await;
    let _pipe = sidecar
        .open(1, RequestKind::Connect, HOST, port, &hash)
        .await
        .unwrap();
    drop(grant);
    for _ in 0..200 {
        if sidecar.revoked() == [1] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(sidecar.revoked(), [1]);
    let close = recorded(
        &f.record,
        |event| matches!(event, NetworkEvent::Close { conn, .. } if conn == "g1:1"),
    )
    .await;
    assert!(matches!(
        close,
        NetworkEvent::Close {
            outcome: CloseOutcome::Revoked,
            ..
        }
    ));
}

/// A reload that puts a host under a route closes the plain tunnels already
/// open to it on the route's ports: the route decides there now. Tunnels to
/// other ports stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_route_added_by_reload_closes_tunnels_to_its_host_and_ports() {
    let allow_only = |port: u16| EgressPolicyConfig {
        session_allow: vec![EgressAllowYaml::Host(axocoatl_config::EgressHostYaml {
            host: HOST.into(),
            ports: Some(vec![port, 443]),
        })],
        credentials: credentials(),
        ..EgressPolicyConfig::default()
    };
    let f = fixture_with(allow_only).await;
    let port = f.upstream.addr.port();
    let (_grant, hash, _) = grant(&f.egress, spec(GrantKind::Agent, true)).await;
    let sidecar = RelaySidecar::attach(&f.egress, 1).await;
    for (id, tunnel_port) in [(1, port), (2, 443)] {
        assert!(matches!(
            sidecar
                .open(id, RequestKind::Connect, HOST, tunnel_port, &hash)
                .await
                .unwrap_err(),
            DaemonFrame::Allow { .. }
        ));
    }
    let reload = f
        .egress
        .reload_config(
            EgressPolicyConfig {
                routes: vec![route_yaml(port, "")],
                ..allow_only(port)
            },
            "human",
        )
        .await
        .unwrap();
    assert_eq!(reload.changed[0].closed, 1);
    for _ in 0..200 {
        if !sidecar.revoked().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(sidecar.revoked(), [1]);
    // New connections to the route's port are relayed.
    assert!(sidecar
        .open(3, RequestKind::Connect, HOST, port, &hash)
        .await
        .is_ok());
}
