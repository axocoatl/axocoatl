//! `sandbox.egress.host_ollama` through the decision point and the route
//! broker: a `CONNECT` to `ollama.host.axocoatl.internal:443` is answered
//! with a relay without any name being resolved, TLS ends here with the
//! Session's authority, and each request goes over plain HTTP to a fake
//! Ollama server on a free loopback port, recorded before it leaves. Only
//! Ollama's inference and model-listing endpoints pass, and a body naming a
//! cloud model is refused. The sidecar is the fake from the route tests; no
//! real Ollama is used.

use super::route_tests::{grant, position, recorded, send, spec, tls_client, RelaySidecar};
use super::tests::{FakeRecord, FakeResolver};
use super::*;
use axocoatl_config::egress_host_ollama::HOST_OLLAMA_ROUTE_HOST as OLLAMA;
use axocoatl_exec::egress::protocol::DaemonFrame;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
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
    /// The path and query, as sent.
    pub(crate) target: String,
    pub(crate) host: String,
    /// The body as it arrived.
    pub(crate) body: Bytes,
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
                            let method = request.method().to_string();
                            let path = request.uri().path().to_string();
                            let target = request
                                .uri()
                                .path_and_query()
                                .map(|target| target.as_str().to_string())
                                .unwrap_or_default();
                            let host = request
                                .headers()
                                .get(hyper::header::HOST)
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_string();
                            let body = request
                                .into_body()
                                .collect()
                                .await
                                .map(|body| body.to_bytes())
                                .unwrap_or_default();
                            seen.lock().unwrap().push(Seen {
                                method,
                                path: path.clone(),
                                target,
                                host,
                                body,
                            });
                            let (status, body) = match path.as_str() {
                                "/api/tags" => {
                                    (StatusCode::OK, r#"{"models":[{"name":"fake:latest"}]}"#)
                                }
                                "/api/chat" => (
                                    StatusCode::OK,
                                    r#"{"message":{"role":"assistant","content":"ok"},"done":true}"#,
                                ),
                                _ => (StatusCode::NOT_FOUND, r#"{"error":"not found"}"#),
                            };
                            let mut response = Response::new(Full::new(Bytes::from(body)));
                            *response.status_mut() = status;
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
    // Only Ollama's inference and model listing pass: anything else is
    // refused at the route and never reaches Ollama.
    let (status, body) = send(&mut sender, "/api/missing", OLLAMA, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body.contains("route_denied") && body.contains("ollama pull"),
        "{body}"
    );
    // The broker reads requests only for the route's own host.
    let (status, body) = send(&mut sender, "/api/tags", "localhost:11434", None).await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "{body}");
    drop(sender);
    let _ = connection.await;

    let seen = ollama.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
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
            "{OLLAMA}:443 (host_ollama to 127.0.0.1:{port}: 17 rules, for agent, provider \
             fetches refused)"
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
            && rule == "host_ollama.rules[2]")
    });
    let response = position(&events, |event| {
        matches!(event, NetworkEvent::Response { conn, seq_in_conn: 1, status: 200, .. }
            if conn == "g1:1")
    });
    let missing = position(&events, |event| {
        matches!(event, NetworkEvent::Request {
            conn, seq_in_conn: 2, path, decision: RecordDecision::Deny, reason: Some(reason),
            rule: None, ..
        } if conn == "g1:1" && path == "/api/missing" && reason == "route_denied")
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

/// Ollama's endpoints that reach other hosts or change its models, its
/// account and web endpoints, and a body naming a cloud model (which Ollama
/// 0.20.6 sends to ollama.com) are refused at the route, recorded with the
/// reason and never reach Ollama; inference with a local model and model
/// listing pass, the body byte for byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_route_allows_only_inference_and_listing_and_refuses_cloud_models() {
    let ollama = FakeOllama::start().await;
    let port = ollama.addr.port();
    let f = fixture(host_ollama(port, None)).await;
    let (_grant, hash, _env) = grant(&f.egress, spec(GrantKind::Agent, true)).await;
    let sidecar = RelaySidecar::attach(&f.egress, 1).await;
    let pipe = sidecar
        .open(1, RequestKind::Connect, OLLAMA, 443, &hash)
        .await
        .unwrap();
    let ca = f.egress.authority_der().unwrap();
    let tls = tls_client(pipe, &ca, OLLAMA).await.unwrap();
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake::<_, Full<Bytes>>(TokioIo::new(tls))
            .await
            .unwrap();
    tokio::spawn(connection);
    let mut exchange = |method: &'static str, path: &'static str, body: &'static str| {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(hyper::header::HOST, OLLAMA)
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from_static(body.as_bytes())))
            .unwrap();
        let response = sender.send_request(request);
        async move {
            let response = response.await.unwrap();
            let status = response.status();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            (status, String::from_utf8_lossy(&body).into_owned())
        }
    };

    // A local model's chat passes as it was sent.
    const CHAT: &str = r#"{"model":"qwen3:8b","messages":[{"role":"user","content":"hi"}],"stream":false,"format":{"properties":{"image_url":{"type":"string"}}},"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}]}"#;
    let (status, body) = exchange("POST", "/api/chat", CHAT).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = exchange("GET", "/v1/models", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "Ollama answered it");

    // Refused at the route, by endpoint.
    for (method, path) in [
        ("POST", "/api/pull"),
        ("POST", "/api/push"),
        ("POST", "/api/create"),
        ("POST", "/api/copy"),
        ("DELETE", "/api/delete"),
        ("POST", "/api/blobs/sha256:00"),
        ("POST", "/api/me"),
        ("POST", "/api/signout"),
        ("POST", "/api/experimental/web_search"),
        ("POST", "/api/experimental/web_fetch"),
        ("POST", "/v1/images/generations"),
        ("GET", "/api/chat"),
    ] {
        let (status, body) = exchange(method, path, r#"{"model":"evil.example/ns/m:tag"}"#).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {body}");
        assert!(body.contains("\"route_denied\""), "{body}");
    }
    // Refused by body: a cloud model, on every inference endpoint.
    for (path, body, model) in [
        (
            "/api/chat",
            r#"{"model":"gpt-oss:120b-cloud","messages":[]}"#,
            "gpt-oss:120b-cloud",
        ),
        (
            "/api/generate",
            r#"{"model":"kimi-k2.5:CLOUD","prompt":"hi"}"#,
            "kimi-k2.5:CLOUD",
        ),
        (
            "/api/show",
            r#"{"model":"evil.example/ns/m:cloud"}"#,
            "evil.example/ns/m:cloud",
        ),
        (
            "/api/embed",
            r#"{"model":" e:x-cloud ","input":"hi"}"#,
            "e:x-cloud",
        ),
        (
            "/v1/chat/completions",
            r#"{"model":"kimi-k2.5:cloud","messages":[]}"#,
            "kimi-k2.5:cloud",
        ),
        (
            "/v1/responses",
            r#"{"model":"kimi-k2.5:cloud","input":"hi"}"#,
            "kimi-k2.5:cloud",
        ),
        (
            "/v1/messages",
            r#"{"model":"kimi-k2.5:cloud","max_tokens":1,"messages":[]}"#,
            "kimi-k2.5:cloud",
        ),
    ] {
        let (status, answer) = exchange("POST", path, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {answer}");
        let json: serde_json::Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(json["error"], "provider_fetch_refused", "{answer}");
        assert_eq!(
            json["construct"],
            format!("model {model}: a cloud model, which Ollama runs on ollama.com")
        );
        assert_eq!(json["json_path"], "$.model");
        assert!(
            json["hint"].as_str().unwrap().contains("ollama.com"),
            "{answer}"
        );
    }
    // And a server tool on a compatible endpoint, as on a hosted API.
    let (status, answer) = exchange(
        "POST",
        "/v1/responses",
        r#"{"model":"qwen3:8b","input":"hi","tools":[{"type":"web_search"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");

    // Only the local chat and the model list reached Ollama, the chat byte
    // for byte.
    let seen = ollama.seen();
    let reached: Vec<(&str, &str)> = seen
        .iter()
        .map(|seen| (seen.method.as_str(), seen.path.as_str()))
        .collect();
    assert_eq!(reached, [("POST", "/api/chat"), ("GET", "/v1/models")]);
    assert_eq!(seen[0].body, Bytes::from_static(CHAT.as_bytes()));
    recorded(&f.record, |event| {
        matches!(
            event,
            NetworkEvent::Request {
                seq_in_conn: 22,
                ..
            }
        )
    })
    .await;
    let events = f.record.events();
    let refusals: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|event| match event {
            NetworkEvent::Request {
                path,
                decision: RecordDecision::Deny,
                reason: Some(reason),
                ..
            } => Some((path.as_str(), reason.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(refusals.len(), 20, "{refusals:?}");
    assert!(refusals.contains(&("/api/pull", "route_denied")));
    assert!(refusals.contains(&("/api/experimental/web_search", "route_denied")));
    assert!(refusals.contains(&("/api/chat", "provider_fetch_refused")));
    assert!(refusals.contains(&("/v1/messages", "provider_fetch_refused")));
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
