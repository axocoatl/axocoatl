//! `sandbox.egress.host_ollama` through the decision point and the route
//! broker: a `CONNECT` to `ollama.host.axocoatl.internal:443` is answered
//! with a relay without any name being resolved, TLS ends here with the
//! Session's authority, and each request goes over plain HTTP to a fake
//! Ollama server on a free loopback port, recorded before it leaves. The
//! sidecar is the fake from the route tests; no real Ollama is used.

use super::route_tests::{grant, position, recorded, send, spec, tls_client, RelaySidecar};
use super::tests::{FakeRecord, FakeResolver};
use super::*;
use axocoatl_config::egress_host_ollama::HOST_OLLAMA_ROUTE_HOST as OLLAMA;
use axocoatl_exec::egress::protocol::DaemonFrame;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio::net::TcpListener;

/// One request the fake Ollama server received.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) host: String,
}

/// A plain-HTTP server standing in for Ollama on a free loopback port.
pub(crate) struct FakeOllama {
    pub(crate) addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl FakeOllama {
    pub(crate) async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let by_server = seen.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let seen = by_server.clone();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                        let seen = seen.clone();
                        async move {
                            seen.lock().unwrap().push(Seen {
                                method: request.method().to_string(),
                                path: request.uri().path().to_string(),
                                host: request
                                    .headers()
                                    .get(hyper::header::HOST)
                                    .and_then(|value| value.to_str().ok())
                                    .unwrap_or_default()
                                    .to_string(),
                            });
                            let body = if request.uri().path() == "/api/tags" {
                                r#"{"models":[{"name":"fake:latest"}]}"#
                            } else {
                                r#"{"error":"not found"}"#
                            };
                            let mut response = Response::new(Full::new(Bytes::from(body)));
                            if request.uri().path() != "/api/tags" {
                                *response.status_mut() = StatusCode::NOT_FOUND;
                            }
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tcp), service)
                        .await;
                });
            }
        });
        Self { addr, seen }
    }

    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

struct Fixture {
    egress: Arc<SessionEgress>,
    record: Arc<FakeRecord>,
    resolver: Arc<FakeResolver>,
    _dir: tempfile::TempDir,
}

fn host_ollama(
    port: u16,
    bindings: Option<Vec<axocoatl_config::RouteForYaml>>,
) -> EgressPolicyConfig {
    EgressPolicyConfig {
        host_ollama: Some(HostOllamaRouteYaml { port, bindings }),
        ..EgressPolicyConfig::default()
    }
}

async fn fixture(config: EgressPolicyConfig) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let record = Arc::new(FakeRecord::default());
    // No name resolves: the host route needs none.
    let resolver = FakeResolver::with(&[]);
    let egress = SessionEgress::open_session(
        "ses-host-ollama",
        config,
        record.clone(),
        resolver.clone(),
        Some(SecureDir::open(dir.path()).unwrap()),
        netaddr::classify,
        RouteSettings::default(),
    )
    .await
    .unwrap();
    Fixture {
        egress,
        record,
        resolver,
        _dir: dir,
    }
}

/// The whole path: the env file points Ollama clients at the route, the
/// `CONNECT` is relayed without DNS, TLS ends with the Session's authority,
/// both requests reach the fake server addressed to itself, and the record
/// holds the policy, the connection, each request before it left and each
/// response.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_route_reaches_ollama_on_loopback_and_records_every_request() {
    let ollama = FakeOllama::start().await;
    let port = ollama.addr.port();
    let f = fixture(host_ollama(port, None)).await;
    let (_grant, hash, env) = grant(&f.egress, spec(GrantKind::Agent, true)).await;
    assert!(
        env.contains("OLLAMA_HOST=https://ollama.host.axocoatl.internal:443\n"),
        "{env}"
    );
    assert!(env.contains("SSL_CERT_FILE="), "{env}");
    let sidecar = RelaySidecar::attach(&f.egress, 1).await;
    let pipe = sidecar
        .open(1, RequestKind::Connect, OLLAMA, 443, &hash)
        .await
        .unwrap();
    let ca = f.egress.authority_der().unwrap();
    let tls = tls_client(pipe, &ca, OLLAMA).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    let (status, body) = send(&mut sender, "/api/tags", OLLAMA, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("fake:latest"), "{body}");
    let (status, _) = send(&mut sender, "/api/missing", OLLAMA, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The broker reads requests only for the route's own host.
    let (status, body) = send(&mut sender, "/api/tags", "localhost:11434", None).await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "{body}");
    drop(sender);
    let _ = connection.await;

    let seen = ollama.seen();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(
        (seen[0].method.as_str(), seen[0].path.as_str()),
        ("GET", "/api/tags")
    );
    for request in &seen {
        assert_eq!(request.host, format!("127.0.0.1:{port}"));
    }
    assert!(f.resolver.queries().is_empty(), "nothing was resolved");

    recorded(
        &f.record,
        |event| matches!(event, NetworkEvent::Close { conn, .. } if conn == "g1:1"),
    )
    .await;
    let events = f.record.events();
    for event in &events {
        event.validate().unwrap();
    }
    let policy = position(&events, |event| {
        matches!(event, NetworkEvent::Policy { scope: EgressScope::Session, rules, .. }
        if rules.contains(&format!(
            "{OLLAMA}:443 (host_ollama to 127.0.0.1:{port}: access full, for agent)"
        )))
    });
    let open = position(&events, |event| {
        matches!(event, NetworkEvent::Open {
            conn, decision: RecordDecision::Allow, rule: Some(rule), conn_kind: ConnKind::Connect,
            addrs, host, ..
        } if conn == "g1:1" && rule == "host_ollama" && host == OLLAMA && addrs == &["127.0.0.1"])
    });
    let request = position(&events, |event| {
        matches!(event, NetworkEvent::Request {
            conn, seq_in_conn: 1, method, path, decision: RecordDecision::Allow,
            rule: Some(rule), credential: None, ..
        } if conn == "g1:1" && method == "GET" && path == "/api/tags"
            && rule == "host_ollama.access=full")
    });
    let response = position(&events, |event| {
        matches!(event, NetworkEvent::Response { conn, seq_in_conn: 1, status: 200, .. }
            if conn == "g1:1")
    });
    let missing = position(&events, |event| {
        matches!(event, NetworkEvent::Response { conn, seq_in_conn: 2, status: 404, .. }
            if conn == "g1:1")
    });
    let close = position(
        &events,
        |event| matches!(event, NetworkEvent::Close { conn, .. } if conn == "g1:1"),
    );
    assert!(policy < open && open < request && request < response);
    assert!(response < missing && missing < close);
    assert!(f.egress.state().relays.is_empty());
}

/// Without the setting the name is refused whatever the allowlist says, and
/// nothing resolves it; with it, only the kinds it lists reach it, only
/// over TLS, and only on its port.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_name_is_refused_without_the_setting_and_outside_what_it_allows() {
    let f = fixture(EgressPolicyConfig {
        session_allow: vec![EgressAllowYaml::Host(axocoatl_config::EgressHostYaml {
            host: OLLAMA.into(),
            ports: None,
        })],
        ..EgressPolicyConfig::default()
    })
    .await;
    assert!(f.egress.authority_pem().is_none(), "no route, no authority");
    let (_grant, hash, env) = grant(&f.egress, spec(GrantKind::Agent, true)).await;
    assert!(!env.contains("OLLAMA_HOST"), "{env}");
    let sidecar = RelaySidecar::attach(&f.egress, 1).await;
    let refused = sidecar
        .open(1, RequestKind::Connect, OLLAMA, 443, &hash)
        .await
        .unwrap_err();
    assert!(
        matches!(refused, DaemonFrame::Deny { status: 403, ref reason, .. } if reason == "reserved_host"),
        "{refused:?}"
    );
    assert!(f.resolver.queries().is_empty());
    recorded(&f.record, |event| {
        matches!(event, NetworkEvent::Open { decision: RecordDecision::Deny, reason: Some(reason), host, .. }
            if reason == "reserved_host" && host == OLLAMA)
    })
    .await;

    let ollama = FakeOllama::start().await;
    let f = fixture(host_ollama(ollama.addr.port(), None)).await;
    let (_terminal, terminal_hash, env) = grant(&f.egress, spec(GrantKind::Terminal, true)).await;
    assert!(
        !env.contains("OLLAMA_HOST"),
        "a terminal is not served: {env}"
    );
    let (_agent, agent_hash, _) = grant(&f.egress, spec(GrantKind::Agent, true)).await;
    let sidecar = RelaySidecar::attach(&f.egress, 1).await;
    for (id, kind, port, hash, reason) in [
        (
            1,
            RequestKind::Connect,
            443,
            &terminal_hash,
            "route_not_for_binding",
        ),
        (2, RequestKind::Http, 443, &agent_hash, "tls_required"),
        (3, RequestKind::Connect, 11434, &agent_hash, "reserved_host"),
    ] {
        let refused = sidecar
            .open(id, kind, OLLAMA, port, hash)
            .await
            .unwrap_err();
        assert!(
            matches!(refused, DaemonFrame::Deny { reason: ref got, .. } if got == reason),
            "{id}: {refused:?}"
        );
    }
    assert!(ollama.seen().is_empty());
    assert!(f.resolver.queries().is_empty());
}

/// A reload turns the route on, changes its port and turns it off like any
/// route change: each change is a new recorded policy, and the connections
/// the old route relayed are closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_turns_the_host_route_on_and_off() {
    let ollama = FakeOllama::start().await;
    let port = ollama.addr.port();
    let f = fixture(EgressPolicyConfig::default()).await;
    let reload = f
        .egress
        .reload_config(host_ollama(port, None), "human")
        .await
        .unwrap();
    assert_eq!(reload.changed.len(), 1);
    assert!(f.egress.trust_files().unwrap().is_some());
    let (_grant, hash, env) = grant(&f.egress, spec(GrantKind::Agent, true)).await;
    assert!(env.contains("OLLAMA_HOST="), "{env}");
    let sidecar = RelaySidecar::attach(&f.egress, 1).await;
    let pipe = sidecar
        .open(1, RequestKind::Connect, OLLAMA, 443, &hash)
        .await
        .unwrap();
    let ca = f.egress.authority_der().unwrap();
    let tls = tls_client(pipe, &ca, OLLAMA).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    tokio::spawn(connection);
    let (status, _) = send(&mut sender, "/api/tags", OLLAMA, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        f.egress.policy_config().host_ollama.map(|r| r.port),
        Some(port)
    );

    // The same setting again changes nothing.
    let same = f
        .egress
        .reload_config(host_ollama(port, None), "human")
        .await
        .unwrap();
    assert!(same.changed.is_empty());

    // Another port is another route: the relayed connection closes.
    let moved = f
        .egress
        .reload_config(host_ollama(port.wrapping_add(1).max(1), None), "human")
        .await
        .unwrap();
    assert_eq!(moved.changed.len(), 1);
    assert_eq!(moved.changed[0].closed, 1);
    for _ in 0..200 {
        if sidecar.revoked() == [1] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(sidecar.revoked(), [1]);

    // Off: the name is refused again.
    f.egress
        .reload_config(EgressPolicyConfig::default(), "human")
        .await
        .unwrap();
    assert!(f.egress.policy_config().host_ollama.is_none());
    let refused = sidecar
        .open(2, RequestKind::Connect, OLLAMA, 443, &hash)
        .await
        .unwrap_err();
    assert!(
        matches!(refused, DaemonFrame::Deny { ref reason, .. } if reason == "reserved_host"),
        "{refused:?}"
    );
    assert!(f.record.events().iter().any(|event| matches!(event,
        NetworkEvent::Policy { source: PolicySource::ConfigReload, rules, .. }
            if rules.iter().any(|rule| rule.contains("host_ollama to 127.0.0.1")))));
}

/// The route's policy entry is part of the Session policy's digest only
/// when set: a policy without it keeps the digest it had before 1.3.
#[test]
fn the_host_route_changes_the_digest_only_when_set() {
    let plain = SessionEgress::compile_scope(
        &EgressPolicyConfig::default(),
        EgressScope::Session,
        &[],
        &[],
    )
    .unwrap()
    .0;
    assert_eq!(
        plain.digest(),
        "8d986a287443262e3bf68b27cf6f95a8f76138a3fd09b476b9d1853f76d654e1"
    );
    let (routed, routes) =
        SessionEgress::compile_scope(&host_ollama(11434, None), EgressScope::Session, &[], &[])
            .unwrap();
    assert_ne!(routed.digest(), plain.digest());
    let route = routes.find(OLLAMA, 443).unwrap();
    assert_eq!(route.host_loopback_port(), Some(11434));
    assert_eq!(route.label(), "host_ollama");
    assert!(routes.find(OLLAMA, 11434).is_none());
    assert_eq!(
        route.canonical()["upstream"],
        serde_json::json!({"host_loopback": 11434})
    );
}
